use super::{PlannedStatement, Step, key_name, steps_without_hashes};
use crate::dialect::Dialect;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::lexer::swap_uniform_case;
use crate::plan::{KeyChoice, Use};
use crate::session::{OutputColumn, QueryResult};
use crate::types::{SqlType, Value};

/// The table `EXPLAIN PLAN` writes to when it names none, created on demand.
pub(crate) const PLAN_TABLE: &str = "plan_table";

/// The name `TABLE(DBMS_XPLAN.DISPLAY)` parses to, replaced by the formatted plan's lines
/// before the query runs.
pub(crate) const PLAN_DISPLAY_FUNCTION: &str = "dbms_xplan.display";

const PLAN_TABLE_COLUMNS: [(&str, &str); 36] = [
    ("statement_id", "VARCHAR2(30)"),
    ("plan_id", "NUMBER"),
    ("timestamp", "DATE"),
    ("remarks", "VARCHAR2(4000)"),
    ("operation", "VARCHAR2(30)"),
    ("options", "VARCHAR2(255)"),
    ("object_node", "VARCHAR2(128)"),
    ("object_owner", "VARCHAR2(128)"),
    ("object_name", "VARCHAR2(128)"),
    ("object_alias", "VARCHAR2(261)"),
    ("object_instance", "NUMBER(38)"),
    ("object_type", "VARCHAR2(30)"),
    ("optimizer", "VARCHAR2(255)"),
    ("search_columns", "NUMBER"),
    ("id", "NUMBER(38)"),
    ("parent_id", "NUMBER(38)"),
    ("depth", "NUMBER(38)"),
    ("position", "NUMBER(38)"),
    ("cost", "NUMBER(38)"),
    ("cardinality", "NUMBER(38)"),
    ("bytes", "NUMBER(38)"),
    ("other_tag", "VARCHAR2(255)"),
    ("partition_start", "VARCHAR2(255)"),
    ("partition_stop", "VARCHAR2(255)"),
    ("partition_id", "NUMBER(38)"),
    ("other", "LONG"),
    ("other_xml", "CLOB"),
    ("distribution", "VARCHAR2(30)"),
    ("cpu_cost", "NUMBER(38)"),
    ("io_cost", "NUMBER(38)"),
    ("temp_space", "NUMBER(38)"),
    ("access_predicates", "VARCHAR2(4000)"),
    ("filter_predicates", "VARCHAR2(4000)"),
    ("projection", "VARCHAR2(4000)"),
    ("time", "NUMBER(38)"),
    ("qblock_name", "VARCHAR2(128)"),
];

const WRITTEN_COLUMNS: [&str; 14] = [
    "statement_id",
    "plan_id",
    "timestamp",
    "operation",
    "options",
    "object_owner",
    "object_name",
    "object_alias",
    "object_type",
    "optimizer",
    "id",
    "parent_id",
    "depth",
    "position",
];

/// One row `EXPLAIN PLAN` writes to the plan table, in Oracle's columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanRow {
    pub(crate) id: usize,
    pub(crate) parent_id: Option<usize>,
    pub(crate) depth: usize,
    pub(crate) position: Option<usize>,
    pub(crate) operation: &'static str,
    pub(crate) options: Option<&'static str>,
    pub(crate) object: Option<PlanObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanObject {
    pub(crate) owner: Option<String>,
    pub(crate) name: String,
    pub(crate) alias: String,
    pub(crate) kind: &'static str,
}

struct Operation {
    depth: usize,
    operation: &'static str,
    options: Option<&'static str>,
    object: Option<PlanObject>,
}

impl PlannedStatement {
    fn oracle_operation(self) -> &'static str {
        match self {
            Self::Select => "SELECT STATEMENT",
            Self::Update => "UPDATE STATEMENT",
            Self::Delete => "DELETE STATEMENT",
        }
    }

    fn oracle_change(self) -> Option<&'static str> {
        match self {
            Self::Select => None,
            Self::Update => Some("UPDATE"),
            Self::Delete => Some("DELETE"),
        }
    }

    fn query_block(self) -> &'static str {
        match self {
            Self::Select => "SEL$1",
            Self::Update => "UPD$1",
            Self::Delete => "DEL$1",
        }
    }
}

fn object(name: &str, label: &str, block: &str, kind: &'static str) -> PlanObject {
    let (owner, name) = match name.split_once(crate::schemas::QUALIFIER) {
        Some((owner, name)) => (Some(swap_uniform_case(owner)), name),
        None => (None, name),
    };
    let label = label
        .rsplit(crate::schemas::QUALIFIER)
        .next()
        .unwrap_or(label);
    PlanObject {
        owner,
        name: swap_uniform_case(name),
        alias: format!("{}@{block}", swap_uniform_case(label)),
        kind,
    }
}

