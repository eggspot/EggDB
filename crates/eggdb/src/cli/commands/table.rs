use super::sql::{exit_for, print_result, statement_object};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format, TableAction};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::{Output, Style};
use crate::cli::render::{self, JsonObject, json_string};
use crate::cli::reports::label;
use eggdb::Database;
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::{
    Dialect, EXACT_ROW_COUNT_UP_TO, QueryResult, Relation, RelationKind, SqlError, Value,
    find_relation,
};
use std::ffi::OsStr;

const DEFAULT_SAMPLE_ROWS: u64 = 10;
const SHOWN_NAME_CHARACTERS: usize = 64;

struct RowCount {
    count: i64,
    exact: bool,
}

impl Session<'_> {
    pub(super) fn table_command(&mut self) -> Result<Exit, Failure> {
        match self.request.table_action {
            Some(TableAction::List) => self.list_tables(),
            Some(TableAction::Show) => self.show_table(),
            Some(TableAction::Sample) => self.sample_table(),
            None => Err(Failure::usage(
                "table needs list, show or sample",
                Some(CommandName::Table),
            )),
        }
    }

    fn list_tables(&mut self) -> Result<Exit, Failure> {
        let db = self.open(Access::ReadOnly, None)?;
        let relations = read_relations(&db).map_err(|error| self.catalog_failure(&error))?;
        drop(db);
        let only = self.request.schema.clone();
        let listed: Vec<&Relation> = tables_then_views(&relations)
            .into_iter()
            .filter(|relation| only.as_deref().is_none_or(|schema| relation.schema == schema))
            .collect();
        match self.format {
            Format::Json => {
                for relation in &listed {
                    self.out.text(&relation_record(relation).line());
                    self.out.line_done()?;
                }
            }
            _ => {
                let rows: Vec<Vec<String>> = listed
                    .iter()
                    .map(|relation| relation_line_cells(relation))
                    .collect();
                self.out.text(&aligned_lines(&rows, ""));
                self.out.finish()?;
                let tables = listed
                    .iter()
                    .filter(|relation| matches!(relation.kind, RelationKind::Table(_)))
                    .count();
                let views = listed.len() - tables;
                self.messages
                    .styled(Style::Dim, &relation_counts(tables, views));
                self.messages.text("\n");
            }
        }
        Ok(Exit::Done)
    }

    fn show_table(&mut self) -> Result<Exit, Failure> {
        let wanted = self.request.table.clone().unwrap_or_default();
        let db = self.open(Access::ReadOnly, None)?;
        let found = read_relations(&db)
            .map_err(|error| self.catalog_failure(&error))
            .map(|relations| named_in(relations, &wanted))?;
        let Some(relation) = found else {
            return Err(self.missing_table("show", &wanted));
        };
        let rows = count_rows(&db, &relation).map_err(|error| self.catalog_failure(&error))?;
        drop(db);
        match self.format {
            Format::Json => self.out.text(&description_json(&relation, &rows)),
            _ => describe_text(&mut self.out, &relation, &rows),
        }
        Ok(Exit::Done)
    }

    fn sample_table(&mut self) -> Result<Exit, Failure> {
        let wanted = self.request.table.clone().unwrap_or_default();
        let limit = self.request.scan.limit.unwrap_or(DEFAULT_SAMPLE_ROWS);
        let db = self.open(Access::ReadOnly, None)?;
        let found = read_relations(&db)
            .map_err(|error| self.catalog_failure(&error))
            .map(|relations| named_in(relations, &wanted))?;
        let Some(relation) = found else {
            return Err(self.missing_table("sample", &wanted));
        };
        let sampled = first_rows(&db, &relation, limit.saturating_add(1))
            .map_err(|error| self.catalog_failure(&error))?;
        drop(db);
        let Some(mut result) = sampled else {
            return Ok(Exit::Done);
        };
        let shown = usize::try_from(limit).unwrap_or(usize::MAX);
        let more = result.rows.len() > shown;
        result.rows.truncate(shown);
        result.tag = format!("SELECT {}", result.rows.len());
        match self.format {
            Format::Json => self.out.text(
                &statement_object(&result, Dialect::PostgreSql)
                    .boolean("truncated", more)
                    .line(),
            ),
            _ => {
                print_result(&mut self.out, &result, Dialect::PostgreSql);
                if more {
                    self.out.finish()?;
                    self.messages.styled(
                        Style::Dim,
                        "more rows follow: -n shows up to 1,000, eggdb sql shows them all",
                    );
                    self.messages.text("\n");
                }
            }
        }
        Ok(Exit::Done)
    }

    fn catalog_failure(&self, error: &SqlError) -> Failure {
        let exit = exit_for(error);
        let code = match exit {
            Exit::Damaged => "damaged",
            Exit::Busy => "database_busy",
            _ => "database_failed",
        };
        Failure::coded(
            code,
            format!("cannot read the tables of {}: {}", self.file, error.message()),
            exit,
        )
    }

    fn missing_table(&self, verb: &str, wanted: &OsStr) -> Failure {
        let file = &self.file;
        let shown = shown_name(wanted);
        Failure::Database {
            message: format!(
                "cannot {verb} \"{shown}\" in {file}: there is no table or view with that name. List the names with `eggdb table list {file}`."
            ),
            try_command: Some(format!("eggdb table list {file}")),
            exit: Exit::NotFound,
            code: "table_not_found",
        }
    }
}

