mod format;

#[cfg(test)]
pub(crate) use format::FieldKind;
pub(crate) use format::{Fields, Input, Layout, MAX_RECORD_BYTES, RecordReader, RowWriter};

use crate::ast::{CopySource, CopyStatement, Name};
use crate::encoding::{TableDefinition, decode_row_into, prefix_end, rows_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{
    ChildRows, ColumnDefaults, Relation, StoredRows, TableCache, assign_to, target_columns,
    undefined_table,
};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;
use std::ops::Range;

const FOREIGN_KEY_BATCH_ROWS: usize = 4096;

/// The form of the data a `COPY` reads or writes: PostgreSQL's text format (tab-separated,
/// backslash escapes, `\N` for NULL) or its CSV format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyFormat {
    /// PostgreSQL's text format.
    #[default]
    Text,
    /// Comma-separated values, as PostgreSQL's `FORMAT csv` reads and writes them.
    Csv,
}

/// What a `COPY` with the client tells the client as the data starts to flow: PostgreSQL's
/// `CopyInResponse` and `CopyOutResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyStart {
    /// The form of the data.
    pub format: CopyFormat,
    /// How many columns each row has.
    pub columns: usize,
}

/// The client end of `COPY ... FROM STDIN` and `COPY ... TO STDOUT`: where the rows a `COPY`
/// loads come from, and where the rows it unloads go. A PostgreSQL connection sends and reads
/// the protocol's copy messages; a command-line client reads a file or its standard input and
/// writes its standard output.
pub trait CopyChannel {
    /// A `COPY ... FROM STDIN` is ready for its data, which [`CopyChannel::read_copy_data`]
    /// then reads.
    fn start_copy_in(&mut self, start: &CopyStart) -> SqlResult<()>;

    /// Appends the next piece of the data to `data`; `false`, with nothing appended, once the
    /// data has ended. A piece may end anywhere, even inside a row or a character. An error,
    /// such as the client failing the copy, fails the `COPY`, which then loads nothing.
    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool>;

    /// A `COPY ... FROM STDIN` failed before its data ended. The default does nothing; a
    /// channel may read and discard the rest, as a script's data lines must be.
    fn abandon_copy_in(&mut self) {}

    /// A `COPY ... TO STDOUT` is about to send its rows.
    fn start_copy_out(&mut self, start: &CopyStart) -> SqlResult<()>;

    /// Sends one row of a `COPY ... TO STDOUT`, ending in a newline.
    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()>;

    /// The rows of a `COPY ... TO STDOUT` have all been sent.
    fn finish_copy_out(&mut self) -> SqlResult<()>;
}

pub(crate) struct NoCopyChannel;

fn without_client(direction: &str) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("COPY {direction} needs a client that takes part in the copy"),
    )
    .with_hint("Run it from psql, eggdb sql or eggdb shell, or through Session::execute_with_copy.")
}

