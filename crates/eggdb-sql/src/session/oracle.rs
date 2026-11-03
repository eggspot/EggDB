use super::{OutputColumn, QueryResult, Session};
use crate::lexer::swap_uniform_case;
use crate::oracle_dictionary::type_text;
use crate::schema::{ColumnSchema, TableSchema};

pub(super) fn shown_columns(columns: Vec<OutputColumn>) -> Vec<OutputColumn> {
    columns
        .into_iter()
        .map(|column| OutputColumn {
            name: swap_uniform_case(&column.name),
            sql_type: column.sql_type,
        })
        .collect()
}

pub(super) fn after_statement(result: &mut QueryResult) {
    result.notices.clear();
    if let Some(columns) = result.columns.take() {
        result.columns = Some(shown_columns(columns));
    }
}

pub(super) fn identifier(name: &str) -> String {
    let shown = swap_uniform_case(name);
    let plain = shown
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && shown
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'))
        && !crate::parser::is_oracle_reserved(&name.to_ascii_lowercase());
    if plain {
        shown
    } else {
        format!("\"{}\"", shown.replace('"', "\"\""))
    }
}

fn column_text(table: &TableSchema, column: &ColumnSchema) -> String {
    let mut text = format!(
        "{} {}",
        identifier(&column.name),
        type_text(column.sql_type)
    );
    let single_key = table.primary_key.len() == 1 && table.primary_key[0] == column.name;
    if let Some(default) = &column.default {
        text.push_str(&format!(" DEFAULT {default}"));
    }
    if single_key {
        text.push_str(" PRIMARY KEY");
    } else if column.not_null {
        text.push_str(" NOT NULL");
    }
    text
}

/// The `CREATE TABLE` statement that makes `table` in Oracle's SQL.
pub(crate) fn create_text(table: &TableSchema) -> String {
    let mut parts: Vec<String> = table
        .columns
        .iter()
        .map(|column| column_text(table, column))
        .collect();
    if table.primary_key.len() > 1 {
        let key: Vec<String> = table
            .primary_key
            .iter()
            .map(|name| identifier(name))
            .collect();
        parts.push(format!("PRIMARY KEY ({})", key.join(", ")));
    }
    format!(
        "CREATE TABLE {} ({})",
        identifier(&table.name),
        parts.join(", ")
    )
}

impl<'s> Session<'s> {
    pub(super) fn oracle_user(&self) -> String {
        self.settings
            .get("session_authorization")
            .unwrap_or_default()
            .to_uppercase()
    }
}

fn first_text(results: Vec<QueryResult>) -> Option<String> {
    results
        .into_iter()
        .flat_map(|result| result.rows)
        .next()
        .and_then(|row| row.into_iter().next())
        .and_then(|value| match value {
            crate::types::Value::Text(text) => Some(text),
            _ => None,
        })
}

fn plan_table_name(target: &crate::ast::PlanTableTarget) -> String {
    target
        .table
        .as_ref()
        .map_or_else(|| crate::explain::PLAN_TABLE.to_owned(), |name| name.text.clone())
}

fn written(name: &str) -> String {
    name.split(crate::schemas::QUALIFIER)
        .map(crate::schema::quoted)
        .collect::<Vec<_>>()
        .join(".")
}

fn constant_text(expr: &crate::ast::Expr) -> crate::error::SqlResult<Option<String>> {
    use crate::ast::{ExprKind, Literal};
    match &expr.kind {
        ExprKind::Literal(Literal::Null) => Ok(None),
        ExprKind::Literal(Literal::String(text)) => Ok(Some(text.clone())),
        _ => Err(crate::error::SqlError::not_supported(
            "DBMS_XPLAN.DISPLAY with an argument that is not a string literal or NULL",
        )
        .at(expr.position)),
    }
}

