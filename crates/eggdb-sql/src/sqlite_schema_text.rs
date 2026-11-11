use crate::ast::{AlterAction, CreateIndex, CreateTable, CreateView};
use crate::dialect::Dialect;
use crate::encoding::{NAMESPACE, ViewDefinition, is_view_record, table_key};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::lexer::{Token, TokenKind, tokenize_in};
use crate::schema::{IndexSchema, TableSchema};
use crate::schemas::{DEFAULT_SCHEMA, base_name};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite};

pub(crate) const SQLITE_TEXT_SPACE: u8 = 0x15;
const RECORD_VERSION: u8 = 1;
const LENGTH_BYTES: usize = 4;
const PART_SEPARATOR: u8 = 0xFF;
const INDEX_PART: u8 = b'i';
const TABLE_CONSTRAINT_WORDS: [&str; 5] = ["constraint", "primary", "unique", "check", "foreign"];
const WORDS_AFTER_A_TYPE: [&str; 11] = [
    "constraint",
    "primary",
    "not",
    "null",
    "unique",
    "check",
    "default",
    "collate",
    "references",
    "generated",
    "as",
];

fn text_key(relation: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(SQLITE_TEXT_SPACE);
    key.extend_from_slice(relation.as_bytes());
    key
}

fn indexes_prefix(table: &str) -> Vec<u8> {
    let mut key = text_key(table);
    key.push(PART_SEPARATOR);
    key.push(INDEX_PART);
    key
}

fn index_key(table: &str, index: &str) -> Vec<u8> {
    let mut key = indexes_prefix(table);
    key.extend_from_slice(index.as_bytes());
    key
}

/// The relation a key of this space belongs to, given the key's bytes after the space number:
/// a table's or view's own record is its internal name, and an index's record is its table's
/// internal name followed by a byte that never occurs in UTF-8.
pub(crate) fn owning_relation(rest: &[u8]) -> &[u8] {
    rest.split(|&byte| byte == PART_SEPARATOR)
        .next()
        .unwrap_or(rest)
}

fn encode(generated: &str, original: &str) -> Option<Vec<u8>> {
    let length = u32::try_from(generated.len()).ok()?;
    let mut out = Vec::with_capacity(1 + LENGTH_BYTES + generated.len() + original.len());
    out.push(RECORD_VERSION);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(generated.as_bytes());
    out.extend_from_slice(original.as_bytes());
    Some(out)
}

#[derive(Debug, PartialEq, Eq)]
enum Unreadable {
    Damaged,
    NewerVersion(u8),
}

fn decode(bytes: &[u8]) -> Result<(&str, &str), Unreadable> {
    let (&version, rest) = bytes.split_first().ok_or(Unreadable::Damaged)?;
    if version > RECORD_VERSION {
        return Err(Unreadable::NewerVersion(version));
    }
    if version != RECORD_VERSION {
        return Err(Unreadable::Damaged);
    }
    let texts = || {
        let (length, rest) = rest.split_first_chunk::<LENGTH_BYTES>()?;
        let length = usize::try_from(u32::from_le_bytes(*length)).ok()?;
        let generated = std::str::from_utf8(rest.get(..length)?).ok()?;
        let original = std::str::from_utf8(rest.get(length..)?).ok()?;
        Some((generated, original))
    };
    texts().ok_or(Unreadable::Damaged)
}

fn kept_original(bytes: &[u8], generated: &str) -> SqlResult<Option<String>> {
    match decode(bytes) {
        Ok((kept_generated, original)) => {
            Ok((kept_generated == generated).then(|| original.to_owned()))
        }
        Err(Unreadable::Damaged) => Ok(None),
        Err(Unreadable::NewerVersion(version)) => Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!(
                "the SQL CREATE TABLE text was written in format version {version}, which this EggDB cannot read"
            ),
        )
        .with_hint("Open the file with the EggDB release that wrote it, or a newer one.")),
    }
}

