use super::sql::{exit_for, print_result, value_json};
use super::table::{named_in, read_relations, shown_name};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::Style;
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb_commands::{CursorList, CursorRefusal, decode_cursor, encode_cursor, shown_cursor};
use eggdb_sql::{
    Dialect, OutputColumn, QueryResult, Relation, RelationKind, SqlError, TableSchema, Value,
};
use std::ffi::OsStr;

const DEFAULT_TEXT_PAGE_ROWS: u64 = 100;

struct RowPage {
    rows: Vec<Vec<Value>>,
    shown: u64,
    last: Option<Vec<u8>>,
    more: bool,
}

impl Session<'_> {
    pub(super) fn row_list(&mut self) -> Result<Exit, Failure> {
        let wanted = self.request.table.clone().unwrap_or_default();
        let db = self.open(Access::ReadOnly, None)?;
        let found = read_relations(&db)
            .map_err(|error| self.catalog_failure(&error))
            .map(|relations| named_in(relations, &wanted))?;
        let Some(relation) = found else {
            return Err(self.missing_table("list the rows of", &wanted));
        };
        let RelationKind::Table(table) = &relation.kind else {
            return Err(self.not_a_table(&wanted));
        };
        let scope = row_scope(&relation);
        let after = match &self.request.after {
            Some(cursor) => Some(
                decode_cursor(CursorList::Rows, &scope, cursor)
                    .map_err(|refusal| cursor_refused(CursorList::Rows, refusal, &scope, cursor))?,
            ),
            None => None,
        };
        let json = self.format == Format::Json;
        let typed_limit = self.request.scan.limit;
        let page_rows = match (json, typed_limit) {
            (_, Some(rows)) => Some(rows),
            (true, None) => None,
            (false, None) => Some(DEFAULT_TEXT_PAGE_ROWS),
        };
        let store = DatabaseStore(&db);
        let mut session = eggdb_sql::Session::with_dialect(&store, Dialect::PostgreSql);
        let mut page = RowPage {
            rows: Vec::new(),
            shown: 0,
            last: None,
            more: false,
        };
        let mut written: Result<(), Failure> = Ok(());
        let out = &mut self.out;
        let read = session.read_stored_rows(&relation, after.as_deref(), &mut |position, values| {
            if page_rows.is_some_and(|rows| page.shown >= rows) {
                page.more = true;
                return Ok(false);
            }
            if json {
                out.text(&row_record(table, &values));
                if let Err(failure) = out.line_done() {
                    written = Err(failure);
                    return Ok(false);
                }
            } else {
                page.rows.push(values);
            }
            page.shown += 1;
            page.last = Some(position.to_vec());
            Ok(true)
        });
        let _ = session.close();
        drop(db);
        written?;
        read.map_err(|error| self.rows_failure(&wanted, &error))?;
        let next_cursor = match (&page.last, page.more) {
            (Some(last), true) => Some(encode_cursor(CursorList::Rows, &scope, last)),
            _ => None,
        };
        if json {
            if let Some(cursor) = &next_cursor {
                self.out.text(&page_trailer(cursor));
            }
            return Ok(Exit::Done);
        }
        let result = page_result(table, page.rows);
        print_result(&mut self.out, &result, Dialect::PostgreSql);
        if let Some(cursor) = &next_cursor {
            self.out.finish()?;
            let mut next = format!(
                "more rows follow; next page: eggdb row list {} {}",
                self.file,
                render::text(wanted.as_encoded_bytes())
            );
            if let Some(rows) = typed_limit {
                next.push_str(&format!(" --limit {rows}"));
            }
            next.push_str(&format!(" --after {cursor}"));
            self.messages.styled(Style::Dim, &next);
            self.messages.text("\n");
        }
        Ok(Exit::Done)
    }

    fn not_a_table(&self, wanted: &OsStr) -> Failure {
        let file = &self.file;
        let shown = shown_name(wanted);
        let typed = render::text(wanted.as_encoded_bytes());
        Failure::Database {
            message: format!(
                "cannot list the rows of \"{shown}\" in {file}: it is a view, and a view has no stored order to page through. Sample it with `eggdb table sample {file} {typed}`, or query it with `eggdb sql`."
            ),
            try_command: Some(format!("eggdb table sample {file} {typed}")),
            exit: Exit::DatabaseError,
            code: "not_a_table",
        }
    }

    fn rows_failure(&self, wanted: &OsStr, error: &SqlError) -> Failure {
        let exit = exit_for(error);
        let code = match exit {
            Exit::Damaged => "damaged",
            Exit::Busy => "database_busy",
            _ => "database_failed",
        };
        Failure::coded(
            code,
            format!(
                "cannot read the rows of \"{}\" in {}: {}",
                shown_name(wanted),
                self.file,
                error.message()
            ),
            exit,
        )
    }
}