impl Session<'_> {
    pub(super) fn explain_into_plan_table(
        &mut self,
        explain: &crate::ast::Explain,
    ) -> crate::error::SqlResult<QueryResult> {
        let target = explain.plan_table.clone().unwrap_or_default();
        let target = &target;
        let settings = std::sync::Arc::clone(&self.settings);
        let binding = crate::name_resolution::Binding {
            settings: &settings,
            walk_expressions: true,
            target_privilege: Some(crate::roles::target_privilege(&explain.statement)),
        };
        let mut planned = (*explain.statement).clone();
        let query = self.read(|reader, tables| {
            binding.statement(reader, tables, &mut planned)?;
            crate::execute::explain(reader, tables, &planned)
        })?;
        let statement = crate::explain::PlannedStatement::of(&explain.statement)
            .unwrap_or(crate::explain::PlannedStatement::Select);
        let rows = crate::explain::plan_rows(&query, statement);
        let table = plan_table_name(target);
        match &target.table {
            Some(name) => {
                self.read(|reader, tables| tables.require(reader, name).map(drop))?;
            }
            None => {
                let exists =
                    self.read(|reader, tables| Ok(tables.table(reader, &table)?.is_some()))?;
                if !exists {
                    self.run_quietly(&crate::explain::create_plan_table(&table))?;
                }
            }
        }
        let plan_id = first_text(self.run_quietly(&format!(
            "SELECT TO_CHAR(NVL(MAX(\"plan_id\"), 0) + 1) FROM {}",
            written(&table)
        ))?)
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(1);
        for row in &rows {
            self.run_quietly(&crate::explain::insert_plan_row(
                &table,
                row,
                plan_id,
                target.statement_id.as_deref(),
            ))?;
        }
        Ok(QueryResult::command("EXPLAIN PLAN"))
    }

    pub(super) fn show_plan_displays(
        &mut self,
        statement: &mut crate::ast::Statement,
    ) -> crate::error::SqlResult<()> {
        use crate::ast::{GeneratedTable, SetExpr, Statement};
        let select = match statement {
            Statement::Select(select) => select,
            Statement::Query(query) => match &mut query.body {
                SetExpr::Select(select) => select.as_mut(),
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        let Some(from) = &mut select.from else {
            return Ok(());
        };
        let reference = &mut from.first;
        let Some(GeneratedTable::Function(function)) = reference.generated.as_deref() else {
            return Ok(());
        };
        if function.name != crate::explain::PLAN_DISPLAY_FUNCTION {
            return Ok(());
        }
        let position = function.position;
        let mut arguments = function.arguments.iter().map(constant_text);
        let table = arguments.next().transpose()?.flatten();
        let statement_id = arguments.next().transpose()?.flatten();
        for argument in arguments {
            argument?;
        }
        let lines = self.plan_display_lines(table.as_deref(), statement_id.as_deref())?;
        reference.generated = Some(Box::new(GeneratedTable::Values(crate::ast::ValuesList {
            rows: lines
                .into_iter()
                .map(|line| {
                    vec![crate::ast::Expr {
                        kind: crate::ast::ExprKind::Literal(crate::ast::Literal::String(line)),
                        position,
                    }]
                })
                .collect(),
            column_names: vec![crate::ast::Name {
                text: "plan_table_output".to_owned(),
                position,
            }],
            position,
        })));
        Ok(())
    }

    fn plan_display_lines(
        &mut self,
        table: Option<&str>,
        statement_id: Option<&str>,
    ) -> crate::error::SqlResult<Vec<String>> {
        let table = table.map_or_else(
            || crate::explain::PLAN_TABLE.to_owned(),
            |table| table.to_ascii_lowercase(),
        );
        let exists = self.read(|reader, tables| Ok(tables.table(reader, &table)?.is_some()))?;
        if !exists {
            return Ok(crate::explain::missing_plan_lines(statement_id));
        }
        let chosen = statement_id.map_or_else(String::new, |id| {
            format!(" WHERE \"statement_id\" = '{}'", id.replace('\'', "''"))
        });
        let plan_id = first_text(self.run_quietly(&format!(
            "SELECT TO_CHAR(MAX(\"plan_id\")) FROM {}{chosen}",
            written(&table)
        ))?);
        let Some(plan_id) = plan_id.and_then(|text| text.trim().parse::<u64>().ok()) else {
            return Ok(crate::explain::missing_plan_lines(statement_id));
        };
        let results = self.run_quietly(&format!(
            "SELECT TO_CHAR(\"id\"), TO_CHAR(\"depth\"), \"operation\", \"options\", \"object_name\" FROM {} WHERE \"plan_id\" = {plan_id} ORDER BY \"id\"",
            written(&table)
        ))?;
        let text = |value: Option<&crate::types::Value>| match value {
            Some(crate::types::Value::Text(text)) => Some(text.clone()),
            _ => None,
        };
        let number = |value: Option<&crate::types::Value>| {
            text(value).and_then(|text| text.trim().parse::<u64>().ok())
        };
        let rows: Vec<crate::explain::DisplayedRow> = results
            .into_iter()
            .flat_map(|result| result.rows)
            .take(crate::explain::MOST_DISPLAYED_ROWS)
            .map(|row| crate::explain::DisplayedRow {
                id: number(row.first()).unwrap_or(0),
                depth: number(row.get(1)).unwrap_or(0),
                operation: text(row.get(2)).unwrap_or_default(),
                options: text(row.get(3)),
                name: text(row.get(4)),
            })
            .collect();
        if rows.is_empty() {
            return Ok(crate::explain::missing_plan_lines(statement_id));
        }
        Ok(crate::explain::display_lines(&rows))
    }
}
