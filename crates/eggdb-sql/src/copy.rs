mod binary;
mod encodings;
mod format;

pub(crate) use encodings::{CopyEncoding, EncodingName};
#[cfg(test)]
pub(crate) use format::FieldKind;
pub(crate) use format::{Fields, Input, Layout, MAX_RECORD_BYTES, RecordReader, RowWriter};

use crate::ast::{CopySource, CopyStatement, Expr, Name};
use crate::encoding::{Column, TableDefinition, decode_row_into, prefix_end, rows_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source, check_columns, reject_aggregates};
use crate::execute::{
    ChildRows, ColumnDefaults, Relation, StoredRows, TableCache, assign_to, target_columns,
    undefined_table,
};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{DefinedKind, SqlType, Value};
use binary::{BinaryReader, BinaryWriter};
use encodings::Transcoded;
use std::ops::Range;

const FOREIGN_KEY_BATCH_ROWS: usize = 4096;

/// The form of the data a `COPY` reads or writes: PostgreSQL's text format (tab-separated,
/// backslash escapes, `\N` for NULL), its CSV format or its binary format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyFormat {
    /// PostgreSQL's text format.
    #[default]
    Text,
    /// Comma-separated values, as PostgreSQL's `FORMAT csv` reads and writes them.
    Csv,
    /// PostgreSQL's binary format: a signature and header, then each row as its field count
    /// and each field's length and bytes in its type's binary form, then a trailer.
    Binary,
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

    /// Sends one piece of a `COPY ... TO STDOUT`: one row ending in a newline in the text
    /// and CSV formats; in the binary format one row, the first one led by the file header,
    /// and last the trailer.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CopyOnError {
    #[default]
    Stop,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CopyLogVerbosity {
    #[default]
    Default,
    Verbose,
    Silent,
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
    pub(crate) encoding: CopyEncoding,
    pub(crate) on_error: CopyOnError,
    pub(crate) log_verbosity: CopyLogVerbosity,
}

pub(crate) struct CopyIo<'c, 'd> {
    pub(crate) channel: &'c mut dyn CopyChannel,
    pub(crate) inline: Option<&'d [u8]>,
    pub(crate) consumed: Option<usize>,
    pub(crate) bulk_rows: Option<&'c mut dyn crate::bulk_load::BulkRows>,
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

const SKIPPED_ROW: &str = "skipping row due to data type incompatibility";

fn is_data_type_error(error: &SqlError) -> bool {
    let state = error.state();
    state.code().starts_with("22")
        && state != SqlState::CHARACTER_NOT_IN_REPERTOIRE
        && state != SqlState::UNTRANSLATABLE_CHARACTER
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRead {
    Ready,
    Skipped,
}

fn table_scope(table: &TableDefinition) -> Scope<'_> {
    Scope::One(Source {
        table,
        label: shown_name(table),
        offset: 0,
    })
}

fn checked_condition<'c>(
    copy: &'c CopyStatement,
    table: &TableDefinition,
) -> SqlResult<Option<&'c Expr>> {
    let Some(condition) = &copy.condition else {
        return Ok(None);
    };
    reject_aggregates(condition, "COPY FROM WHERE conditions")?;
    if let Some(found) = crate::window::first_call(condition) {
        return Err(SqlError::new(
            SqlState::WINDOWING_ERROR,
            "window functions are not allowed in COPY FROM WHERE conditions",
        )
        .at(found.position));
    }
    if crate::subquery::mentions_subquery(condition) {
        return Err(
            SqlError::not_supported("cannot use subquery in COPY FROM WHERE condition")
                .at(condition.position),
        );
    }
    check_columns(condition, table_scope(table))?;
    Ok(Some(condition))
}

fn kept_by(condition: &Expr, table: &TableDefinition, row: &[Value]) -> SqlResult<bool> {
    let evaluator = Evaluator {
        scope: table_scope(table),
        row,
        correlation: None,
    };
    Ok(evaluator.boolean(condition, "WHERE")? == Some(true))
}