fn remember(
    writer: &mut dyn KeyValueWrite,
    key: &[u8],
    generated: &str,
    original: &str,
) -> SqlResult<()> {
    match encode(generated, original) {
        Some(record) => writer.put(key, &record),
        None => Ok(()),
    }
}

fn kept(reader: &dyn KeyValueRead, key: &[u8], generated: &str) -> SqlResult<Option<String>> {
    let Some(bytes) = reader.get(key)? else {
        return Ok(None);
    };
    kept_original(&bytes, generated)
}

pub(crate) fn create_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateTable,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let original = match &create.sqlite_text {
        Some(text) if dialect == Dialect::Sqlite && create.temporary.is_none() => text,
        _ => return execute::create_table(writer, tables, create, dialect),
    };
    let existed = writer.get(&table_key(&create.name.text))?.is_some();
    let result = execute::create_table(writer, tables, create, dialect)?;
    if !existed && let Some(table) = crate::schema::read_table(&*writer, &create.name.text)? {
        remember(
            writer,
            &text_key(&create.name.text),
            &crate::session::sqlite_create_text(&table),
            original,
        )?;
    }
    Ok(result)
}

fn index_of<'t>(table: &'t TableSchema, name: &str) -> Option<&'t IndexSchema> {
    table.indexes.iter().find(|index| index.name == name)
}

pub(crate) fn create_index(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateIndex,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let (Some(original), Some(name)) = (&create.sqlite_text, &create.index.name) else {
        return crate::indexes::create_index(writer, tables, create, dialect);
    };
    if dialect != Dialect::Sqlite {
        return crate::indexes::create_index(writer, tables, create, dialect);
    }
    let table_name = &create.table.text;
    let index_name = base_name(&name.text);
    let existed = crate::schema::read_table(&*writer, table_name)?
        .is_some_and(|table| index_of(&table, index_name).is_some());
    let result = crate::indexes::create_index(writer, tables, create, dialect)?;
    if !existed
        && let Some(table) = crate::schema::read_table(&*writer, table_name)?
        && let Some(index) = index_of(&table, index_name)
    {
        remember(
            writer,
            &index_key(table_name, &index.name),
            &crate::session::sqlite_index_text(&table, index),
            original,
        )?;
    }
    Ok(result)
}

fn read_view(reader: &dyn KeyValueRead, internal: &str) -> SqlResult<Option<ViewDefinition>> {
    match reader.get(&table_key(internal))? {
        Some(bytes) if is_view_record(&bytes) => ViewDefinition::decode(internal, &bytes).map(Some),
        _ => Ok(None),
    }
}

pub(crate) fn create_view(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateView,
    context: &crate::views::ViewContext<'_>,
) -> SqlResult<QueryResult> {
    let original = match &create.sqlite_text {
        Some(text) if context.dialect == Dialect::Sqlite && !create.temporary => text,
        _ => return crate::views::create_view(writer, tables, create, context),
    };
    let Ok(internal) = crate::schemas::creation_name(&*writer, &create.name) else {
        return crate::views::create_view(writer, tables, create, context);
    };
    let existed = writer.get(&table_key(&internal))?.is_some();
    let result = crate::views::create_view(writer, tables, create, context)?;
    if !existed && let Some(view) = read_view(&*writer, &internal)? {
        remember(
            writer,
            &text_key(&internal),
            &crate::session::sqlite_view_text(&view),
            original,
        )?;
    }
    Ok(result)
}

pub(crate) fn original_text(
    reader: &dyn KeyValueRead,
    table: &str,
    generated: &str,
) -> SqlResult<Option<String>> {
    kept(reader, &text_key(table), generated)
}

pub(crate) fn original_index_text(
    reader: &dyn KeyValueRead,
    table: &str,
    index: &str,
    generated: &str,
) -> SqlResult<Option<String>> {
    kept(reader, &index_key(table, index), generated)
}

