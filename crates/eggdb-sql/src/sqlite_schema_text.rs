use crate::ast::CreateTable;
use crate::dialect::Dialect;
use crate::encoding::{NAMESPACE, table_key};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite};

pub(crate) const SQLITE_TEXT_SPACE: u8 = 0x15;
const RECORD_VERSION: u8 = 1;
const LENGTH_BYTES: usize = 4;

fn text_key(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(SQLITE_TEXT_SPACE);
    key.extend_from_slice(table.as_bytes());
    key
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
    if !existed
        && let Some(table) = crate::schema::read_table(&*writer, &create.name.text)?
        && let Some(record) = encode(&crate::session::sqlite_create_text(&table), original)
    {
        writer.put(&text_key(&create.name.text), &record)?;
    }
    Ok(result)
}

pub(crate) fn original_text(
    reader: &dyn KeyValueRead,
    table: &str,
    generated: &str,
) -> SqlResult<Option<String>> {
    let Some(bytes) = reader.get(&text_key(table))? else {
        return Ok(None);
    };
    kept_original(&bytes, generated)
}

pub(crate) fn forget(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    writer.delete(&text_key(table)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{Unreadable, decode, encode, kept_original};

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
}
