//! Invisible columns (D1032): SQL Server's `HIDDEN` period columns, and MySQL's and Oracle's
//! `INVISIBLE` columns, which `SELECT *` and an `INSERT` without a column list leave out while
//! naming them still reads and writes them. They are kept in `eggdb_internal.invisible_columns`
//! and loaded into a per-statement frame when a statement could expand `*` or an implicit column
//! list.

use crate::dialect::Dialect;
use crate::lexer::{TokenKind, tokenize_in};
use std::cell::RefCell;

pub(crate) const TABLE: &str = "eggdb_internal.invisible_columns";
pub(crate) const CREATE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.invisible_columns (table_name text NOT NULL, column_name text NOT NULL, PRIMARY KEY (table_name, column_name))";

thread_local! {
    static FRAME: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// Installs the invisible columns of the statement about to run.
pub(crate) fn set_frame(columns: Vec<(String, String)>) {
    FRAME.with(|frame| {
        if let Ok(mut frame) = frame.try_borrow_mut() {
            *frame = columns;
        }
    });
}

/// Whether `column` of `table` (an internal table name) is invisible to `*` and implicit column
/// lists in the statement running.
pub(crate) fn is_invisible(table: &str, column: &str) -> bool {
    FRAME.with(|frame| {
        frame.try_borrow().is_ok_and(|frame| {
            let base = crate::schemas::base_name(table);
            frame.iter().any(|(name, invisible)| name.eq_ignore_ascii_case(base) && invisible.eq_ignore_ascii_case(column))
        })
    })
}

/// Whether a statement could expand `*` or an implicit column list, so its frame is worth
/// loading.
pub(crate) fn wanted(text: &str) -> bool {
    text.contains('*') || text.trim_start().get(..6).is_some_and(|start| start.eq_ignore_ascii_case("insert"))
}

const NOT_COLUMNS: [&str; 10] = ["constraint", "primary", "period", "index", "unique", "key", "foreign", "check", "fulltext", "spatial"];

fn word(kind: &TokenKind<'_>) -> Option<String> {
    match kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        TokenKind::QuotedIdentifier(name) => Some(name.clone()),
        _ => None,
    }
}

/// The table and the invisible columns a `CREATE TABLE` statement declares (`HIDDEN` after a
/// generated row period column, or `INVISIBLE`), if any.
pub(crate) fn declared(text: &str, dialect: Dialect) -> Option<(String, Vec<String>)> {
    let lowered = text.to_ascii_lowercase();
    if !(lowered.contains("hidden") || lowered.contains("invisible")) || !lowered.trim_start().starts_with("create") {
        return None;
    }
    let tokens = tokenize_in(text, dialect).ok()?;
    let kinds: Vec<&TokenKind<'_>> = tokens.iter().map(|token| &token.kind).collect();
    let table_at = kinds.iter().position(|kind| word(kind).as_deref() == Some("table"))?;
    let mut at = table_at + 1;
    while kinds.get(at).and_then(|kind| word(kind)).is_some_and(|found| found == "if" || found == "not" || found == "exists") {
        at += 1;
    }
    let mut table = word(kinds.get(at)?)?;
    at += 1;
    while matches!(kinds.get(at), Some(TokenKind::Dot)) {
        table = word(kinds.get(at + 1)?)?;
        at += 2;
    }
    if !matches!(kinds.get(at), Some(TokenKind::LeftParen)) {
        return None;
    }
    let mut depth = 0usize;
    let mut definition: Vec<&TokenKind<'_>> = Vec::new();
    let mut invisible = Vec::new();
    let mut finish = |definition: &mut Vec<&TokenKind<'_>>| {
        let name = definition.first().and_then(|kind| word(kind));
        let hides = definition.iter().skip(1).any(|kind| matches!(word(kind).as_deref(), Some("hidden" | "invisible")));
        if let Some(name) = name
            && hides
            && !NOT_COLUMNS.contains(&name.as_str())
        {
            invisible.push(name);
        }
        definition.clear();
    };
    for kind in kinds[at..].iter().copied() {
        match kind {
            TokenKind::LeftParen => {
                depth += 1;
                if depth > 1 {
                    definition.push(kind);
                }
            }
            TokenKind::RightParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    finish(&mut definition);
                    break;
                }
                definition.push(kind);
            }
            TokenKind::Comma if depth == 1 => finish(&mut definition),
            _ if depth >= 1 => definition.push(kind),
            _ => {}
        }
    }
    (!invisible.is_empty()).then_some((table, invisible))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_invisible_columns_are_found_in_create_table() {
        let (table, columns) = declared(
            "CREATE TABLE dbo.Employee (Id int PRIMARY KEY, ValidFrom datetime2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL, ValidTo datetime2 GENERATED ALWAYS AS ROW END HIDDEN NOT NULL, PERIOD FOR SYSTEM_TIME (ValidFrom, ValidTo))",
            Dialect::SqlServer,
        )
        .unwrap();
        assert_eq!(table, "employee");
        assert_eq!(columns, vec!["validfrom".to_owned(), "validto".to_owned()]);
        let (_, mysql) = declared("CREATE TABLE t (a int, b int INVISIBLE, c decimal(10, 2))", Dialect::MySql).unwrap();
        assert_eq!(mysql, vec!["b".to_owned()]);
        assert!(declared("CREATE TABLE t (a int)", Dialect::PostgreSql).is_none());
    }
}
