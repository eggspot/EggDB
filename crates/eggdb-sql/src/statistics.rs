use crate::ast::Name;
use crate::encoding::{NAMESPACE, TableDefinition, prefix_end, rows_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;
use std::collections::HashMap;

const STATISTICS_SPACE: u8 = 0x10;
const RECORD_VERSION: u8 = 1;

fn statistics_key(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(STATISTICS_SPACE);
    key.extend_from_slice(table.as_bytes());
    key
}

fn encode(rows: u64) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    out.extend_from_slice(&rows.to_le_bytes());
    out
}

fn decode(bytes: &[u8]) -> SqlResult<u64> {
    match bytes {
        [RECORD_VERSION, rest @ ..] if rest.len() == 8 => {
            let mut word = [0u8; 8];
            word.copy_from_slice(rest);
            Ok(u64::from_le_bytes(word))
        }
        _ => Err(crate::encoding::corrupt("table statistics")),
    }
}

fn counted_rows(reader: &dyn KeyValueRead, table: &TableDefinition) -> SqlResult<u64> {
    let prefix = rows_prefix(table.id);
    let mut rows = 0u64;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, _| {
            rows += 1;
            Ok(true)
        },
    )?;
    Ok(rows)
}

pub(crate) fn analyze(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    names: &[Name],
) -> SqlResult<QueryResult> {
    let chosen: Vec<TableDefinition> = if names.is_empty() {
        crate::indexes::every_table(writer)?
    } else {
        let mut chosen = Vec::with_capacity(names.len());
        for name in names {
            let Some(table) = tables.table(writer, &name.text)? else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", name.text),
                )
                .at(name.position));
            };
            chosen.push((*table).clone());
        }
        chosen
    };
    let mut analyzed = Vec::with_capacity(chosen.len());
    for table in chosen {
        if crate::temp_tables::is_temp_table(&table.name) {
            continue;
        }
        let rows = counted_rows(writer, &table)?;
        writer.put(&statistics_key(&table.name), &encode(rows))?;
        analyzed.push(table.name);
    }
    if crate::constraints::current_dialect() == crate::Dialect::MySql {
        let columns = ["Table", "Op", "Msg_type", "Msg_text"]
            .iter()
            .map(|name| crate::OutputColumn {
                name: (*name).to_owned(),
                sql_type: Some(crate::types::SqlType::Text),
            })
            .collect();
        let rows = analyzed
            .into_iter()
            .map(|table| {
                vec![
                    Value::Text(table),
                    Value::Text("analyze".to_owned()),
                    Value::Text("status".to_owned()),
                    Value::Text("OK".to_owned()),
                ]
            })
            .collect();
        return Ok(QueryResult::rows("ANALYZE".to_owned(), columns, rows));
    }
    Ok(QueryResult::command("ANALYZE"))
}

pub(crate) fn every_row_count(reader: &dyn KeyValueRead) -> SqlResult<HashMap<String, u64>> {
    let prefix = statistics_key("");
    let mut counts = HashMap::new();
    let mut failure = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let decoded = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| crate::encoding::corrupt("table statistics"))
                .and_then(|table| Ok((table.to_owned(), decode(value)?)));
            match decoded {
                Ok((table, rows)) => {
                    counts.insert(table, rows);
                    Ok(true)
                }
                Err(error) => {
                    failure = Some(error);
                    Ok(false)
                }
            }
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(counts),
    }
}

pub(crate) fn forget(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    writer.delete(&statistics_key(table)).map(|_| ())
}

pub(crate) fn rename(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    if let Some(bytes) = writer.get(&statistics_key(from))? {
        forget(writer, from)?;
        writer.put(&statistics_key(to), &bytes)?;
    }
    Ok(())
}