fn table_object(source: &ExplainedSource, block: &str) -> PlanObject {
    object(&source.table.name, &source.label, block, "TABLE")
}

fn index_object(source: &ExplainedSource, key: KeyChoice, block: &str) -> PlanObject {
    let kind = match key {
        KeyChoice::Primary => "INDEX (UNIQUE)",
        KeyChoice::Index(_) => "INDEX",
    };
    let index = key_name(&source.table, key, Dialect::Oracle);
    let mut indexed = object(&index, &source.label, block, kind);
    if let Some((owner, _)) = source.table.name.split_once(crate::schemas::QUALIFIER) {
        indexed.owner = Some(swap_uniform_case(owner));
    }
    indexed
}

fn full_scan(depth: usize, source: &ExplainedSource, block: &str) -> Operation {
    Operation {
        depth,
        operation: "TABLE ACCESS",
        options: Some("FULL"),
        object: Some(table_object(source, block)),
    }
}

fn keyed(
    depth: usize,
    source: &ExplainedSource,
    key: KeyChoice,
    scan: &'static str,
    block: &str,
) -> [Operation; 2] {
    [
        Operation {
            depth,
            operation: "TABLE ACCESS",
            options: Some("BY INDEX ROWID"),
            object: Some(table_object(source, block)),
        },
        Operation {
            depth: depth + 1,
            operation: "INDEX",
            options: Some(scan),
            object: Some(index_object(source, key, block)),
        },
    ]
}

fn operations(query: &ExplainedQuery, statement: PlannedStatement) -> Vec<Operation> {
    let block = statement.query_block();
    let mut out = vec![Operation {
        depth: 0,
        operation: statement.oracle_operation(),
        options: None,
        object: None,
    }];
    let mut base = 1;
    if let Some(change) = statement.oracle_change() {
        out.push(Operation {
            depth: 1,
            operation: change,
            options: None,
            object: query
                .sources
                .first()
                .map(|source| table_object(source, block)),
        });
        base = 2;
    }
    let plain = |depth, operation, options| Operation {
        depth,
        operation,
        options,
        object: None,
    };
    for (level, step) in steps_without_hashes(query) {
        let depth = base + level;
        match step {
            Step::Result => out.push(plain(depth, "FAST DUAL", None)),
            Step::Append => out.push(plain(depth, "UNION-ALL", None)),
            Step::Sort => out.push(plain(depth, "SORT", Some("ORDER BY"))),
            Step::Limit => out.push(plain(depth, "COUNT", Some("STOPKEY"))),
            Step::NestedLoop => out.push(plain(depth, "NESTED LOOPS", None)),
            Step::HashJoin | Step::Hash => out.push(plain(depth, "HASH JOIN", None)),
            Step::FullScan(source) => out.push(full_scan(depth, source, block)),
            Step::Scan(source) => match source.access.key() {
                None => out.push(full_scan(depth, source, block)),
                Some(key) => {
                    let scan = if source.access.used == Use::Point {
                        "UNIQUE SCAN"
                    } else if source.access.backward() {
                        "RANGE SCAN DESCENDING"
                    } else {
                        "RANGE SCAN"
                    };
                    out.extend(keyed(depth, source, key, scan, block));
                }
            },
            Step::KeyLookup(source, key) => {
                let scan = match key {
                    KeyChoice::Primary => "UNIQUE SCAN",
                    KeyChoice::Index(_) => "RANGE SCAN",
                };
                out.extend(keyed(depth, source, key, scan, block));
            }
        }
    }
    out
}

/// The rows `EXPLAIN PLAN` writes for `query`: one per operation in Oracle's names, numbered
/// from 0 in pre-order with each row's parent, depth and place among its siblings.
pub(crate) fn plan_rows(query: &ExplainedQuery, statement: PlannedStatement) -> Vec<PlanRow> {
    let operations = operations(query, statement);
    let mut children = vec![0_usize; operations.len()];
    let mut open: Vec<usize> = Vec::new();
    let mut rows = Vec::with_capacity(operations.len());
    for (id, operation) in operations.into_iter().enumerate() {
        open.truncate(operation.depth);
        let parent_id = open.last().copied();
        let position = parent_id.and_then(|parent| {
            let count = children.get_mut(parent)?;
            *count += 1;
            Some(*count)
        });
        open.push(id);
        rows.push(PlanRow {
            id,
            parent_id,
            depth: operation.depth,
            position,
            operation: operation.operation,
            options: operation.options,
            object: operation.object,
        });
    }
    rows
}

/// `name`, a plan table's catalog name, written as Oracle SQL names it.
pub(crate) fn written_plan_table(name: &str) -> String {
    name.split(crate::schemas::QUALIFIER)
        .map(crate::session::oracle_identifier)
        .collect::<Vec<_>>()
        .join(".")
}

