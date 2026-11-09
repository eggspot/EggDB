use super::estimates::{self, ROW_COST, SEEK_COST};
use super::expression_text::{ExpressionText, Spelling};
use super::mysql_tree::unconsumed_filter;
use super::{Planned, PlannedStatement, PlannedWrite, key_name, mysql_access};
use crate::ast::Expr;
use crate::dialect::Dialect;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::join::JoinMethod;
use crate::plan::{KeyChoice, Use};

enum PlanJson {
    Object(Vec<(&'static str, PlanJson)>),
    Array(Vec<PlanJson>),
    Text(String),
    Number(u64),
    Flag(bool),
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write(value: &PlanJson, level: usize, out: &mut String) {
    match value {
        PlanJson::Object(members) if members.is_empty() => out.push_str("{}"),
        PlanJson::Object(members) => {
            out.push_str("{\n");
            for (at, (name, member)) in members.iter().enumerate() {
                indent(out, level + 1);
                crate::json::write_string(name, out);
                out.push_str(": ");
                write(member, level + 1, out);
                if at + 1 < members.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, level);
            out.push('}');
        }
        PlanJson::Array(items) if items.is_empty() => out.push_str("[]"),
        PlanJson::Array(items) => {
            out.push_str("[\n");
            for (at, item) in items.iter().enumerate() {
                indent(out, level + 1);
                write(item, level + 1, out);
                if at + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, level);
            out.push(']');
        }
        PlanJson::Text(text) => crate::json::write_string(text, out),
        PlanJson::Number(number) => out.push_str(&number.to_string()),
        PlanJson::Flag(flag) => out.push_str(if *flag { "true" } else { "false" }),
    }
}

fn text(value: &str) -> PlanJson {
    PlanJson::Text(value.to_owned())
}

fn money(value: f64) -> PlanJson {
    PlanJson::Text(format!("{:.2}", value.max(0.0)))
}

fn whole(value: f64) -> PlanJson {
    PlanJson::Number(if value.is_finite() && value > 0.0 {
        value.round() as u64
    } else {
        0
    })
}

fn condition_text(query: &ExplainedQuery, expr: &Expr) -> String {
    let qualifier = match query.sources.as_slice() {
        [only] => Some(only.label.as_str()),
        _ => None,
    };
    ExpressionText {
        spelling: Spelling::MySql,
        qualifier,
        bare_columns: false,
        subplans: &query.subplans,
        select_numbers: &[],
    }
    .text(expr)
}

fn key_parts(source: &ExplainedSource, key: KeyChoice) -> Vec<String> {
    let table = &source.table;
    let columns: Vec<usize> = match (source.join, source.access.used, key) {
        (Some(JoinMethod::Index(_)), _, KeyChoice::Index(position)) => table
            .indexes
            .get(position)
            .and_then(|index| index.columns.first().copied())
            .into_iter()
            .collect(),
        (None, Use::Point | Use::Equal | Use::Range, _) => {
            let mut columns = Vec::new();
            for &(column, _) in &source.access.conditions {
                if !columns.contains(&column) {
                    columns.push(column);
                }
            }
            columns
        }
        (_, _, KeyChoice::Primary) => table.primary_key.clone(),
        (_, _, KeyChoice::Index(position)) => table
            .indexes
            .get(position)
            .map(|index| index.columns.clone())
            .unwrap_or_default(),
    };
    columns
        .into_iter()
        .filter_map(|column| table.columns.get(column))
        .map(|column| column.name.clone())
        .collect()
}

struct Walk<'p> {
    planned: &'p Planned,
    costed: bool,
    prefix_rows: f64,
    prefix_cost: f64,
}

impl Walk<'_> {
    fn written_by(&self, position: usize) -> Option<&'static str> {
        let write: &PlannedWrite = self.planned.write.as_ref()?;
        if !write.targets.contains(&position) {
            return None;
        }
        Some(match write.statement {
            PlannedStatement::Delete => "delete",
            PlannedStatement::Update | PlannedStatement::Select => "update",
        })
    }

