use super::{QueryResult, Session};
use crate::ast::{LoadData, LoadDuplicates, LoadTarget};
use crate::copy::{CopyFormat, CopyIo, CopyStart};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::Relation;
use crate::lexer::{TokenKind, tokenize_in};
use crate::name_resolution::Binding;

const ROWS_PER_INSERT: usize = 500;
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

type Field = Option<Vec<u8>>;

enum Parsed {
    Record(Vec<Field>, usize),
    NeedMore,
    End(usize),
}

enum Seen {
    Yes,
    No,
    More,
}

fn seen(data: &[u8], at: usize, pattern: &[u8], eof: bool) -> Seen {
    if pattern.is_empty() {
        return Seen::No;
    }
    let rest = &data[at.min(data.len())..];
    if rest.starts_with(pattern) {
        return Seen::Yes;
    }
    if !eof && rest.len() < pattern.len() && pattern.starts_with(rest) {
        return Seen::More;
    }
    Seen::No
}

fn unescaped(byte: u8) -> u8 {
    match byte {
        b'0' => 0,
        b'b' => 8,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'Z' => 0x1A,
        other => other,
    }
}

struct Reader<'l> {
    load: &'l LoadData,
}

enum FieldEnd {
    Field(usize),
    Record(usize),
    NeedMore,
}

impl Reader<'_> {
    fn field_end(&self, data: &[u8], at: usize, eof: bool) -> FieldEnd {
        match seen(data, at, &self.load.fields_terminated, eof) {
            Seen::Yes => return FieldEnd::Field(at + self.load.fields_terminated.len()),
            Seen::More => return FieldEnd::NeedMore,
            Seen::No => {}
        }
        match seen(data, at, &self.load.lines_terminated, eof) {
            Seen::Yes => FieldEnd::Record(at + self.load.lines_terminated.len()),
            Seen::More => FieldEnd::NeedMore,
            Seen::No if at >= data.len() && eof => FieldEnd::Record(at),
            Seen::No if at >= data.len() => FieldEnd::NeedMore,
            Seen::No => FieldEnd::Field(usize::MAX),
        }
    }

    fn next(&self, data: &[u8], eof: bool) -> Parsed {
        let mut at = 0;
        if !self.load.lines_starting.is_empty() {
            let prefix = &self.load.lines_starting;
            match data
                .windows(prefix.len())
                .position(|window| window == prefix.as_slice())
            {
                Some(found) => at = found + prefix.len(),
                None if eof => return Parsed::End(data.len()),
                None => return Parsed::NeedMore,
            }
        }
        if at >= data.len() && eof {
            return Parsed::End(data.len());
        }
        let mut fields: Vec<Field> = Vec::new();
        loop {
            let quote = self
                .load
                .enclosed
                .filter(|quote| data.get(at) == Some(quote));
            let mut value = Vec::new();
            let raw_start = at;
            if let Some(quote) = quote {
                at += 1;
                loop {
                    let Some(&byte) = data.get(at) else {
                        if eof {
                            fields.push(Some(value));
                            return Parsed::Record(fields, at);
                        }
                        return Parsed::NeedMore;
                    };
                    if self.load.escaped == Some(byte) {
                        let Some(&next) = data.get(at + 1) else {
                            return if eof {
                                value.push(byte);
                                fields.push(Some(value));
                                Parsed::Record(fields, at + 1)
                            } else {
                                Parsed::NeedMore
                            };
                        };
                        value.push(unescaped(next));
                        at += 2;
                        continue;
                    }
                    if byte == quote {
                        if data.get(at + 1) == Some(&quote) {
                            value.push(quote);
                            at += 2;
                            continue;
                        }
                        if data.get(at + 1).is_none() && !eof {
                            return Parsed::NeedMore;
                        }
                        match self.field_end(data, at + 1, eof) {
                            FieldEnd::Field(next) if next != usize::MAX => {
                                fields.push(Some(value));
                                at = next;
                                break;
                            }
                            FieldEnd::Record(next) => {
                                fields.push(Some(value));
                                return Parsed::Record(fields, next);
                            }
                            FieldEnd::NeedMore => return Parsed::NeedMore,
                            FieldEnd::Field(_) => {
                                value.push(quote);
                                at += 1;
                            }
                        }
                        continue;
                    }
                    value.push(byte);
                    at += 1;
                }
                continue;
            }
            loop {
                match self.field_end(data, at, eof) {
                    FieldEnd::Field(next) if next != usize::MAX => {
                        fields.push(self.finished(value, &data[raw_start..at]));
                        at = next;
                        break;
                    }
                    FieldEnd::Record(next) => {
                        fields.push(self.finished(value, &data[raw_start..at]));
                        return Parsed::Record(fields, next);
                    }
                    FieldEnd::NeedMore => return Parsed::NeedMore,
                    FieldEnd::Field(_) => {}
                }
                let byte = data[at];
                if self.load.escaped == Some(byte) {
                    let Some(&next) = data.get(at + 1) else {
                        if eof {
                            value.push(byte);
                            at += 1;
                            continue;
                        }
                        return Parsed::NeedMore;
                    };
                    value.push(unescaped(next));
                    at += 2;
                    continue;
                }
                value.push(byte);
                at += 1;
            }
        }
    }

    fn finished(&self, value: Vec<u8>, raw: &[u8]) -> Field {
        let null = match self.load.escaped {
            Some(escape) => raw == [escape, b'N'],
            None => raw == b"NULL",
        };
        if null { None } else { Some(value) }
    }
}