fn text_literal(text: Option<&str>) -> String {
    match text {
        Some(text) => format!("'{}'", text.replace('\'', "''")),
        None => "NULL".to_owned(),
    }
}

fn number_literal(number: Option<usize>) -> String {
    number.map_or_else(|| "NULL".to_owned(), |number| number.to_string())
}

/// `CREATE TABLE` for a plan table named `table`, with Oracle's columns.
pub(crate) fn create_plan_table(table: &str) -> String {
    let columns: Vec<String> = PLAN_TABLE_COLUMNS
        .iter()
        .map(|(name, sql_type)| format!("{} {sql_type}", crate::session::oracle_identifier(name)))
        .collect();
    format!(
        "CREATE TABLE {} ({})",
        written_plan_table(table),
        columns.join(", ")
    )
}

/// `INSERT` of one plan row into `table` under `plan_id` and `statement_id`.
pub(crate) fn insert_plan_row(
    table: &str,
    row: &PlanRow,
    plan_id: u64,
    statement_id: Option<&str>,
) -> String {
    let columns: Vec<String> = WRITTEN_COLUMNS
        .iter()
        .map(|name| crate::session::oracle_identifier(name))
        .collect();
    let object = row.object.as_ref();
    let values = [
        text_literal(statement_id),
        plan_id.to_string(),
        "SYSDATE".to_owned(),
        text_literal(Some(row.operation)),
        text_literal(row.options),
        text_literal(object.and_then(|object| object.owner.as_deref())),
        text_literal(object.map(|object| object.name.as_str())),
        text_literal(object.map(|object| object.alias.as_str())),
        text_literal(object.map(|object| object.kind)),
        text_literal((row.id == 0).then_some("ALL_ROWS")),
        row.id.to_string(),
        number_literal(row.parent_id),
        row.depth.to_string(),
        number_literal(row.position),
    ];
    format!(
        "INSERT INTO {} ({}) VALUES ({})",
        written_plan_table(table),
        columns.join(", "),
        values.join(", ")
    )
}

/// A plan row as `DBMS_XPLAN.DISPLAY` reads it back from the plan table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisplayedRow {
    pub(crate) id: u64,
    pub(crate) depth: u64,
    pub(crate) operation: String,
    pub(crate) options: Option<String>,
    pub(crate) name: Option<String>,
}