fn refuse_binary_type(column: &str, sql_type: SqlType) -> SqlError {
    SqlError::not_supported(format!(
        "COPY in the binary format does not handle column \"{column}\" of type {} yet",
        sql_type.name()
    ))
    .with_hint("Use FORMAT text or FORMAT csv for this column.")
}

fn transcoding(options: &CopyOptions) -> Option<CopyEncoding> {
    (options.format != CopyFormat::Binary && options.encoding != CopyEncoding::Utf8)
        .then_some(options.encoding)
}

fn text_value(bytes: &[u8], column: &Column) -> SqlResult<Value> {
    format::utf8_text(bytes)
        .and_then(|text| crate::eval::string_constant(text, Some(column.sql_type)))
        .and_then(|value| assign_to(value, column))
}

fn binary_value(bytes: &[u8], column: &Column) -> SqlResult<Value> {
    let value = match column.sql_type {
        SqlType::Array(element) => {
            let (lower_bound, texts) = crate::pg_binary::decode_array_texts(bytes, element)?;
            let items = texts
                .iter()
                .map(|text| match text {
                    None => Ok(Value::Null),
                    Some(text) => crate::eval::string_constant(text, Some(element.sql_type())),
                })
                .collect::<SqlResult<Vec<_>>>()?;
            let mut array = crate::arrays::ArrayValue::new(element, items);
            array.lower_bound = lower_bound;
            Value::Array(Box::new(array))
        }
        SqlType::Defined(defined) if defined.kind == DefinedKind::Enum => {
            crate::eval::string_constant(format::utf8_text(bytes)?, Some(column.sql_type))?
        }
        other => {
            let text = crate::pg_binary::decode_pg_binary_text(
                bytes,
                crate::pg_catalog::type_oid(other),
            )?;
            crate::eval::string_constant(&text, Some(other))?
        }
    };
    assign_to(value, column)
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
    empty_is_null: bool,
    on_error: CopyOnError,
    log_verbosity: CopyLogVerbosity,
    skipped: u64,
    notices: Vec<String>,
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

    fn failed_field(
        &mut self,
        error: SqlError,
        line: u64,
        index: usize,
        shown: Option<String>,
    ) -> SqlResult<RowRead> {
        let table = self.table;
        let column = &table.columns[index].name;
        if self.on_error == CopyOnError::Stop || !is_data_type_error(&error) {
            return Err(with_context(error, || match &shown {
                Some(shown) => format!(
                    "COPY {}, line {line}, column {column}: \"{shown}\"",
                    shown_name(table)
                ),
                None => format!("COPY {}, line {line}, column {column}", shown_name(table)),
            }));
        }
        self.skipped += 1;
        if self.log_verbosity == CopyLogVerbosity::Verbose {
            self.notices.push(format!(
                "NOTICE:  {SKIPPED_ROW} at line {line} for column \"{column}\": \"{}\"",
                shown.unwrap_or_default()
            ));
        }
        Ok(RowRead::Skipped)
    }

    fn row(&mut self, record: &[u8], line: u64, row: &mut Vec<Value>) -> SqlResult<RowRead> {
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
        let table = self.table;
        for (index, column) in table.columns.iter().enumerate() {
            let value = match self.slots[index] {
                None => self.defaults.value(index, column)?,
                Some(slot) => {
                    let field = &self.fields.fields[slot];
                    match field.kind {
                        format::FieldKind::Null => Value::Null,
                        format::FieldKind::Default => self.defaults.value(index, column)?,
                        format::FieldKind::Value
                            if self.empty_is_null && self.fields.value(field).is_empty() =>
                        {
                            Value::Null
                        }
                        format::FieldKind::Value => {
                            let bytes = self.fields.value(field);
                            match text_value(bytes, column) {
                                Ok(value) => value,
                                Err(error) => {
                                    let shown = format::shown_data(bytes);
                                    return self.failed_field(error, line, index, Some(shown));
                                }
                            }
                        }
                    }
                }
            };
            row.push(value);
        }
        Ok(RowRead::Ready)
    }

    fn binary_row(
        &mut self,
        reader: &BinaryReader<'_>,
        fields: &[Option<Range<usize>>],
        line: u64,
        row: &mut Vec<Value>,
    ) -> SqlResult<RowRead> {
        row.clear();
        let table = self.table;
        for (index, column) in table.columns.iter().enumerate() {
            let value = match self.slots[index] {
                None => self.defaults.value(index, column)?,
                Some(slot) => match fields.get(slot).cloned().flatten() {
                    None => Value::Null,
                    Some(range) => match binary_value(reader.bytes(range), column) {
                        Ok(value) => value,
                        Err(error) => return self.failed_field(error, line, index, None),
                    },
                },
            };
            row.push(value);
        }
        Ok(RowRead::Ready)
    }
}