pub(crate) fn forget(writer: &mut dyn KeyValueWrite, relation: &str) -> SqlResult<()> {
    writer.delete(&text_key(relation))?;
    crate::store::delete_prefix(writer, &indexes_prefix(relation))
}

pub(crate) fn forget_index(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    index: &str,
) -> SqlResult<()> {
    writer.delete(&index_key(table, index)).map(|_| ())
}

/// The original texts of a SQLite table and its indexes, taken out of the catalog before an
/// `ALTER TABLE` and saved again, edited the way SQLite edits them, when the change is one
/// SQLite edits the text for.
#[derive(Debug, Default)]
pub(crate) struct AlteredTexts {
    table: Option<String>,
    indexes: Vec<(String, String)>,
}

fn public_table(reader: &dyn KeyValueRead, table: &str) -> SqlResult<Option<TableSchema>> {
    Ok(crate::schema::read_table(reader, table)?.filter(|schema| schema.schema == DEFAULT_SCHEMA))
}

impl AlteredTexts {
    pub(crate) fn take(
        writer: &mut dyn KeyValueWrite,
        table: &str,
        dialect: Dialect,
    ) -> SqlResult<Self> {
        let mut taken = Self::default();
        if dialect == Dialect::Sqlite
            && let Some(schema) = public_table(&*writer, table)?
        {
            taken.table = original_text(
                &*writer,
                table,
                &crate::session::sqlite_create_text(&schema),
            )
            .ok()
            .flatten();
            for index in &schema.indexes {
                let generated = crate::session::sqlite_index_text(&schema, index);
                if let Some(original) =
                    original_index_text(&*writer, table, &index.name, &generated)
                        .ok()
                        .flatten()
                {
                    taken.indexes.push((index.name.clone(), original));
                }
            }
        }
        forget(writer, table)?;
        Ok(taken)
    }

    pub(crate) fn save(
        self,
        writer: &mut dyn KeyValueWrite,
        table: &str,
        actions: &[AlterAction],
    ) -> SqlResult<()> {
        if self.table.is_none() && self.indexes.is_empty() {
            return Ok(());
        }
        let Some(schema) = public_table(&*writer, table)? else {
            return Ok(());
        };
        let shown_name = base_name(table);
        let mut text = self.table;
        let mut renamed = false;
        let mut column_renames: Vec<(&str, &str)> = Vec::new();
        for action in actions {
            match action {
                AlterAction::Accepted => {}
                AlterAction::AddColumn {
                    sqlite_text: Some(column),
                    ..
                } => text = text.and_then(|kept| added_column_text(&kept, column)),
                AlterAction::RenameTable(_) => {
                    renamed = true;
                    text = text.and_then(|kept| renamed_table_text(&kept, shown_name));
                }
                AlterAction::RenameColumn { from, to } => {
                    column_renames.push((&from.text, &to.text));
                    text = text.and_then(|kept| {
                        renamed_column_text(&kept, shown_name, &from.text, &to.text)
                    });
                }
                AlterAction::DropColumn { name, .. } => {
                    text = text.and_then(|kept| dropped_column_text(&kept, &name.text));
                }
                _ => return Ok(()),
            }
        }
        if let Some(text) = text {
            remember(
                writer,
                &text_key(table),
                &crate::session::sqlite_create_text(&schema),
                &text,
            )?;
        }
        for (name, original) in self.indexes {
            let Some(index) = index_of(&schema, &name) else {
                continue;
            };
            let mut edited = if renamed {
                renamed_index_text(&original, shown_name)
            } else {
                Some(original)
            };
            for (from, to) in &column_renames {
                edited = edited.and_then(|kept| renamed_index_column_text(&kept, from, to));
            }
            if let Some(edited) = edited {
                remember(
                    writer,
                    &index_key(table, &name),
                    &crate::session::sqlite_index_text(&schema, index),
                    &edited,
                )?;
            }
        }
        Ok(())
    }
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn is_word(token: &Token<'_>, word: &str) -> bool {
    matches!(&token.kind, TokenKind::Word(text) if text.eq_ignore_ascii_case(word))
}

fn is_name(token: &Token<'_>) -> bool {
    matches!(
        token.kind,
        TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) | TokenKind::String(_)
    )
}