pub(super) fn cursor_refused(
    list: CursorList,
    refusal: CursorRefusal,
    scope: &str,
    cursor: &str,
) -> Failure {
    let shown = shown_cursor(&render::text(cursor.as_bytes()));
    let command = match list {
        CursorList::Rows => CommandName::Row,
        CursorList::Tables => CommandName::Table,
    };
    Failure::usage(refusal.message("--after", list, scope, &shown), Some(command))
}

fn row_scope(relation: &Relation) -> String {
    format!("{}.{}", relation.schema, relation.name)
}

fn row_record(table: &TableSchema, values: &[Value]) -> String {
    table
        .columns
        .iter()
        .zip(values)
        .fold(JsonObject::new(), |record, (column, value)| {
            record.raw(&column.name, &value_json(value, Dialect::PostgreSql))
        })
        .line()
}

pub(super) fn page_trailer(cursor: &str) -> String {
    JsonObject::new()
        .boolean("truncated", true)
        .string("next_cursor", cursor)
        .line()
}

fn page_result(table: &TableSchema, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult {
        tag: format!("SELECT {}", rows.len()),
        columns: Some(
            table
                .columns
                .iter()
                .map(|column| OutputColumn {
                    name: column.name.clone(),
                    sql_type: Some(column.sql_type),
                })
                .collect(),
        ),
        rows,
        notices: Vec::new(),
        warnings: Vec::new(),
        last_insert_id: None,
        last_insert_rowid: None,
        last_identity_value: None,
        info_messages: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{page_result, page_trailer, row_record};
    use eggdb_sql::{ColumnSchema, SqlType, TableSchema, Value};

    fn orders() -> TableSchema {
        let column = |name: &str, sql_type| ColumnSchema {
            name: name.to_owned(),
            sql_type,
            not_null: false,
            default: None,
            default_dialect: eggdb_sql::Dialect::PostgreSql,
            auto_increment: false,
            comment: None,
        };
        TableSchema {
            name: "orders".to_owned(),
            schema: "public".to_owned(),
            comment: None,
            columns: vec![column("id", SqlType::Integer), column("customer", SqlType::Text)],
            primary_key: vec!["id".to_owned()],
            indexes: Vec::new(),
        }
    }

    #[test]
    fn a_row_is_one_record_of_column_and_value_and_a_cut_page_ends_with_its_cursor() {
        let table = orders();
        assert_eq!(
            row_record(&table, &[Value::Integer(1), Value::Text("Ada".to_owned())]),
            "{\"id\":1,\"customer\":\"Ada\"}\n"
        );
        assert_eq!(row_record(&table, &[Value::Integer(2), Value::Null]), "{\"id\":2,\"customer\":null}\n");
        assert_eq!(
            page_trailer("r1.818549630180000002"),
            "{\"truncated\":true,\"next_cursor\":\"r1.818549630180000002\"}\n"
        );
        let page = page_result(&table, vec![vec![Value::Integer(1), Value::Null]]);
        assert_eq!(page.tag, "SELECT 1");
        assert_eq!(page.columns.map(|columns| columns.len()), Some(2));
    }
}