fn shown_name(wanted: &OsStr) -> String {
    match wanted.to_str() {
        Some(name) => {
            let cut: String = name.chars().take(SHOWN_NAME_CHARACTERS).collect();
            render::text(cut.as_bytes())
        }
        None => render::text(wanted.as_encoded_bytes()),
    }
}

fn named_in(relations: Vec<Relation>, wanted: &OsStr) -> Option<Relation> {
    find_relation(relations, wanted.to_str()?)
}

fn read_relations(db: &Database) -> Result<Vec<Relation>, SqlError> {
    let store = DatabaseStore(db);
    let mut session = eggdb_sql::Session::with_dialect(&store, Dialect::PostgreSql);
    let relations = session.relations();
    let _ = session.close();
    relations
}

fn one_result(db: &Database, statement: &str) -> Result<Option<QueryResult>, SqlError> {
    let store = DatabaseStore(db);
    let mut session = eggdb_sql::Session::with_dialect(&store, Dialect::PostgreSql);
    let mut result = None;
    let ran = session.execute(statement, &mut |returned| result = Some(returned));
    let _ = session.close();
    ran.map(|()| result)
}

fn count_rows(db: &Database, relation: &Relation) -> Result<RowCount, SqlError> {
    let counted = one_result(db, &relation.bounded_count_statement(EXACT_ROW_COUNT_UP_TO))?
        .and_then(|result| result.rows.into_iter().next())
        .and_then(|row| row.into_iter().next());
    let count = match counted {
        Some(Value::BigInt(count)) => count,
        _ => 0,
    };
    Ok(RowCount {
        count,
        exact: count <= EXACT_ROW_COUNT_UP_TO,
    })
}

fn first_rows(
    db: &Database,
    relation: &Relation,
    rows: u64,
) -> Result<Option<QueryResult>, SqlError> {
    one_result(db, &relation.first_rows_statement(rows))
}

fn tables_then_views(relations: &[Relation]) -> Vec<&Relation> {
    let is_table = |relation: &&Relation| matches!(relation.kind, RelationKind::Table(_));
    let tables = relations.iter().filter(is_table);
    let views = relations.iter().filter(|relation| !is_table(relation));
    tables.chain(views).collect()
}

fn relation_record(relation: &Relation) -> JsonObject {
    JsonObject::new()
        .string("schema", &relation.schema)
        .string("name", &relation.name)
        .string("kind", relation.kind_name())
        .number("columns", relation.column_count() as u64)
        .optional_string("comment", relation.comment())
}