fn mysql_literal(field: &Field) -> String {
    match field {
        None => "NULL".to_owned(),
        Some(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            let mut quoted = String::with_capacity(text.len() + 2);
            quoted.push('\'');
            for character in text.chars() {
                match character {
                    '\'' => quoted.push_str("''"),
                    '\\' => quoted.push_str("\\\\"),
                    '\0' => quoted.push_str("\\0"),
                    other => quoted.push(other),
                }
            }
            quoted.push('\'');
            quoted
        }
    }
}

fn quoted_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn substituted(
    expression: &str,
    variables: &[(String, String)],
    columns: &[(String, String)],
) -> SqlResult<String> {
    let tokens = tokenize_in(expression, Dialect::MySql)?;
    let mut out = String::with_capacity(expression.len());
    let mut copied = 0;
    for token in &tokens {
        let replacement = match &token.kind {
            TokenKind::Variable(name) => variables
                .iter()
                .find(|(known, _)| known.eq_ignore_ascii_case(name))
                .map(|(_, literal)| literal.clone()),
            TokenKind::Word(word) | TokenKind::QuotedIdentifier(word) => columns
                .iter()
                .find(|(known, _)| known.eq_ignore_ascii_case(word))
                .map(|(_, literal)| literal.clone()),
            _ => None,
        };
        if let Some(replacement) = replacement {
            out.push_str(expression.get(copied..token.start).unwrap_or_default());
            out.push_str(&replacement);
            copied = token.end;
        }
    }
    out.push_str(expression.get(copied..).unwrap_or_default());
    Ok(out)
}

struct Plan {
    targets: Vec<LoadTarget>,
    columns: Vec<String>,
    assignments: Vec<(String, String)>,
}

impl Plan {
    fn row(&self, fields: &[Field]) -> SqlResult<String> {
        let mut variables = Vec::new();
        let mut columns: Vec<(String, String)> = Vec::new();
        for (at, target) in self.targets.iter().enumerate() {
            let literal = fields.get(at).map_or_else(|| "NULL".to_owned(), mysql_literal);
            match target {
                LoadTarget::Column(name) => columns.push((name.clone(), literal)),
                LoadTarget::Variable(name) => variables.push((name.clone(), literal)),
            }
        }
        let mut values: Vec<String> = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            if let Some((_, expression)) = self
                .assignments
                .iter()
                .find(|(assigned, _)| assigned.eq_ignore_ascii_case(column))
            {
                values.push(format!("({})", substituted(expression, &variables, &columns)?));
                continue;
            }
            let literal = columns
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(column))
                .map_or_else(|| "DEFAULT".to_owned(), |(_, literal)| literal.clone());
            values.push(literal);
        }
        Ok(format!("({})", values.join(", ")))
    }
}