impl CopyChannel for NoCopyChannel {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Err(without_client("FROM STDIN"))
    }

    fn read_copy_data(&mut self, _data: &mut Vec<u8>) -> SqlResult<bool> {
        Err(without_client("FROM STDIN"))
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Err(without_client("TO STDOUT"))
    }

    fn write_copy_data(&mut self, _data: &[u8]) -> SqlResult<()> {
        Err(without_client("TO STDOUT"))
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Err(without_client("TO STDOUT"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CopyHeader {
    #[default]
    Absent,
    Present,
    Match,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum CopyColumns {
    #[default]
    None,
    All,
    Named(Vec<Name>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CopyOptions {
    pub(crate) format: CopyFormat,
    pub(crate) delimiter: Option<u8>,
    pub(crate) null: Option<String>,
    pub(crate) default: Option<String>,
    pub(crate) header: CopyHeader,
    pub(crate) quote: Option<u8>,
    pub(crate) escape: Option<u8>,
    pub(crate) force_quote: CopyColumns,
    pub(crate) force_not_null: CopyColumns,
    pub(crate) force_null: CopyColumns,
}

pub(crate) struct CopyIo<'c, 'd> {
    pub(crate) channel: &'c mut dyn CopyChannel,
    pub(crate) inline: Option<&'d [u8]>,
    pub(crate) consumed: Option<usize>,
}

fn shown_name(table: &TableDefinition) -> &str {
    crate::schemas::base_name(&table.name)
}

fn with_context(error: SqlError, context: impl FnOnce() -> String) -> SqlError {
    if error.context().is_some() {
        error
    } else {
        error.with_context(context())
    }
}

fn column_flags(choice: &CopyColumns, names: &[&str], option: &str) -> SqlResult<Vec<bool>> {
    match choice {
        CopyColumns::None => Ok(vec![false; names.len()]),
        CopyColumns::All => Ok(vec![true; names.len()]),
        CopyColumns::Named(chosen) => {
            let mut flags = vec![false; names.len()];
            for name in chosen {
                let index = names
                    .iter()
                    .position(|candidate| *candidate == name.text)
                    .ok_or_else(|| {
                        SqlError::new(
                            SqlState::INVALID_COLUMN_REFERENCE,
                            format!("{option} column \"{}\" not referenced by COPY", name.text),
                        )
                        .at(name.position)
                    })?;
                flags[index] = true;
            }
            Ok(flags)
        }
    }
}

fn copied_table(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &Name,
    to_client: bool,
) -> SqlResult<std::sync::Arc<TableDefinition>> {
    match tables
        .relation(reader, &name.text)
        .map_err(|error| error.at_if_unplaced(name.position))?
    {
        Some(Relation::Table(table)) => Ok(table),
        Some(Relation::View(_)) if to_client => Err(SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("cannot copy from view \"{}\"", name.text),
        )
        .with_hint("Try the COPY (SELECT ...) TO variant.")
        .at(name.position)),
        Some(Relation::View(_)) => Err(SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("cannot copy to view \"{}\"", name.text),
        )
        .with_hint("To enable copying to a view, provide an INSTEAD OF INSERT trigger.")
        .at(name.position)),
        None => Err(undefined_table(name)),
    }
}

fn table_source(copy: &CopyStatement) -> SqlResult<(&Name, &[Name])> {
    match &copy.source {
        CopySource::Table { name, columns } => Ok((name, columns)),
        CopySource::Query(_) => Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "COPY of a query reached the table path",
        )),
    }
}

fn targets_of(table: &TableDefinition, columns: &[Name]) -> SqlResult<Vec<usize>> {
    if columns.is_empty() {
        Ok((0..table.columns.len()).collect())
    } else {
        target_columns(table, columns)
    }
}

struct Loader<'t> {
    table: &'t TableDefinition,
    layout: Layout,
    targets: Vec<usize>,
    slots: Vec<Option<usize>>,
    force_not_null: Vec<bool>,
    force_null: Vec<bool>,
    defaults: ColumnDefaults,
    fields: Fields,
}

impl Loader<'_> {
    fn line_context(&self, line: u64, record: &[u8]) -> String {
        format!(
            "COPY {}, line {line}: \"{}\"",
            shown_name(self.table),
            format::shown_data(record)
        )
    }

    fn check_header(&mut self, record: &[u8], line: u64, matching: bool) -> SqlResult<()> {
        self.fields
            .split(record, &self.layout, &[], &[])
            .map_err(|error| with_context(error, || self.line_context(line, record)))?;
        if !matching {
            return Ok(());
        }
        let context = || format!("COPY {}, line {line}", shown_name(self.table));
        if self.fields.fields.len() != self.targets.len() {
            return Err(SqlError::new(
                SqlState::BAD_COPY_FILE_FORMAT,
                format!(
                    "wrong number of fields in header line: got {}, expected {}",
                    self.fields.fields.len(),
                    self.targets.len()
                ),
            )
            .with_context(context()));
        }
        for (number, (field, &target)) in self.fields.fields.iter().zip(&self.targets).enumerate() {
            let expected = &self.table.columns[target].name;
            let found = match field.kind {
                format::FieldKind::Null => None,
                _ => Some(String::from_utf8_lossy(self.fields.value(field))),
            };
            if found.as_deref() != Some(expected.as_str()) {
                let got = match found {
                    Some(name) => format!("\"{name}\""),
                    None => format!(
                        "null value (\"{}\")",
                        String::from_utf8_lossy(&self.layout.null)
                    ),
                };
                return Err(SqlError::new(
                    SqlState::BAD_COPY_FILE_FORMAT,
                    format!(
                        "column name mismatch in header line field {}: got {got}, expected \"{expected}\"",
                        number + 1
                    ),
                )
                .with_context(context()));
            }
        }
        Ok(())
    }

    fn row(&mut self, record: &[u8], line: u64, row: &mut Vec<Value>) -> SqlResult<()> {
        self.fields
            .split(record, &self.layout, &self.force_not_null, &self.force_null)
            .map_err(|error| with_context(error, || self.line_context(line, record)))?;
        let found = if self.targets.is_empty() && record.is_empty() {
            0
        } else {
            self.fields.fields.len()
        };
        if found > self.targets.len() {
            return Err(SqlError::new(
                SqlState::BAD_COPY_FILE_FORMAT,
                "extra data after last expected column",
            )
            .with_context(self.line_context(line, record)));
        }
        if found < self.targets.len() {
            let missing = &self.table.columns[self.targets[found]].name;
            return Err(SqlError::new(
                SqlState::BAD_COPY_FILE_FORMAT,
                format!("missing data for column \"{missing}\""),
            )
            .with_context(self.line_context(line, record)));
        }
        row.clear();
        for (index, column) in self.table.columns.iter().enumerate() {
            let value = match self.slots[index] {
                None => self.defaults.value(index, column),
                Some(slot) => {
                    let field = &self.fields.fields[slot];
                    match field.kind {
                        format::FieldKind::Null => Ok(Value::Null),
                        format::FieldKind::Default => self.defaults.value(index, column),
                        format::FieldKind::Value => {
                            let bytes = self.fields.value(field);
                            format::utf8_text(bytes)
                                .and_then(|text| {
                                    crate::eval::string_constant(text, Some(column.sql_type))
                                })
                                .and_then(|value| assign_to(value, column))
                                .map_err(|error| {
                                    with_context(error, || {
                                        format!(
                                            "COPY {}, line {line}, column {}: \"{}\"",
                                            shown_name(self.table),
                                            column.name,
                                            format::shown_data(bytes)
                                        )
                                    })
                                })
                        }
                    }
                }
            }?;
            row.push(value);
        }
        Ok(())
    }
}

