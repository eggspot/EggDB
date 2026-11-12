use super::{QueryResult, Session};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::property_graphs::{
    CREATE_GRAPHS_TABLE, GRAPHS_TABLE, PropertyGraph, dropped_graph, graph_command, graph_table_query, graph_tables,
    parse_definition,
};
use crate::sqlserver_security::literal;
use crate::types::Value;

pub(super) enum GraphText {
    Unchanged,
    Done,
    Rewritten(String),
}

const WORDS_AFTER_A_TABLE: &[&str] = &[
    "where", "join", "inner", "left", "right", "full", "cross", "natural", "on", "using", "order", "group", "having", "limit",
    "offset", "union", "except", "intersect", "window", "fetch", "for", "returning",
];

fn has_alias(after: &str) -> bool {
    let word: String = after
        .trim_start()
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_' || *character == '"')
        .collect::<String>()
        .to_ascii_lowercase();
    !word.is_empty() && !WORDS_AFTER_A_TABLE.contains(&word.as_str())
}

impl Session<'_> {
    fn graph_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn graph_definition(&mut self, name: &str) -> Option<String> {
        self.graph_sql(&format!("SELECT definition FROM {GRAPHS_TABLE} WHERE name = {}", literal(name)))
            .ok()?
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
            .find_map(|row| row.first().and_then(Value::to_text))
    }

    fn property_graph(&mut self, name: &str) -> SqlResult<PropertyGraph> {
        let definition = self
            .graph_definition(name)
            .ok_or_else(|| SqlError::new(SqlState::UNDEFINED_OBJECT, format!("property graph \"{name}\" does not exist")))?;
        Ok(parse_definition(&definition)?.0)
    }

    /// Runs `CREATE` and `DROP PROPERTY GRAPH`, and rewrites each `GRAPH_TABLE (...)` of a
    /// statement into the joins its pattern stands for, before the statement is parsed (D1007).
    pub(super) fn prepare_property_graphs(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<GraphText> {
        if !text.to_ascii_lowercase().contains("graph") {
            return Ok(GraphText::Unchanged);
        }
        let statement = text.trim().trim_end_matches(';').trim_end();
        match graph_command(statement) {
            Some(true) => {
                let (graph, replace) = parse_definition(statement)?;
                self.graph_sql(CREATE_GRAPHS_TABLE)?;
                if self.graph_definition(&graph.name).is_some() {
                    if !replace {
                        return Err(SqlError::new(
                            SqlState::DUPLICATE_OBJECT,
                            format!("property graph \"{}\" already exists", graph.name),
                        ));
                    }
                    self.graph_sql(&format!("DELETE FROM {GRAPHS_TABLE} WHERE name = {}", literal(&graph.name)))?;
                }
                self.graph_sql(&format!(
                    "INSERT INTO {GRAPHS_TABLE} VALUES ({}, {})",
                    literal(&graph.name),
                    literal(statement)
                ))?;
                on_result(QueryResult::command("CREATE PROPERTY GRAPH"));
                return Ok(GraphText::Done);
            }
            Some(false) => {
                let (name, if_exists) = dropped_graph(statement)?;
                if self.graph_definition(&name).is_none() {
                    if !if_exists {
                        return Err(SqlError::new(SqlState::UNDEFINED_OBJECT, format!("property graph \"{name}\" does not exist")));
                    }
                } else {
                    self.graph_sql(&format!("DELETE FROM {GRAPHS_TABLE} WHERE name = {}", literal(&name)))?;
                }
                on_result(QueryResult::command("DROP PROPERTY GRAPH"));
                return Ok(GraphText::Done);
            }
            None => {}
        }
        let Ok(spans) = graph_tables(text) else {
            return Ok(GraphText::Unchanged);
        };
        if spans.is_empty() {
            return Ok(GraphText::Unchanged);
        }
        let mut rewritten = text.to_owned();
        for (range, name, body) in spans.into_iter().rev() {
            let graph = self.property_graph(&name)?;
            let mut query = graph_table_query(&graph, &body)?;
            if !has_alias(&text[range.end..]) {
                query.push_str(" AS graph_table");
            }
            rewritten.replace_range(range, &query);
        }
        Ok(GraphText::Rewritten(rewritten))
    }
}