    fn examined(&self, source: &ExplainedSource) -> f64 {
        let table_rows = &self.planned.table_rows;
        match source.join {
            None => estimates::scanned_rows(source, table_rows),
            Some(JoinMethod::PrimaryKey) => {
                estimates::lookup_rows(source, KeyChoice::Primary, table_rows)
            }
            Some(JoinMethod::Index(position)) => {
                estimates::lookup_rows(source, KeyChoice::Index(position), table_rows)
            }
            Some(JoinMethod::Hash | JoinMethod::Loop) => table_rows.of(&source.table),
        }
    }

    fn table(
        &mut self,
        query: &ExplainedQuery,
        position: usize,
        source: &ExplainedSource,
        filter: Option<&Expr>,
    ) -> PlanJson {
        let (access_type, key) = mysql_access(source);
        let examined = self.examined(source);
        let mut fraction = 1.0;
        let mut conditions: Vec<String> = Vec::new();
        let join_condition = query
            .join_conditions
            .get(position)
            .and_then(Option::as_ref)
            .filter(|_| matches!(source.join, Some(JoinMethod::Hash | JoinMethod::Loop)));
        if let Some(condition) = join_condition {
            fraction *= estimates::selectivity(condition);
            conditions.push(condition_text(query, condition));
        }
        if let Some(filter) = filter {
            let base = if query.sources.len() == 1 {
                self.planned.table_rows.of(&source.table)
            } else {
                examined
            };
            let passing = (base * estimates::selectivity(filter)).min(examined);
            if examined > 0.0 {
                fraction *= passing / examined;
            }
            conditions.push(condition_text(query, filter));
        }
        let produced = self.prefix_rows * examined * fraction;
        let read_all = if source.join == Some(JoinMethod::Hash) {
            SEEK_COST + ROW_COST * examined + ROW_COST * (self.prefix_rows + examined)
        } else {
            self.prefix_rows * (SEEK_COST + ROW_COST * examined)
        };
        let eval_cost = ROW_COST * produced;
        self.prefix_cost += read_all;
        self.prefix_rows = produced;
        let mut members: Vec<(&'static str, PlanJson)> = Vec::new();
        if let Some(verb) = self.written_by(position) {
            members.push((verb, PlanJson::Flag(true)));
        }
        members.push(("table_name", text(&source.label)));
        members.push(("access_type", text(access_type)));
        if let Some(key) = key {
            let name = key_name(&source.table, key, Dialect::MySql);
            let parts = key_parts(source, key);
            members.push(("possible_keys", PlanJson::Array(vec![text(&name)])));
            members.push(("key", text(&name)));
            members.push((
                "used_key_parts",
                PlanJson::Array(parts.iter().map(|part| text(part)).collect()),
            ));
            if source.join.is_none() && matches!(source.access.used, Use::Point | Use::Equal) {
                members.push((
                    "ref",
                    PlanJson::Array(parts.iter().map(|_| text("const")).collect()),
                ));
            }
        }
        members.push(("rows_examined_per_scan", whole(examined)));
        if self.costed {
            members.push(("rows_produced_per_join", whole(produced)));
        }
        members.push((
            "filtered",
            PlanJson::Text(format!("{:.2}", (fraction * 100.0).clamp(0.0, 100.0))),
        ));
        if source.join == Some(JoinMethod::Hash) {
            members.push(("using_join_buffer", text("hash join")));
        }
        if source.join.is_none() && source.access.backward() {
            members.push(("backward_index_scan", PlanJson::Flag(true)));
        }
        if self.costed {
            members.push((
                "cost_info",
                PlanJson::Object(vec![
                    ("read_cost", money(read_all - eval_cost)),
                    ("eval_cost", money(eval_cost)),
                    ("prefix_cost", money(self.prefix_cost)),
                ]),
            ));
        }
        match conditions.as_slice() {
            [] => {}
            [only] => members.push(("attached_condition", text(only))),
            many => members.push((
                "attached_condition",
                PlanJson::Text(format!("({})", many.join(" and "))),
            )),
        }
        PlanJson::Object(members)
    }
}

fn union_block(planned: &Planned) -> PlanJson {
    let query = &planned.query;
    let specifications = query
        .sources
        .iter()
        .enumerate()
        .map(|(at, source)| {
            let mut walk = Walk {
                planned,
                costed: true,
                prefix_rows: 1.0,
                prefix_cost: 0.0,
            };
            let table = walk.table(query, at, source, None);
            PlanJson::Object(vec![
                ("dependent", PlanJson::Flag(false)),
                ("cacheable", PlanJson::Flag(true)),
                (
                    "query_block",
                    PlanJson::Object(vec![
                        ("select_id", PlanJson::Number(at as u64 + 1)),
                        (
                            "cost_info",
                            PlanJson::Object(vec![("query_cost", money(walk.prefix_cost))]),
                        ),
                        ("table", table),
                    ]),
                ),
            ])
        })
        .collect();
    PlanJson::Object(vec![(
        "union_result",
        PlanJson::Object(vec![
            ("using_temporary_table", PlanJson::Flag(false)),
            ("query_specifications", PlanJson::Array(specifications)),
        ]),
    )])
}

fn query_block(planned: &Planned) -> PlanJson {
    let query = &planned.query;
    if query.appended {
        return union_block(planned);
    }
    let mut members: Vec<(&'static str, PlanJson)> = vec![("select_id", PlanJson::Number(1))];
    if query.sources.is_empty() {
        members.push(("message", text("No tables used")));
        return PlanJson::Object(members);
    }
    let mut walk = Walk {
        planned,
        costed: planned.write.as_ref().is_none_or(|write| write.joined),
        prefix_rows: 1.0,
        prefix_cost: 0.0,
    };
    let filter = unconsumed_filter(query);
    let last = query.sources.len() - 1;
    let tables: Vec<PlanJson> = query
        .sources
        .iter()
        .enumerate()
        .map(|(position, source)| {
            let filter = filter.filter(|_| position == last);
            walk.table(query, position, source, filter)
        })
        .collect();
    let mut inner: Vec<(&'static str, PlanJson)> = match <[PlanJson; 1]>::try_from(tables) {
        Ok([only]) => vec![("table", only)],
        Err(tables) => vec![(
            "nested_loop",
            PlanJson::Array(
                tables
                    .into_iter()
                    .map(|table| PlanJson::Object(vec![("table", table)]))
                    .collect(),
            ),
        )],
    };
    if query
        .grouping
        .as_ref()
        .is_some_and(|grouping| grouping.keyed)
    {
        let mut grouping = vec![
            ("using_temporary_table", PlanJson::Flag(true)),
            ("using_filesort", PlanJson::Flag(false)),
        ];
        grouping.extend(inner);
        inner = vec![("grouping_operation", PlanJson::Object(grouping))];
    }
    let mut query_cost = walk.prefix_cost;
    if !query.order_by.is_empty() {
        let mut ordering = vec![("using_filesort", PlanJson::Flag(query.sorted))];
        if query.sorted {
            let sort_cost = estimates::sort_cost(walk.prefix_rows);
            query_cost += sort_cost;
            ordering.push((
                "cost_info",
                PlanJson::Object(vec![("sort_cost", money(sort_cost))]),
            ));
        }
        ordering.extend(inner);
        inner = vec![("ordering_operation", PlanJson::Object(ordering))];
    }
    if walk.costed {
        members.push((
            "cost_info",
            PlanJson::Object(vec![("query_cost", money(query_cost))]),
        ));
    }
    members.extend(inner);
    PlanJson::Object(members)
}

/// MySQL 8's `EXPLAIN FORMAT=JSON` document (format version 1): a `query_block` with its
/// `select_id`, `cost_info`, the `ordering_operation` and `grouping_operation` around the
/// `table` (or the `nested_loop` of tables) it reads, each table with its `access_type`,
/// `key`, `rows_examined_per_scan`, `filtered` and `attached_condition`, from EggDB's plan and
/// estimates. Pretty-printed with two spaces per level, as MySQL prints it.
pub(crate) fn json_text(planned: &Planned) -> String {
    let document = PlanJson::Object(vec![("query_block", query_block(planned))]);
    let mut out = String::new();
    write(&document, 0, &mut out);
    out
}