fn qualified(relation: &Relation) -> String {
    render::text(format!("{}.{}", relation.schema, relation.name).as_bytes())
}

fn columns_phrase(count: usize) -> String {
    match count {
        1 => "1 column".to_owned(),
        count => format!("{} columns", render::count(count as u64)),
    }
}

fn relation_line_cells(relation: &Relation) -> Vec<String> {
    let mut cells = vec![
        qualified(relation),
        relation.kind_name().to_owned(),
        columns_phrase(relation.column_count()),
    ];
    if let Some(comment) = relation.comment() {
        cells.push(render::text(comment.as_bytes()));
    }
    cells
}

fn relation_counts(tables: usize, views: usize) -> String {
    let tables = match tables {
        1 => "1 table".to_owned(),
        count => format!("{} tables", render::count(count as u64)),
    };
    let views = match views {
        1 => "1 view".to_owned(),
        count => format!("{} views", render::count(count as u64)),
    };
    format!("{tables}, {views}")
}

fn aligned_lines(rows: &[Vec<String>], indent: &str) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in rows {
        let mut line = String::from(indent);
        for (index, (cell, width)) in row.iter().zip(&widths).enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            line.push_str(&" ".repeat(width - cell.chars().count()));
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

fn rows_phrase(rows: &RowCount) -> String {
    if rows.exact {
        render::count(u64::try_from(rows.count).unwrap_or(0))
    } else {
        format!(
            "more than {}",
            render::count(u64::try_from(EXACT_ROW_COUNT_UP_TO).unwrap_or(0))
        )
    }
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items
            .iter()
            .map(|item| render::text(item.as_bytes()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn describe_text(out: &mut Output<'_>, relation: &Relation, rows: &RowCount) {
    out.styled(Style::Bold, &qualified(relation));
    out.text("\n");
    label(out, "kind", relation.kind_name(), Style::Bold);
    label(out, "rows", &rows_phrase(rows), Style::Bold);
    label(
        out,
        "columns",
        &render::count(relation.column_count() as u64),
        Style::Bold,
    );
    match &relation.kind {
        RelationKind::Table(table) => {
            let columns: Vec<Vec<String>> = table
                .columns
                .iter()
                .map(|column| {
                    let mut attributes = Vec::new();
                    if column.not_null {
                        attributes.push("not null".to_owned());
                    }
                    if column.auto_increment {
                        attributes.push("auto increment".to_owned());
                    }
                    if let Some(default) = &column.default {
                        attributes.push(format!("default {}", render::text(default.as_bytes())));
                    }
                    let mut cells = vec![
                        render::text(column.name.as_bytes()),
                        column.sql_type.name(),
                        attributes.join(", "),
                    ];
                    if let Some(comment) = &column.comment {
                        cells.push(render::text(comment.as_bytes()));
                    }
                    cells
                })
                .collect();
            out.text(&aligned_lines(&columns, "    "));
            label(out, "primary key", &list_or_none(&table.primary_key), Style::Bold);
            let indexes = if table.indexes.is_empty() {
                "none".to_owned()
            } else {
                render::count(table.indexes.len() as u64)
            };
            label(out, "indexes", &indexes, Style::Bold);
            let lines: Vec<Vec<String>> = table
                .indexes
                .iter()
                .map(|index| {
                    let columns = format!("({})", list_or_none(&index.columns));
                    let shape = if index.unique {
                        format!("unique {columns}")
                    } else {
                        columns
                    };
                    vec![render::text(index.name.as_bytes()), shape]
                })
                .collect();
            out.text(&aligned_lines(&lines, "    "));
        }
        RelationKind::View(view) => {
            let columns: Vec<Vec<String>> = view
                .columns
                .iter()
                .map(|column| {
                    vec![
                        render::text(column.name.as_bytes()),
                        column.sql_type.map(|sql_type| sql_type.name()).unwrap_or_default(),
                    ]
                })
                .collect();
            out.text(&aligned_lines(&columns, "    "));
        }
    }
    if let Some(comment) = relation.comment() {
        label(out, "comment", &render::text(comment.as_bytes()), Style::Bold);
    }
}

fn strings(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|item| json_string(item)).collect();
    format!("[{}]", items.join(","))
}

fn description_json(relation: &Relation, rows: &RowCount) -> String {
    let mut object = JsonObject::new()
        .string("schema", &relation.schema)
        .string("name", &relation.name)
        .string("kind", relation.kind_name());
    match &relation.kind {
        RelationKind::Table(table) => {
            let columns: Vec<String> = table
                .columns
                .iter()
                .map(|column| {
                    let mut described = JsonObject::new()
                        .string("name", &column.name)
                        .string("type", &column.sql_type.name())
                        .boolean("not_null", column.not_null)
                        .optional_string("default", column.default.as_deref());
                    if column.auto_increment {
                        described = described.boolean("auto_increment", true);
                    }
                    described
                        .optional_string("comment", column.comment.as_deref())
                        .object()
                })
                .collect();
            let indexes: Vec<String> = table
                .indexes
                .iter()
                .map(|index| {
                    JsonObject::new()
                        .string("name", &index.name)
                        .boolean("unique", index.unique)
                        .raw("columns", &strings(&index.columns))
                        .object()
                })
                .collect();
            object = object
                .raw("columns", &format!("[{}]", columns.join(",")))
                .raw("primary_key", &strings(&table.primary_key))
                .raw("indexes", &format!("[{}]", indexes.join(",")));
        }
        RelationKind::View(view) => {
            let columns: Vec<String> = view
                .columns
                .iter()
                .map(|column| {
                    let kind = column
                        .sql_type
                        .map_or_else(|| "null".to_owned(), |sql_type| json_string(&sql_type.name()));
                    JsonObject::new()
                        .string("name", &column.name)
                        .raw("type", &kind)
                        .object()
                })
                .collect();
            object = object.raw("columns", &format!("[{}]", columns.join(",")));
        }
    }
    let counted = JsonObject::new()
        .number("count", u64::try_from(rows.count).unwrap_or(0))
        .boolean("exact", rows.exact)
        .object();
    object
        .optional_string("comment", relation.comment())
        .raw("rows", &counted)
        .line()
}

#[cfg(test)]
mod tests {
    use super::{RowCount, aligned_lines, relation_counts, rows_phrase, shown_name};
    use std::ffi::OsStr;

    #[test]
    fn lines_are_padded_to_their_widest_cell_and_end_without_spaces() {
        let rows = vec![
            vec!["public.orders".to_owned(), "table".to_owned(), "5 columns".to_owned()],
            vec![
                "public.users".to_owned(),
                "table".to_owned(),
                "4 columns".to_owned(),
                "the people who sign in".to_owned(),
            ],
            vec!["sales.totals".to_owned(), "view".to_owned(), "2 columns".to_owned()],
        ];
        assert_eq!(
            aligned_lines(&rows, ""),
            "public.orders  table  5 columns\npublic.users   table  4 columns  the people who sign in\nsales.totals   view   2 columns\n"
        );
        assert_eq!(
            aligned_lines(&[vec!["id".to_owned(), String::new()]], "    "),
            "    id\n"
        );
    }

    #[test]
    fn counts_and_names_read_as_the_design_shows_them() {
        assert_eq!(relation_counts(2, 1), "2 tables, 1 view");
        assert_eq!(relation_counts(1, 0), "1 table, 0 views");
        assert_eq!(
            rows_phrase(&RowCount {
                count: 1204,
                exact: true
            }),
            "1,204"
        );
        assert_eq!(
            rows_phrase(&RowCount {
                count: 10_001,
                exact: false
            }),
            "more than 10,000"
        );
        assert_eq!(shown_name(OsStr::new(&"x".repeat(70))), "x".repeat(64));
    }
}