fn replaced(text: &str, start: usize, end: usize, with: &str) -> Option<String> {
    Some(format!("{}{with}{}", text.get(..start)?, text.get(end..)?))
}

fn renamed_table_text(text: &str, new_name: &str) -> Option<String> {
    let tokens = tokenize_in(text, Dialect::Sqlite).ok()?;
    let name = tokens.get(2).filter(|token| {
        tokens.first().is_some_and(|first| is_word(first, "create"))
            && tokens.get(1).is_some_and(|second| is_word(second, "table"))
            && is_name(token)
    })?;
    replaced(text, name.start, name.end, &quoted(new_name))
}

fn renamed_index_text(text: &str, new_table: &str) -> Option<String> {
    let tokens = tokenize_in(text, Dialect::Sqlite).ok()?;
    let on = tokens.iter().position(|token| is_word(token, "on"))?;
    let table = tokens.get(on + 1).filter(|token| is_name(token))?;
    replaced(text, table.start, table.end, &quoted(new_table))
}

fn identifier_text<'t>(token: &'t Token<'_>) -> Option<&'t str> {
    match &token.kind {
        TokenKind::Word(word) => Some(word.as_ref()),
        TokenKind::QuotedIdentifier(name) => Some(name.as_str()),
        _ => None,
    }
}

fn names_column(token: &Token<'_>, column: &str) -> bool {
    identifier_text(token).is_some_and(|name| name.eq_ignore_ascii_case(column))
}

fn with_edits(text: &str, mut spans: Vec<(usize, usize)>, with: &str) -> Option<String> {
    spans.sort_unstable();
    let mut edited = text.to_owned();
    for (start, end) in spans.into_iter().rev() {
        edited = replaced(&edited, start, end, with)?;
    }
    Some(edited)
}

fn renamed_column_text(text: &str, table: &str, from: &str, to: &str) -> Option<String> {
    let tokens = tokenize_in(text, Dialect::Sqlite).ok()?;
    let open = tokens
        .iter()
        .position(|token| token.kind == TokenKind::LeftParen)?;
    let mut depth = 0usize;
    let mut element_start = false;
    let mut expecting_referenced = false;
    let mut referenced_list: Option<usize> = None;
    let mut other_table_columns = false;
    let mut spans = Vec::new();
    for token in tokens.get(open..)? {
        match token.kind {
            TokenKind::LeftParen => {
                depth += 1;
                if depth == 1 {
                    element_start = true;
                }
                if other_table_columns {
                    referenced_list = Some(depth);
                    other_table_columns = false;
                }
                continue;
            }
            TokenKind::RightParen => {
                if referenced_list == Some(depth) {
                    referenced_list = None;
                }
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    break;
                }
                continue;
            }
            TokenKind::Comma if depth == 1 => {
                element_start = true;
                continue;
            }
            TokenKind::End => return None,
            _ => {}
        }
        let first = std::mem::replace(&mut element_start, false);
        if is_word(token, "references") {
            expecting_referenced = true;
            continue;
        }
        if expecting_referenced {
            expecting_referenced = false;
            other_table_columns = identifier_text(token)
                .is_some_and(|name| !name.eq_ignore_ascii_case(table));
            continue;
        }
        other_table_columns = false;
        let renames = (depth == 1 && first) || (depth >= 2 && referenced_list.is_none());
        if renames && names_column(token, from) {
            spans.push((token.start, token.end));
        }
    }
    if spans.is_empty() {
        return None;
    }
    with_edits(text, spans, &quoted(to))
}