trait RowSource {
    fn next_row(
        &mut self,
        loader: &mut Loader<'_>,
        row: &mut Vec<Value>,
    ) -> SqlResult<Option<(u64, RowRead)>>;

    fn finish(&mut self) -> SqlResult<()>;

    fn abandon(&mut self);
}

struct TextRows<'a> {
    reader: RecordReader<'a>,
    header: CopyHeader,
}

impl RowSource for TextRows<'_> {
    fn next_row(
        &mut self,
        loader: &mut Loader<'_>,
        row: &mut Vec<Value>,
    ) -> SqlResult<Option<(u64, RowRead)>> {
        let table = loader.table;
        if self.header != CopyHeader::Absent {
            let matching = self.header == CopyHeader::Match;
            self.header = CopyHeader::Absent;
            if let Some(range) = next_record(&mut self.reader, table)? {
                let line = self.reader.line();
                loader.check_header(self.reader.bytes(range), line, matching)?;
            }
        }
        let Some(range) = next_record(&mut self.reader, table)? else {
            return Ok(None);
        };
        let line = self.reader.line();
        let read = loader.row(self.reader.bytes(range), line, row)?;
        Ok(Some((line, read)))
    }

    fn finish(&mut self) -> SqlResult<()> {
        self.reader.drain()
    }

    fn abandon(&mut self) {
        self.reader.abandon();
    }
}

struct BinaryRows<'a> {
    reader: BinaryReader<'a>,
    fields: Vec<Option<Range<usize>>>,
    columns: usize,
    header_read: bool,
}

impl RowSource for BinaryRows<'_> {
    fn next_row(
        &mut self,
        loader: &mut Loader<'_>,
        row: &mut Vec<Value>,
    ) -> SqlResult<Option<(u64, RowRead)>> {
        let table = loader.table;
        if !self.header_read {
            self.header_read = true;
            self.reader.header().map_err(|error| {
                with_context(error, || format!("COPY {}, line 0", shown_name(table)))
            })?;
        }
        let more = self
            .reader
            .next_tuple(self.columns, &mut self.fields)
            .map_err(|error| {
                let line = self.reader.line();
                with_context(error, || format!("COPY {}, line {line}", shown_name(table)))
            })?;
        if !more {
            return Ok(None);
        }
        let line = self.reader.line();
        let read = loader.binary_row(&self.reader, &self.fields, line, row)?;
        Ok(Some((line, read)))
    }

    fn finish(&mut self) -> SqlResult<()> {
        self.reader.drain()
    }

    fn abandon(&mut self) {
        self.reader.abandon();
    }
}