pub(crate) fn copy_from_client(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    copy: &CopyStatement,
    io: &mut CopyIo<'_, '_>,
) -> SqlResult<QueryResult> {
    let (name, columns) = table_source(copy)?;
    let table = copied_table(writer, tables, name, false)?;
    let targets = targets_of(&table, columns)?;
    let names: Vec<&str> = targets
        .iter()
        .map(|&index| table.columns[index].name.as_str())
        .collect();
    let force_not_null = column_flags(&copy.options.force_not_null, &names, "FORCE_NOT_NULL")?;
    let force_null = column_flags(&copy.options.force_null, &names, "FORCE_NULL")?;
    let mut slots = vec![None; table.columns.len()];
    for (slot, &index) in targets.iter().enumerate() {
        slots[index] = Some(slot);
    }
    if crate::sequences::mentions_nextval(&table) {
        crate::sequences::load_all(writer)?;
    }
    let mut loader = Loader {
        table: &table,
        layout: Layout::of(&copy.options),
        slots,
        force_not_null,
        force_null,
        defaults: ColumnDefaults::of(&table)?,
        fields: Fields::default(),
        targets,
    };
    let start = CopyStart {
        format: copy.options.format,
        columns: loader.targets.len(),
    };
    let inline = io.inline;
    let input = match inline {
        Some(bytes) => Input::Inline(bytes),
        None => {
            io.channel.start_copy_in(&start)?;
            Input::Channel(&mut *io.channel)
        }
    };
    let mut reader = RecordReader::new(input, &loader.layout, MAX_RECORD_BYTES);
    let loaded = load(
        writer,
        tables,
        &mut loader,
        &mut reader,
        copy.options.header,
    );
    let loaded = loaded.and_then(|count| reader.drain().map(|()| count));
    match loaded {
        Ok(count) => {
            if inline.is_some() {
                io.consumed = Some(reader.consumed());
            }
            Ok(QueryResult::command(format!("COPY {count}")))
        }
        Err(error) => {
            reader.abandon();
            Err(error)
        }
    }
}

fn next_record(
    reader: &mut RecordReader<'_>,
    table: &TableDefinition,
) -> SqlResult<Option<Range<usize>>> {
    reader.next_record().map_err(|error| {
        with_context(error, || {
            format!("COPY {}, line {}", shown_name(table), reader.line())
        })
    })
}