fn renamed_index_column_text(text: &str, from: &str, to: &str) -> Option<String> {
    let tokens = tokenize_in(text, Dialect::Sqlite).ok()?;
    let on = tokens.iter().position(|token| is_word(token, "on"))?;
    let open = on
        + tokens
            .get(on..)?
            .iter()
            .position(|token| token.kind == TokenKind::LeftParen)?;
    let spans: Vec<(usize, usize)> = tokens
        .get(open..)?
        .iter()
        .filter(|token| names_column(token, from))
        .map(|token| (token.start, token.end))
        .collect();
    with_edits(text, spans, &quoted(to))
}

fn dropped_column_text(text: &str, column: &str) -> Option<String> {
    let tokens = tokenize_in(text, Dialect::Sqlite).ok()?;
    let open = tokens
        .iter()
        .position(|token| token.kind == TokenKind::LeftParen)?;
    let mut depth = 0usize;
    let mut elements: Vec<(usize, usize, Option<usize>, Option<usize>)> = Vec::new();
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    let mut comma_before: Option<usize> = None;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenKind::LeftParen => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            TokenKind::Comma if depth == 1 => {
                elements.push((first.take()?, last, comma_before, Some(index)));
                comma_before = Some(index);
                continue;
            }
            TokenKind::RightParen if depth == 1 => {
                elements.push((first.take()?, last, comma_before, None));
                break;
            }
            TokenKind::RightParen => depth = depth.checked_sub(1)?,
            TokenKind::End => return None,
            _ => {}
        }
        if depth >= 1 {
            if first.is_none() {
                first = Some(index);
            }
            last = index;
        }
    }
    let (start, end, before, after) = elements.into_iter().find(|(start, ..)| {
        let token = &tokens[*start];
        names_column(token, column)
            && !TABLE_CONSTRAINT_WORDS
                .iter()
                .any(|word| is_word(token, word))
    })?;
    let (cut_from, cut_to) = match (before, after) {
        (Some(comma), _) => (tokens[comma].start, tokens[end].end),
        (None, Some(comma)) => (tokens[start].start, tokens.get(comma + 1)?.start),
        (None, None) => return None,
    };
    replaced(text, cut_from, cut_to, "")
}

fn added_column_text(text: &str, column: &str) -> Option<String> {
    let tokens = tokenize_in(text, Dialect::Sqlite).ok()?;
    let open = tokens
        .iter()
        .position(|token| token.kind == TokenKind::LeftParen)?;
    let mut depth = 0usize;
    let mut element_first: Option<&Token<'_>> = None;
    let mut previous_end = 0;
    let mut last_column_end = None;
    for token in tokens.get(open..)? {
        match token.kind {
            TokenKind::LeftParen => {
                depth += 1;
                if depth == 1 {
                    previous_end = token.end;
                    continue;
                }
            }
            TokenKind::RightParen | TokenKind::Comma if depth == 1 => {
                if let Some(first) = element_first.take()
                    && !TABLE_CONSTRAINT_WORDS
                        .iter()
                        .any(|word| is_word(first, word))
                {
                    last_column_end = Some(previous_end);
                }
                if token.kind == TokenKind::RightParen {
                    let at = last_column_end?;
                    return Some(format!("{}, {column}{}", text.get(..at)?, text.get(at..)?));
                }
                previous_end = token.end;
                continue;
            }
            TokenKind::RightParen => depth = depth.checked_sub(1)?,
            TokenKind::End => return None,
            _ => {}
        }
        if depth == 1 && element_first.is_none() {
            element_first = Some(token);
        }
        previous_end = token.end;
    }
    None
}

pub(crate) fn declared_column_types(text: &str) -> Vec<(String, String)> {
    column_declarations(text).unwrap_or_default()
}