fn refuse_inline_binary() -> SqlError {
    SqlError::not_supported("COPY FROM STDIN in the binary format cannot take its data from a script")
        .with_hint(
            "Send the data from a client: psql's \\copy, a driver's binary copy, or Session::execute_with_copy.",
        )
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
    let condition = checked_condition(copy, &table)?;
    let binary = copy.options.format == CopyFormat::Binary;
    if binary {
        if io.inline.is_some() {
            return Err(refuse_inline_binary());
        }
        for &index in &targets {
            let column = &table.columns[index];
            if !crate::pg_binary::reads_binary(column.sql_type) {
                return Err(refuse_binary_type(&column.name, column.sql_type));
            }
        }
    }
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
        empty_is_null: crate::constraints::current_dialect() == crate::dialect::Dialect::Oracle,
        on_error: copy.options.on_error,
        log_verbosity: copy.options.log_verbosity,
        skipped: 0,
        notices: Vec::new(),
    };
    let start = CopyStart {
        format: copy.options.format,
        columns: loader.targets.len(),
    };
    let loaded = match io.inline {
        Some(bytes) => {
            let mut rows = TextRows {
                reader: RecordReader::new(Input::Inline(bytes), &loader.layout, MAX_RECORD_BYTES),
                header: copy.options.header,
            };
            let loaded = load_all_rows(writer, tables, &mut loader, &mut rows, condition);
            if loaded.is_ok() {
                io.consumed = Some(rows.reader.consumed());
            }
            loaded
        }
        None => {
            io.channel.start_copy_in(&start)?;
            let mut transcoded;
            let channel: &mut dyn CopyChannel = match transcoding(&copy.options) {
                Some(encoding) => {
                    transcoded = Transcoded::new(&mut *io.channel, encoding);
                    &mut transcoded
                }
                None => &mut *io.channel,
            };
            if binary {
                let mut rows = BinaryRows {
                    reader: BinaryReader::new(channel, MAX_RECORD_BYTES),
                    fields: Vec::new(),
                    columns: loader.targets.len(),
                    header_read: false,
                };
                load_all_rows(writer, tables, &mut loader, &mut rows, condition)
            } else {
                let mut rows = TextRows {
                    reader: RecordReader::new(
                        Input::Channel(channel),
                        &loader.layout,
                        MAX_RECORD_BYTES,
                    ),
                    header: copy.options.header,
                };
                load_all_rows(writer, tables, &mut loader, &mut rows, condition)
            }
        }
    };
    let count = loaded?;
    let mut result = QueryResult::command(format!("COPY {count}"));
    result.notices = std::mem::take(&mut loader.notices);
    if loader.skipped > 0 && loader.log_verbosity != CopyLogVerbosity::Silent {
        let rows = if loader.skipped == 1 {
            "row was"
        } else {
            "rows were"
        };
        result.notices.push(format!(
            "NOTICE:  {} {rows} skipped due to data type incompatibility",
            loader.skipped
        ));
    }
    Ok(result)
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

fn load_all_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    loader: &mut Loader<'_>,
    rows: &mut dyn RowSource,
    condition: Option<&Expr>,
) -> SqlResult<u64> {
    let loaded = load(writer, tables, loader, rows, condition)
        .and_then(|count| rows.finish().map(|()| count));
    if loaded.is_err() {
        rows.abandon();
    }
    loaded
}

