//! SQL Server's partition functions and schemes: `CREATE PARTITION FUNCTION ... AS RANGE LEFT |
//! RIGHT FOR VALUES (...)`, `CREATE PARTITION SCHEME ... AS PARTITION ... TO (...)`, and tables
//! created `ON scheme(column)`, which become range-partitioned tables with one partition per
//! range the function defines (D975).

use crate::ast::Name;

/// A partition function or scheme statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PartitionSchemeCommand {
    CreateFunction {
        name: Name,
        input_type: String,
        right: bool,
        boundaries: Vec<String>,
    },
    DropFunction(Name),
    CreateScheme {
        name: Name,
        function: Name,
    },
    DropScheme(Name),
}

impl PartitionSchemeCommand {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::CreateFunction { .. } => "CREATE PARTITION FUNCTION",
            Self::DropFunction(_) => "DROP PARTITION FUNCTION",
            Self::CreateScheme { .. } => "CREATE PARTITION SCHEME",
            Self::DropScheme(_) => "DROP PARTITION SCHEME",
        }
    }
}

pub(crate) const FUNCTIONS_TABLE: &str = "eggdb_internal.sqlserver_partition_functions";
pub(crate) const SCHEMES_TABLE: &str = "eggdb_internal.sqlserver_partition_schemes";

pub(crate) const CREATE_TABLES: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
     CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_partition_functions (name text PRIMARY KEY, input_type text NOT NULL, boundary_on_right boolean NOT NULL, boundaries text NOT NULL); \
     CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_partition_schemes (name text PRIMARY KEY, function text NOT NULL)";

const BOUNDARY_SEPARATOR: char = '\u{1f}';

pub(crate) fn joined(boundaries: &[String]) -> String {
    boundaries.join(&BOUNDARY_SEPARATOR.to_string())
}

pub(crate) fn split(boundaries: &str) -> Vec<String> {
    if boundaries.is_empty() {
        return Vec::new();
    }
    boundaries.split(BOUNDARY_SEPARATOR).map(str::to_owned).collect()
}

fn postgresql_literal(literal: &str) -> String {
    let trimmed = literal.trim();
    match trimmed.strip_prefix(['N', 'n']) {
        Some(rest) if rest.starts_with('\'') => rest.to_owned(),
        _ => trimmed.to_owned(),
    }
}

/// The smallest value above `literal` in a column of SQL Server type `input_type`, as SQL in
/// PostgreSQL's dialect, so a `RANGE LEFT` boundary (which belongs to the range below it)
/// becomes the exclusive upper bound EggDB's range partitions take; `None` for types with no
/// next value (`decimal`, `float` and the like).
pub(crate) fn successor(input_type: &str, literal: &str) -> Option<String> {
    let value = postgresql_literal(literal);
    let base = input_type
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    Some(match base.as_str() {
        "tinyint" | "smallint" | "int" | "integer" | "bigint" => format!("({value}) + 1"),
        "date" => format!("CAST({value} AS date) + 1"),
        "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" => {
            format!("CAST({value} AS timestamp) + interval '1 microsecond'")
        }
        "char" | "varchar" | "nchar" | "nvarchar" | "sysname" => format!("({value}) || chr(1)"),
        _ => return None,
    })
}

/// The ranges of a partition function as `FOR VALUES FROM (...) TO (...)` bounds in
/// PostgreSQL's dialect, lowest first: one more range than there are boundaries.
pub(crate) fn ranges(input_type: &str, right: bool, boundaries: &[String]) -> Option<Vec<(String, String)>> {
    let mut cuts = Vec::with_capacity(boundaries.len());
    for boundary in boundaries {
        cuts.push(if right {
            postgresql_literal(boundary)
        } else {
            successor(input_type, boundary)?
        });
    }
    let mut ranges = Vec::with_capacity(cuts.len() + 1);
    let mut low = "MINVALUE".to_owned();
    for cut in cuts {
        ranges.push((low, cut.clone()));
        low = cut;
    }
    ranges.push((low, "MAXVALUE".to_owned()));
    Some(ranges)
}

#[cfg(test)]
mod tests {
    use super::{joined, ranges, split, successor};

    #[test]
    fn left_boundaries_become_the_next_value_and_right_ones_stay() {
        assert_eq!(successor("int", "100").as_deref(), Some("(100) + 1"));
        assert_eq!(successor("nvarchar(20)", "N'm'").as_deref(), Some("('m') || chr(1)"));
        assert_eq!(successor("decimal(10,2)", "1.5"), None);
        let right = ranges("int", true, &["10".to_owned(), "20".to_owned()]).unwrap();
        assert_eq!(right.first().unwrap(), &("MINVALUE".to_owned(), "10".to_owned()));
        assert_eq!(right.last().unwrap(), &("20".to_owned(), "MAXVALUE".to_owned()));
        assert_eq!(right.len(), 3);
        let left = ranges("date", false, &["'2024-01-01'".to_owned()]).unwrap();
        assert_eq!(left[0].1, "CAST('2024-01-01' AS date) + 1");
        let stored = vec!["1".to_owned(), "'a,b'".to_owned()];
        assert_eq!(split(&joined(&stored)), stored);
    }
}
