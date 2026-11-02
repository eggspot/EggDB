use crate::ast::CreateTable;
use crate::dialect::Dialect;
use crate::encoding::{NAMESPACE, corrupt, table_key};
use crate::error::SqlResult;
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

fn decode(bytes: &[u8]) -> Option<(&str, &str)> {
    let (&version, rest) = bytes.split_first()?;
    if version != RECORD_VERSION {
        return None;
    }
    let (length, rest) = rest.split_first_chunk::<LENGTH_BYTES>()?;
    let length = usize::try_from(u32::from_le_bytes(*length)).ok()?;
    let generated = std::str::from_utf8(rest.get(..length)?).ok()?;
    let original = std::str::from_utf8(rest.get(length..)?).ok()?;
    Some((generated, original))
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
    let (kept_generated, original) = decode(&bytes).ok_or_else(|| corrupt("CREATE TABLE text"))?;
    Ok((kept_generated == generated).then(|| original.to_owned()))
}

pub(crate) fn forget(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    writer.delete(&text_key(table)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn a_record_keeps_both_texts() {
        let record = encode("CREATE TABLE t (a INTEGER)", "CREATE TABLE t(a int)");
        assert_eq!(
            record.as_deref().and_then(decode),
            Some(("CREATE TABLE t (a INTEGER)", "CREATE TABLE t(a int)"))
        );
    }

    #[test]
    fn a_damaged_record_is_refused_without_a_panic() {
        for damaged in [
            &[][..],
            &[2, 0, 0, 0, 0][..],
            &[1, 9, 0, 0][..],
            &[1, 9, 0, 0, 0, b'a'][..],
            &[1, 1, 0, 0, 0, 0xFF][..],
        ] {
            assert_eq!(decode(damaged), None);
        }
    }
}