impl Session<'_> {
    pub(super) fn load_data(
        &mut self,
        load: &LoadData,
        io: &mut CopyIo<'_, '_>,
        binding: &Binding<'_>,
    ) -> SqlResult<QueryResult> {
        if !load.local {
            return Err(SqlError::new(
                SqlState::MYSQL_GENERAL,
                "The MySQL server is running with the --secure-file-priv option so it cannot execute this statement",
            )
            .with_vendor_code(1290));
        }
        let mut name = load.table.clone();
        let table = self.read(|reader, tables| {
            binding.names(reader, tables, std::slice::from_mut(&mut name))?;
            match tables.relation(reader, &name.text)? {
                Some(Relation::Table(table)) => Ok(table),
                _ => Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", load.table.text),
                )
                .at(load.table.position)),
            }
        })?;
        let table_columns: Vec<String> =
            table.columns.iter().map(|column| column.name.clone()).collect();
        let targets = load.targets.clone().unwrap_or_else(|| {
            table_columns
                .iter()
                .map(|name| LoadTarget::Column(name.clone()))
                .collect()
        });
        let mut columns: Vec<String> = Vec::new();
        for target in &targets {
            if let LoadTarget::Column(column) = target
                && !columns.iter().any(|known| known.eq_ignore_ascii_case(column))
            {
                columns.push(column.clone());
            }
        }
        for (column, _) in &load.assignments {
            if !columns.iter().any(|known| known.eq_ignore_ascii_case(column)) {
                columns.push(column.clone());
            }
        }
        let plan = Plan {
            targets,
            columns,
            assignments: load.assignments.clone(),
        };
        let verb = match load.duplicates {
            LoadDuplicates::Replace => "REPLACE",
            LoadDuplicates::Ignore => "INSERT IGNORE",
            LoadDuplicates::Error => "INSERT",
        };
        let head = format!(
            "{verb} INTO {}{} ({}) VALUES ",
            load.table_text,
            load.partitions
                .as_deref()
                .map_or_else(String::new, |partitions| format!(" {partitions}")),
            plan.columns
                .iter()
                .map(|column| quoted_identifier(column))
                .collect::<Vec<_>>()
                .join(", ")
        );
        io.channel.start_copy_in(&CopyStart {
            format: CopyFormat::Text,
            columns: plan.targets.len(),
            file: Some(load.file.clone()),
        })?;
        let reader = Reader { load };
        let mut buffer: Vec<u8> = Vec::new();
        let mut eof = false;
        let mut skipped_lines = load.ignore_lines;
        let mut batch: Vec<String> = Vec::new();
        let mut affected: u64 = 0;
        let mut records: u64 = 0;
        let outcome = (|| -> SqlResult<()> {
            loop {
                match reader.next(&buffer, eof) {
                    Parsed::Record(fields, consumed) => {
                        buffer.drain(..consumed);
                        if skipped_lines > 0 {
                            skipped_lines -= 1;
                            continue;
                        }
                        records += 1;
                        batch.push(plan.row(&fields)?);
                        if batch.len() >= ROWS_PER_INSERT {
                            affected += self.insert_loaded(&head, &mut batch)?;
                        }
                    }
                    Parsed::End(consumed) => {
                        buffer.drain(..consumed);
                        break;
                    }
                    Parsed::NeedMore => {
                        if eof {
                            break;
                        }
                        if buffer.len() > MAX_RECORD_BYTES {
                            return Err(SqlError::new(
                                SqlState::PROGRAM_LIMIT_EXCEEDED,
                                "a LOAD DATA record is longer than 64 MB",
                            ));
                        }
                        if !io.channel.read_copy_data(&mut buffer)? {
                            eof = true;
                        }
                    }
                }
            }
            if !batch.is_empty() {
                affected += self.insert_loaded(&head, &mut batch)?;
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            if !eof {
                io.channel.abandon_copy_in();
            }
            return Err(error);
        }
        let skipped = records.saturating_sub(affected);
        Ok(QueryResult::command(format!("INSERT 0 {affected}")).with_notice(format!(
            "Records: {records}  Deleted: 0  Skipped: {skipped}  Warnings: 0"
        )))
    }

    fn insert_loaded(&mut self, head: &str, batch: &mut Vec<String>) -> SqlResult<u64> {
        let text = format!("{head}{}", batch.join(", "));
        batch.clear();
        let results = self.run_quietly(&text)?;
        Ok(results
            .iter()
            .filter_map(|result| result.tag.rsplit(' ').next()?.parse::<u64>().ok())
            .sum())
    }
}