fn column_declarations(text: &str) -> Option<Vec<(String, String)>> {
    let tokens = tokenize_in(text, Dialect::Sqlite).ok()?;
    let open = tokens
        .iter()
        .position(|token| token.kind == TokenKind::LeftParen)?;
    if tokens.get(..open)?.iter().any(|token| is_word(token, "as")) {
        return None;
    }
    let mut declared = Vec::new();
    let mut depth = 0usize;
    let mut element: Vec<&Token<'_>> = Vec::new();
    for token in tokens.get(open + 1..)? {
        match token.kind {
            TokenKind::RightParen | TokenKind::Comma if depth == 0 => {
                declared.extend(declared_column(text, &element));
                element.clear();
                if token.kind == TokenKind::RightParen {
                    return Some(declared);
                }
                continue;
            }
            TokenKind::LeftParen => depth += 1,
            TokenKind::RightParen => depth = depth.checked_sub(1)?,
            TokenKind::End => return None,
            _ => {}
        }
        element.push(token);
    }
    None
}

fn is_type_word(token: &Token<'_>) -> bool {
    matches!(&token.kind, TokenKind::Word(word)
        if !WORDS_AFTER_A_TYPE.iter().any(|stop| word.eq_ignore_ascii_case(stop)))
}

fn declared_column(text: &str, element: &[&Token<'_>]) -> Option<(String, String)> {
    let (name, rest) = element.split_first()?;
    if TABLE_CONSTRAINT_WORDS
        .iter()
        .any(|word| is_word(name, word))
    {
        return None;
    }
    let name = match &name.kind {
        TokenKind::Word(word) | TokenKind::String(word) => word.to_string(),
        TokenKind::QuotedIdentifier(quoted) => quoted.clone(),
        _ => return None,
    };
    let words = rest.iter().take_while(|token| is_type_word(token)).count();
    let Some(last_word) = words.checked_sub(1).and_then(|at| rest.get(at)) else {
        return Some((name, String::new()));
    };
    let mut end = last_word.end;
    if rest
        .get(words)
        .is_some_and(|token| token.kind == TokenKind::LeftParen)
    {
        let after = rest.get(words..)?;
        let close = after
            .iter()
            .position(|token| token.kind == TokenKind::RightParen)?;
        end = after.get(close)?.end;
    }
    let start = rest.first()?.start;
    Some((name, text.get(start..end)?.to_owned()))
}

/// A SQLite trigger's text the way `sqlite_schema` stores it: `IF NOT EXISTS` and a schema
/// before the trigger's name left out, everything else as written. Text that does not start
/// with `CREATE TRIGGER` is returned unchanged.
pub(crate) fn stored_trigger_text(text: &str) -> String {
    let Ok(tokens) = tokenize_in(text, Dialect::Sqlite) else {
        return text.to_owned();
    };
    let starts = tokens.first().is_some_and(|token| is_word(token, "create"))
        && tokens.get(1).is_some_and(|token| is_word(token, "trigger"));
    if !starts {
        return text.to_owned();
    }
    let mut at = 2;
    if tokens.get(at).is_some_and(|token| is_word(token, "if"))
        && tokens
            .get(at + 1)
            .is_some_and(|token| is_word(token, "not"))
        && tokens
            .get(at + 2)
            .is_some_and(|token| is_word(token, "exists"))
    {
        at += 3;
    }
    if tokens
        .get(at + 1)
        .is_some_and(|token| token.kind == TokenKind::Dot)
    {
        at += 2;
    }
    match tokens.get(at).filter(|token| is_name(token)) {
        Some(name) => format!("CREATE TRIGGER {}", text.get(name.start..).unwrap_or("")),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Unreadable, added_column_text, decode, encode, index_key, kept_original, owning_relation,
        renamed_index_text, renamed_table_text, stored_trigger_text, text_key,
    };

    #[test]
    fn a_record_keeps_both_texts() {
        let record = encode("CREATE TABLE t (a INTEGER)", "CREATE TABLE t(a int)");
        assert_eq!(
            record.as_deref().map(decode),
            Some(Ok(("CREATE TABLE t (a INTEGER)", "CREATE TABLE t(a int)")))
        );
    }

    #[test]
    fn a_damaged_record_is_refused_without_a_panic() {
        for damaged in [
            &[][..],
            &[0, 0, 0, 0, 0][..],
            &[1, 9, 0, 0][..],
            &[1, 9, 0, 0, 0, b'a'][..],
            &[1, 1, 0, 0, 0, 0xFF][..],
        ] {
            assert_eq!(decode(damaged), Err(Unreadable::Damaged));
        }
        assert_eq!(decode(&[2, 0, 0, 0, 0]), Err(Unreadable::NewerVersion(2)));
    }

    #[test]
    fn a_damaged_record_falls_back_to_the_generated_text_and_a_newer_one_is_named() {
        let generated = "CREATE TABLE t (a INTEGER)";
        assert_eq!(kept_original(&[1, 9, 0, 0], generated).unwrap(), None);
        let record = encode(generated, "CREATE TABLE t(a int)").unwrap();
        assert_eq!(
            kept_original(&record, generated).unwrap().as_deref(),
            Some("CREATE TABLE t(a int)")
        );
        let error = kept_original(&[7, 0, 0, 0, 0], generated).unwrap_err();
        assert_eq!(error.state().code(), "0A000");
        assert!(error.message().contains("format version 7"), "{error}");
    }

    #[test]
    fn an_index_key_belongs_to_its_table_and_never_meets_a_qualified_table_name() {
        let index = index_key("pets", "by_name");
        let space = text_key("").len();
        assert_eq!(owning_relation(&index[space..]), b"pets");
        let qualified = text_key("pets\0by_name");
        assert_ne!(index, qualified);
        assert_eq!(owning_relation(&qualified[space..]), b"pets\0by_name");
    }

    #[test]
    fn a_rename_quotes_the_new_name_where_the_old_one_was_written() {
        assert_eq!(
            renamed_table_text("CREATE TABLE pets(id integer, \"pets\" text)", "animals")
                .as_deref(),
            Some("CREATE TABLE \"animals\"(id integer, \"pets\" text)")
        );
        assert_eq!(
            renamed_table_text("CREATE TABLE \"my pets\" (id)", "a\"b").as_deref(),
            Some("CREATE TABLE \"a\"\"b\" (id)")
        );
        assert_eq!(
            renamed_index_text("CREATE INDEX on_pets ON pets (name)", "animals").as_deref(),
            Some("CREATE INDEX on_pets ON \"animals\" (name)")
        );
        assert_eq!(renamed_table_text("CREATE", "x"), None);
    }

    #[test]
    fn an_added_column_goes_after_the_last_column_and_before_table_constraints() {
        assert_eq!(
            added_column_text("CREATE TABLE t(a int)", "b text").as_deref(),
            Some("CREATE TABLE t(a int, b text)")
        );
        assert_eq!(
            added_column_text(
                "CREATE TABLE t(a int CHECK (a > 0), b, PRIMARY KEY (a, b)) WITHOUT ROWID",
                "c DEFAULT (1)"
            )
            .as_deref(),
            Some(
                "CREATE TABLE t(a int CHECK (a > 0), b, c DEFAULT (1), PRIMARY KEY (a, b)) WITHOUT ROWID"
            )
        );
        assert_eq!(
            added_column_text("CREATE TABLE t(a int,\n  b text -- note\n)", "c").as_deref(),
            Some("CREATE TABLE t(a int,\n  b text, c -- note\n)")
        );
        assert_eq!(added_column_text("CREATE TABLE t(a int", "b"), None);
        assert_eq!(added_column_text("CREATE TABLE t AS SELECT 1", "b"), None);
    }

    #[test]
    fn a_trigger_text_leaves_out_if_not_exists_and_the_schema() {
        assert_eq!(
            stored_trigger_text(
                "CREATE TRIGGER IF NOT EXISTS main.tr AFTER INSERT ON t BEGIN SELECT 1; END"
            ),
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END"
        );
        assert_eq!(
            stored_trigger_text("create  trigger tr after insert on t begin select 1; end"),
            "CREATE TRIGGER tr after insert on t begin select 1; end"
        );
        assert_eq!(stored_trigger_text("not a trigger"), "not a trigger");
    }
}
