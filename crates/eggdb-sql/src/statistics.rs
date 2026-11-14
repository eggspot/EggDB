use crate::ast::Name;
use crate::column_statistics::{ColumnStatistics, EveryColumnStatistics, RowSample};
use crate::encoding::{NAMESPACE, TableDefinition, decode_row, prefix_end, rows_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;
use std::collections::HashMap;

pub(crate) const STATISTICS_SPACE: u8 = 0x10;
const RECORD_VERSION: u8 = 1;
const RECORD_WITH_COLUMNS: u8 = 2;

fn statistics_key(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(STATISTICS_SPACE);
    key.extend_from_slice(table.as_bytes());
    key
}

fn encode(rows: u64, columns: &[ColumnStatistics]) -> Vec<u8> {
    let mut out = vec![RECORD_WITH_COLUMNS];
    out.extend_from_slice(&rows.to_le_bytes());
    crate::column_statistics::encode_into(&mut out, columns);
    out
}

fn split(bytes: &[u8]) -> SqlResult<(u64, &[u8])> {
    let (version, rest) = match bytes {
        [version @ (RECORD_VERSION | RECORD_WITH_COLUMNS), rest @ ..] if rest.len() >= 8 => {
            (*version, rest)
        }
        _ => return Err(crate::encoding::corrupt("table statistics")),
    };
    let (count, columns) = rest.split_at(8);
    if version == RECORD_VERSION && !columns.is_empty() {
        return Err(crate::encoding::corrupt("table statistics"));
    }
    let mut word = [0u8; 8];
    word.copy_from_slice(count);
    Ok((u64::from_le_bytes(word), columns))
}

fn decode(bytes: &[u8]) -> SqlResult<u64> {
    split(bytes).map(|(rows, _)| rows)
}

fn decode_columns(bytes: &[u8]) -> SqlResult<Vec<ColumnStatistics>> {
    let (_, columns) = split(bytes)?;
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    crate::column_statistics::decode(columns)
}

fn sampled_rows(reader: &dyn KeyValueRead, table: &TableDefinition) -> SqlResult<RowSample> {
    let prefix = rows_prefix(table.id);
    let mut sample = RowSample::new(u64::from(table.id).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut failure = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, value| match decode_row(value, &table.columns) {
            Ok(row) => {
                sample.offer(row);
                Ok(true)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(sample),
    }
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
        let sample = sampled_rows(writer, &table)?;
        let columns = sample.statistics(&table.columns);
        writer.put(
            &statistics_key(&table.name),
            &encode(sample.total_rows(), &columns),
        )?;
        crate::extended_statistics::analyze_table(writer, &table, &sample.row_values())?;
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

pub(crate) fn every_column_statistics(
    reader: &dyn KeyValueRead,
) -> SqlResult<EveryColumnStatistics> {
    let prefix = statistics_key("");
    let mut every = HashMap::new();
    let mut failure = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let decoded = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| crate::encoding::corrupt("table statistics"))
                .and_then(|table| Ok((table.to_owned(), decode_columns(value)?)));
            match decoded {
                Ok((table, columns)) => {
                    if !columns.is_empty() {
                        every.insert(table, columns);
                    }
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
        None => Ok(every),
    }
}

pub(crate) fn forget(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    writer.delete(&statistics_key(table))?;
    crate::extended_statistics::forget_table(writer, table)
}

pub(crate) fn rename(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    if let Some(bytes) = writer.get(&statistics_key(from))? {
        forget(writer, from)?;
        writer.put(&statistics_key(to), &bytes)?;
    }
    Ok(())
}