fn load(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    loader: &mut Loader<'_>,
    reader: &mut RecordReader<'_>,
    header: CopyHeader,
) -> SqlResult<u64> {
    let table = loader.table;
    if header != CopyHeader::Absent
        && let Some(range) = next_record(reader, table)?
    {
        let line = reader.line();
        loader.check_header(reader.bytes(range), line, header == CopyHeader::Match)?;
    }
    let checks = crate::constraints::PreparedChecks::of(table)?;
    let mut stored = StoredRows::checked(table, &checks);
    let mut children = ChildRows::new(table);
    let deferred_parents = table
        .foreign_keys
        .iter()
        .any(|key| key.parent == table.name);
    let mut row = Vec::with_capacity(table.columns.len());
    let mut count = 0u64;
    while let Some(range) = next_record(reader, table)? {
        let line = reader.line();
        loader.row(reader.bytes(range), line, &mut row)?;
        let context = || format!("COPY {}, line {line}", shown_name(table));
        stored
            .insert(writer, &mut row)
            .map_err(|error| with_context(error, context))?;
        children
            .push(&row)
            .map_err(|error| with_context(error, context))?;
        count += 1;
        if !deferred_parents && children.len() >= FOREIGN_KEY_BATCH_ROWS {
            children.check_and_forget(writer, tables)?;
        }
    }
    children.check(writer, tables)?;
    Ok(count)
}

fn texts_of(row: &[Value], targets: &[usize], texts: &mut Vec<Option<String>>) {
    texts.clear();
    texts.extend(
        targets
            .iter()
            .map(|&index| row.get(index).and_then(Value::to_text)),
    );
}

fn writers(
    copy: &CopyStatement,
    names: &[&str],
) -> SqlResult<(RowWriter, Option<Vec<Option<String>>>)> {
    let layout = Layout::of(&copy.options);
    let force_quote = column_flags(&copy.options.force_quote, names, "FORCE_QUOTE")?;
    let header = (copy.options.header != CopyHeader::Absent)
        .then(|| names.iter().map(|name| Some((*name).to_owned())).collect());
    Ok((RowWriter::new(layout, force_quote), header))
}

fn send_header(
    channel: &mut dyn CopyChannel,
    copy: &CopyStatement,
    header: Option<Vec<Option<String>>>,
) -> SqlResult<()> {
    if let Some(names) = header {
        let mut plain = RowWriter::new(Layout::of(&copy.options), Vec::new());
        channel.write_copy_data(plain.line(&names))?;
    }
    Ok(())
}

pub(crate) fn copy_table_to_client(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    copy: &CopyStatement,
    channel: &mut dyn CopyChannel,
) -> SqlResult<QueryResult> {
    let (name, columns) = table_source(copy)?;
    let table = copied_table(reader, tables, name, true)?;
    let targets = targets_of(&table, columns)?;
    let names: Vec<&str> = targets
        .iter()
        .map(|&index| table.columns[index].name.as_str())
        .collect();
    let (mut rows, header) = writers(copy, &names)?;
    channel.start_copy_out(&CopyStart {
        format: copy.options.format,
        columns: targets.len(),
    })?;
    send_header(channel, copy, header)?;
    let prefix = rows_prefix(table.id);
    let mut row = Vec::with_capacity(table.columns.len());
    let mut texts = Vec::with_capacity(targets.len());
    let mut count = 0u64;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, bytes| {
            row.clear();
            decode_row_into(bytes, &table.columns, None, &mut row)?;
            row.resize(table.columns.len(), Value::Null);
            texts_of(&row, &targets, &mut texts);
            channel.write_copy_data(rows.line(&texts))?;
            count += 1;
            Ok(true)
        },
    )?;
    channel.finish_copy_out()?;
    Ok(QueryResult::command(format!("COPY {count}")))
}

pub(crate) fn copy_rows_to_client(
    result: QueryResult,
    copy: &CopyStatement,
    channel: &mut dyn CopyChannel,
) -> SqlResult<QueryResult> {
    let columns = result.columns.unwrap_or_default();
    let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    let (mut rows, header) = writers(copy, &names)?;
    channel.start_copy_out(&CopyStart {
        format: copy.options.format,
        columns: names.len(),
    })?;
    send_header(channel, copy, header)?;
    let all: Vec<usize> = (0..names.len()).collect();
    let mut texts = Vec::with_capacity(names.len());
    for row in &result.rows {
        texts_of(row, &all, &mut texts);
        channel.write_copy_data(rows.line(&texts))?;
    }
    channel.finish_copy_out()?;
    Ok(QueryResult::command(format!("COPY {}", result.rows.len())))
}