fn load(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    loader: &mut Loader<'_>,
    rows: &mut dyn RowSource,
    condition: Option<&Expr>,
) -> SqlResult<u64> {
    let table = loader.table;
    let checks =
        crate::constraints::PreparedChecks::of(table)?.with_partitions(writer, tables, table)?;
    let mut stored = StoredRows::checked(table, &checks);
    let mut children = ChildRows::new(table);
    let deferred_parents = table
        .foreign_keys
        .iter()
        .any(|key| key.parent == table.name);
    let mut row = Vec::with_capacity(table.columns.len());
    let mut count = 0u64;
    let routing = RoutedRows::of(writer, tables, table)?;
    let mut routed = Vec::new();
    while let Some((line, read)) = rows.next_row(loader, &mut row)? {
        if read == RowRead::Skipped {
            continue;
        }
        let context = || format!("COPY {}, line {line}", shown_name(table));
        if let Some(condition) = condition
            && !kept_by(condition, table, &row).map_err(|error| with_context(error, context))?
        {
            continue;
        }
        if let Some(routing) = &routing
            && routing.takes(&row)
        {
            routed.push(row.clone());
            count += 1;
            if routed.len() >= ROUTED_BATCH_ROWS {
                routing
                    .flush(writer, tables, table, &mut routed)
                    .map_err(|error| with_context(error, context))?;
            }
            continue;
        }
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
    if let Some(routing) = &routing {
        routing.flush(writer, tables, table, &mut routed)?;
    }
    children.check(writer, tables)?;
    Ok(count)
}

const ROUTED_BATCH_ROWS: usize = 1024;

enum RoutedRows {
    Every,
    OutsideOwnHashPartition {
        root: String,
        constraint: std::sync::Arc<crate::partitions::PartitionConstraint>,
    },
}

impl RoutedRows {
    fn of(
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        table: &TableDefinition,
    ) -> SqlResult<Option<Self>> {
        if crate::partitions::partitioned(writer, tables, table)?.is_some() {
            return Ok(Some(Self::Every));
        }
        let Some(constraint) = crate::partitions::constraint_of(writer, tables, table)? else {
            return Ok(None);
        };
        let Some(root) = constraint.root().map(str::to_owned) else {
            return Ok(None);
        };
        Ok(constraint
            .uses_hash()
            .then_some(Self::OutsideOwnHashPartition { root, constraint }))
    }

    fn takes(&self, row: &[Value]) -> bool {
        match self {
            Self::Every => true,
            Self::OutsideOwnHashPartition { constraint, .. } => !constraint.holds(row),
        }
    }

    fn flush(
        &self,
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        table: &TableDefinition,
        rows: &mut Vec<Vec<Value>>,
    ) -> SqlResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let target = match self {
            Self::Every => table.name.clone(),
            Self::OutsideOwnHashPartition { root, .. } => root.clone(),
        };
        let values = rows
            .drain(..)
            .map(|row| {
                row.into_iter()
                    .map(|value| {
                        crate::ast::InsertValue::Expr(crate::ast::Expr {
                            kind: crate::ast::ExprKind::Constant {
                                sql_type: value.sql_type(),
                                value,
                            },
                            position: 0,
                        })
                    })
                    .collect()
            })
            .collect();
        let columns = table
            .columns
            .iter()
            .map(|column| crate::ast::Name {
                text: column.name.clone(),
                position: 0,
            })
            .collect();
        let insert = crate::ast::Insert {
            table: crate::ast::TableReference::named(
                crate::ast::Name {
                    text: target,
                    position: 0,
                },
                None,
            ),
            columns: Some(columns),
            source: crate::ast::InsertSource::Values(values),
            returning: None,
            on_duplicate: crate::ast::OnDuplicate::Fail,
            rollback_on_conflict: false,
            overriding: Some(crate::ast::Overriding::SystemValue),
        };
        crate::execute::insert(writer, tables, &insert).map(|_| ())
    }
}

fn texts_of(row: &[Value], targets: &[usize], texts: &mut Vec<Option<String>>) {
    texts.clear();
    texts.extend(
        targets
            .iter()
            .map(|&index| row.get(index).and_then(Value::to_text)),
    );
}

enum RowsOut {
    Text {
        writer: RowWriter,
        texts: Vec<Option<String>>,
    },
    Binary(BinaryWriter),
}

impl RowsOut {
    fn of(copy: &CopyStatement, names: &[&str], types: &[SqlType]) -> SqlResult<Self> {
        if copy.options.format == CopyFormat::Binary {
            for (name, &sql_type) in names.iter().zip(types) {
                if !crate::pg_binary::writes_binary(sql_type) {
                    return Err(refuse_binary_type(name, sql_type));
                }
            }
            let oids = types
                .iter()
                .map(|&sql_type| crate::pg_catalog::type_oid(sql_type))
                .collect();
            return Ok(Self::Binary(BinaryWriter::new(
                oids,
                crate::datetime::session_zone(),
            )));
        }
        let force_quote = column_flags(&copy.options.force_quote, names, "FORCE_QUOTE")?;
        Ok(Self::Text {
            writer: RowWriter::new(Layout::of(&copy.options), force_quote),
            texts: Vec::with_capacity(names.len()),
        })
    }