fn plan_hash(rows: &[DisplayedRow]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for row in rows {
        for byte in row
            .operation
            .bytes()
            .chain(row.options.iter().flat_map(|options| options.bytes()))
            .chain(row.name.iter().flat_map(|name| name.bytes()))
            .chain(std::iter::once(b'|'))
        {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
    }
    hash
}

const MOST_SHOWN_DEPTH: u64 = 64;

/// The most plan rows `DBMS_XPLAN.DISPLAY` reads back for one plan.
pub(crate) const MOST_DISPLAYED_ROWS: usize = 10_000;

/// The lines `DBMS_XPLAN.DISPLAY` returns for one plan's rows in `id` order: the plan hash,
/// then the `Id`, `Operation` and `Name` table with each operation indented by its depth,
/// then a note that EggDB gives no estimates.
pub(crate) fn display_lines(rows: &[DisplayedRow]) -> Vec<String> {
    let operations: Vec<String> = rows
        .iter()
        .map(|row| {
            let indent = " ".repeat(usize::try_from(row.depth.min(MOST_SHOWN_DEPTH)).unwrap_or(0));
            match &row.options {
                Some(options) => format!("{indent}{} {options}", row.operation),
                None => format!("{indent}{}", row.operation),
            }
        })
        .collect();
    let operation_width = operations
        .iter()
        .map(|operation| operation.chars().count())
        .chain(std::iter::once("Operation".len()))
        .max()
        .unwrap_or(0);
    let name_width = rows
        .iter()
        .filter_map(|row| row.name.as_ref().map(|name| name.chars().count()))
        .chain(std::iter::once("Name".len()))
        .max()
        .unwrap_or(0);
    let line = |id: &str, operation: &str, name: &str| {
        format!("|{id:>4} | {operation:<operation_width$}| {name:<name_width$} |")
    };
    let header = line("Id ", "Operation", "Name");
    let rule = "-".repeat(header.chars().count());
    let mut lines = vec![
        format!("Plan hash value: {}", plan_hash(rows)),
        String::new(),
        rule.clone(),
        header,
        rule.clone(),
    ];
    for (row, operation) in rows.iter().zip(&operations) {
        lines.push(line(
            &row.id.to_string(),
            operation,
            row.name.as_deref().unwrap_or(""),
        ));
    }
    lines.push(rule);
    lines.push(String::new());
    lines.push("Note".to_owned());
    lines.push("-----".to_owned());
    lines.push("   - EggDB gives no cost, row or time estimates in a plan".to_owned());
    lines
}

/// The rows `DBMS_XPLAN.DISPLAY` reads back for `rows` once they are in a plan table.
pub(crate) fn displayed_rows(rows: &[PlanRow]) -> Vec<DisplayedRow> {
    rows.iter()
        .map(|row| DisplayedRow {
            id: u64::try_from(row.id).unwrap_or(u64::MAX),
            depth: u64::try_from(row.depth).unwrap_or(u64::MAX),
            operation: row.operation.to_owned(),
            options: row.options.map(str::to_owned),
            name: row.object.as_ref().map(|object| object.name.clone()),
        })
        .collect()
}

/// The answer `SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)` gives right after `EXPLAIN PLAN`
/// wrote `rows`: one `PLAN_TABLE_OUTPUT` row per line.
pub(crate) fn display_result(rows: &[PlanRow]) -> QueryResult {
    let lines = display_lines(&displayed_rows(rows));
    QueryResult::rows(
        format!("SELECT {}", lines.len()),
        vec![OutputColumn {
            name: "plan_table_output".to_owned(),
            sql_type: Some(SqlType::Text),
        }],
        lines
            .into_iter()
            .map(|line| vec![Value::Text(line)])
            .collect(),
    )
}

/// The line `DBMS_XPLAN.DISPLAY` returns when the plan table holds no plan to show.
pub(crate) fn missing_plan_lines(statement_id: Option<&str>) -> Vec<String> {
    vec![match statement_id {
        Some(id) => format!("Error: cannot fetch plan for statement_id '{id}'"),
        None => "Error: cannot fetch last explain plan from PLAN_TABLE".to_owned(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn displayed(
        id: u64,
        depth: u64,
        operation: &str,
        options: Option<&str>,
        name: Option<&str>,
    ) -> DisplayedRow {
        DisplayedRow {
            id,
            depth,
            operation: operation.to_owned(),
            options: options.map(str::to_owned),
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn display_draws_the_id_operation_and_name_table_with_operations_indented_by_depth() {
        let rows = [
            displayed(0, 0, "SELECT STATEMENT", None, None),
            displayed(1, 1, "TABLE ACCESS", Some("FULL"), Some("EMP")),
        ];
        let lines = display_lines(&rows);
        assert!(lines[0].starts_with("Plan hash value: "), "{lines:?}");
        let rule = "-".repeat(34);
        assert_eq!(
            lines[1..8],
            [
                "",
                rule.as_str(),
                "| Id  | Operation         | Name |",
                rule.as_str(),
                "|   0 | SELECT STATEMENT  |      |",
                "|   1 |  TABLE ACCESS FULL| EMP  |",
                rule.as_str(),
            ]
        );
        assert_eq!(display_lines(&rows)[0], lines[0]);
    }

    #[test]
    fn a_plan_shown_without_a_plan_table_reads_as_the_written_one_does() {
        let rows = [
            PlanRow {
                id: 0,
                parent_id: None,
                depth: 0,
                position: None,
                operation: "SELECT STATEMENT",
                options: None,
                object: None,
            },
            PlanRow {
                id: 1,
                parent_id: Some(0),
                depth: 1,
                position: Some(1),
                operation: "TABLE ACCESS",
                options: Some("FULL"),
                object: Some(object("emp", "emp", "SEL$1", "TABLE")),
            },
        ];
        let written = [
            displayed(0, 0, "SELECT STATEMENT", None, None),
            displayed(1, 1, "TABLE ACCESS", Some("FULL"), Some("EMP")),
        ];
        assert_eq!(displayed_rows(&rows), written);
        let result = display_result(&rows);
        let expected: Vec<Vec<Value>> = display_lines(&written)
            .into_iter()
            .map(|line| vec![Value::Text(line)])
            .collect();
        assert_eq!(result.rows, expected);
        assert_eq!(
            result
                .columns
                .as_ref()
                .map(|columns| columns[0].name.as_str()),
            Some("plan_table_output")
        );
    }

    #[test]
    fn a_missing_plan_says_which_one_it_could_not_fetch() {
        assert_eq!(
            missing_plan_lines(None),
            ["Error: cannot fetch last explain plan from PLAN_TABLE"]
        );
        assert_eq!(
            missing_plan_lines(Some("q1")),
            ["Error: cannot fetch plan for statement_id 'q1'"]
        );
    }

    #[test]
    fn hostile_depths_and_quotes_never_panic() {
        let rows = [displayed(
            0,
            u64::MAX,
            "SELECT STATEMENT",
            Some("x'y"),
            Some("é"),
        )];
        assert_eq!(display_lines(&rows).len(), 11);
        assert_eq!(text_literal(Some("it's")), "'it''s'");
        assert_eq!(number_literal(None), "NULL");
    }
}