    fn send(
        &mut self,
        channel: &mut dyn CopyChannel,
        row: &[Value],
        targets: &[usize],
    ) -> SqlResult<()> {
        match self {
            Self::Text { writer, texts } => {
                texts_of(row, targets, texts);
                channel.write_copy_data(writer.line(texts))
            }
            Self::Binary(writer) => {
                let values: Vec<&Value> = targets
                    .iter()
                    .filter_map(|&index| row.get(index))
                    .collect();
                channel.write_copy_data(writer.row(&values)?)
            }
        }
    }

    fn finish(&mut self, channel: &mut dyn CopyChannel) -> SqlResult<()> {
        if let Self::Binary(writer) = self {
            channel.write_copy_data(writer.trailer())?;
        }
        channel.finish_copy_out()
    }
}

fn send_header(
    channel: &mut dyn CopyChannel,
    copy: &CopyStatement,
    names: &[&str],
) -> SqlResult<()> {
    if copy.options.header != CopyHeader::Absent {
        let names: Vec<Option<String>> = names.iter().map(|name| Some((*name).to_owned())).collect();
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
    let types: Vec<SqlType> = targets
        .iter()
        .map(|&index| table.columns[index].sql_type)
        .collect();
    let mut rows = RowsOut::of(copy, &names, &types)?;
    let mut transcoded;
    let channel: &mut dyn CopyChannel = match transcoding(&copy.options) {
        Some(encoding) => {
            transcoded = Transcoded::new(channel, encoding);
            &mut transcoded
        }
        None => channel,
    };
    channel.start_copy_out(&CopyStart {
        format: copy.options.format,
        columns: targets.len(),
    })?;
    send_header(channel, copy, &names)?;
    let prefix = rows_prefix(table.id);
    let mut row = Vec::with_capacity(table.columns.len());
    let mut count = 0u64;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, bytes| {
            row.clear();
            decode_row_into(bytes, &table.columns, None, &mut row)?;
            row.resize(table.columns.len(), Value::Null);
            rows.send(channel, &row, &targets)?;
            count += 1;
            Ok(true)
        },
    )?;
    rows.finish(channel)?;
    Ok(QueryResult::command(format!("COPY {count}")))
}

pub(crate) fn copy_rows_to_client(
    result: QueryResult,
    copy: &CopyStatement,
    channel: &mut dyn CopyChannel,
) -> SqlResult<QueryResult> {
    let columns = result.columns.unwrap_or_default();
    let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    let types: Vec<SqlType> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            column
                .sql_type
                .or_else(|| {
                    result
                        .rows
                        .iter()
                        .find_map(|row| row.get(index).and_then(Value::sql_type))
                })
                .unwrap_or(SqlType::Text)
        })
        .collect();
    let mut rows = RowsOut::of(copy, &names, &types)?;
    let mut transcoded;
    let channel: &mut dyn CopyChannel = match transcoding(&copy.options) {
        Some(encoding) => {
            transcoded = Transcoded::new(channel, encoding);
            &mut transcoded
        }
        None => channel,
    };
    channel.start_copy_out(&CopyStart {
        format: copy.options.format,
        columns: names.len(),
    })?;
    send_header(channel, copy, &names)?;
    let all: Vec<usize> = (0..names.len()).collect();
    for row in &result.rows {
        rows.send(channel, row, &all)?;
    }
    rows.finish(channel)?;
    Ok(QueryResult::command(format!("COPY {}", result.rows.len())))
}
