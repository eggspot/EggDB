use crate::ast::{ExtendedStatistics, Name};
use crate::encoding::{NAMESPACE, TableDefinition, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{Value, equality_key};
use std::collections::HashMap;

const EXTENDED_STATISTICS_SPACE: u8 = 0x1A;
const RECORD_VERSION: u8 = 1;
pub(crate) const KIND_NDISTINCT: u8 = 1;
pub(crate) const KIND_DEPENDENCIES: u8 = 2;
const MOST_COLUMNS: usize = 8;

/// One `CREATE STATISTICS` object (D1137): its table, columns and kinds, and what the last
/// `ANALYZE` of the table found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExtendedObject {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) columns: Vec<String>,
    pub(crate) kinds: u8,
    pub(crate) distinct: Option<f64>,
    pub(crate) dependencies: Vec<(usize, usize, f64)>,
}

fn key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(EXTENDED_STATISTICS_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

fn push_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn encode(object: &ExtendedObject) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION, object.kinds];
    push_text(&mut out, &object.table);
    out.push(u8::try_from(object.columns.len()).unwrap_or(u8::MAX));
    for column in &object.columns {
        push_text(&mut out, column);
    }
    out.extend_from_slice(&object.distinct.unwrap_or(-1.0).to_le_bytes());
    out.push(u8::try_from(object.dependencies.len()).unwrap_or(u8::MAX));
    for (from, to, degree) in &object.dependencies {
        out.push(u8::try_from(*from).unwrap_or(u8::MAX));
        out.push(u8::try_from(*to).unwrap_or(u8::MAX));
        out.extend_from_slice(&degree.to_le_bytes());
    }
    out
}

struct Reader<'b> {
    bytes: &'b [u8],
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> SqlResult<&'b [u8]> {
        if self.bytes.len() < count {
            return Err(crate::encoding::corrupt("extended statistics"));
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(taken)
    }

    fn byte(&mut self) -> SqlResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn text(&mut self) -> SqlResult<String> {
        let mut word = [0u8; 4];
        word.copy_from_slice(self.take(4)?);
        let length = usize::try_from(u32::from_le_bytes(word)).unwrap_or(usize::MAX);
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| crate::encoding::corrupt("extended statistics"))
    }

    fn number(&mut self) -> SqlResult<f64> {
        let mut word = [0u8; 8];
        word.copy_from_slice(self.take(8)?);
        Ok(f64::from_le_bytes(word))
    }
}

fn decode(name: &str, bytes: &[u8]) -> SqlResult<ExtendedObject> {
    let mut reader = Reader { bytes };
    if reader.byte()? != RECORD_VERSION {
        return Err(crate::encoding::corrupt("extended statistics"));
    }
    let kinds = reader.byte()?;
    let table = reader.text()?;
    let count = usize::from(reader.byte()?);
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        columns.push(reader.text()?);
    }
    let distinct = Some(reader.number()?).filter(|value| *value >= 0.0);
    let pairs = usize::from(reader.byte()?);
    let mut dependencies = Vec::with_capacity(pairs);
    for _ in 0..pairs {
        let from = usize::from(reader.byte()?);
        let to = usize::from(reader.byte()?);
        dependencies.push((from, to, reader.number()?));
    }
    Ok(ExtendedObject {
        name: name.to_owned(),
        table,
        columns,
        kinds,
        distinct,
        dependencies,
    })
}

/// Every extended statistics object, by the table it describes.
pub(crate) fn every(reader: &dyn KeyValueRead) -> SqlResult<HashMap<String, Vec<ExtendedObject>>> {
    let prefix = key("");
    let mut every: HashMap<String, Vec<ExtendedObject>> = HashMap::new();
    let mut failure = None;
    reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |stored, value| {
        let name = String::from_utf8_lossy(&stored[prefix.len()..]).into_owned();
        match decode(&name, value) {
            Ok(object) => {
                every.entry(object.table.clone()).or_default().push(object);
                Ok(true)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        }
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(every),
    }
}

fn duplicate(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_OBJECT,
        format!("statistics object \"{}\" already exists", name.text),
    )
    .at(name.position)
}

/// Runs `CREATE STATISTICS` and `DROP STATISTICS` (D1137).
pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    command: &ExtendedStatistics,
) -> SqlResult<QueryResult> {
    match command {
        ExtendedStatistics::Create {
            name,
            if_not_exists,
            kinds,
            columns,
            table,
        } => {
            if writer.get(&key(&name.text))?.is_some() {
                if *if_not_exists {
                    return Ok(QueryResult::command("CREATE STATISTICS").with_notice(format!(
                        "NOTICE:  statistics object \"{}\" already exists, skipping",
                        name.text
                    )));
                }
                return Err(duplicate(name));
            }
            let definition = tables.require(writer, table)?;
            if columns.len() < 2 {
                return Err(SqlError::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    "extended statistics require at least 2 columns",
                )
                .at(name.position));
            }
            if columns.len() > MOST_COLUMNS {
                return Err(SqlError::new(
                    SqlState::TOO_MANY_COLUMNS,
                    format!("cannot have more than {MOST_COLUMNS} columns in statistics"),
                )
                .at(name.position));
            }
            for column in columns {
                if definition.column_index(&column.text).is_none() {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!("column \"{}\" does not exist", column.text),
                    )
                    .at(column.position));
                }
            }
            let object = ExtendedObject {
                name: name.text.clone(),
                table: definition.name.clone(),
                columns: columns.iter().map(|column| column.text.clone()).collect(),
                kinds: if *kinds == 0 { KIND_NDISTINCT | KIND_DEPENDENCIES } else { *kinds },
                distinct: None,
                dependencies: Vec::new(),
            };
            writer.put(&key(&name.text), &encode(&object))?;
            Ok(QueryResult::command("CREATE STATISTICS"))
        }
        ExtendedStatistics::Drop { names, if_exists } => {
            let mut result = QueryResult::command("DROP STATISTICS");
            for name in names {
                if !writer.delete(&key(&name.text))? {
                    if *if_exists {
                        result = result.with_notice(format!(
                            "NOTICE:  statistics object \"{}\" does not exist, skipping",
                            name.text
                        ));
                        continue;
                    }
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("statistics object \"{}\" does not exist", name.text),
                    )
                    .at(name.position));
                }
            }
            Ok(result)
        }
    }
}

fn tuple_key(row: &[Value], positions: &[usize]) -> Vec<u8> {
    let mut key = Vec::new();
    for &at in positions {
        if let Some(value) = row.get(at) {
            equality_key(&mut key, value);
            key.push(0xFF);
        }
    }
    key
}

/// The share of rows in which column `from` decides column `to`: the rows of every group of
/// equal `from` values that all hold one `to` value, over all rows, as PostgreSQL measures a
/// functional dependency.
pub(crate) fn dependency_degree(rows: &[&[Value]], from: usize, to: usize) -> f64 {
    if rows.is_empty() {
        return 0.0;
    }
    let mut groups: HashMap<Vec<u8>, (Vec<u8>, usize, bool)> = HashMap::new();
    for row in rows {
        let group = tuple_key(row, &[from]);
        let value = tuple_key(row, &[to]);
        let entry = groups.entry(group).or_insert_with(|| (value.clone(), 0, true));
        entry.1 += 1;
        if entry.0 != value {
            entry.2 = false;
        }
    }
    let decided: usize = groups.values().filter(|(_, _, single)| *single).map(|(_, count, _)| *count).sum();
    decided as f64 / rows.len() as f64
}

/// Recomputes the extended statistics of `table` from `rows`, a sample of its rows in column
/// order, as part of `ANALYZE`.
pub(crate) fn analyze_table(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    rows: &[&[Value]],
) -> SqlResult<()> {
    let objects = every(writer)?.remove(&table.name).unwrap_or_default();
    for mut object in objects {
        let positions: Vec<usize> = object
            .columns
            .iter()
            .filter_map(|column| table.column_index(column))
            .collect();
        if positions.len() != object.columns.len() {
            continue;
        }
        if object.kinds & KIND_NDISTINCT != 0 {
            let mut seen = std::collections::HashSet::new();
            for row in rows {
                seen.insert(tuple_key(row, &positions));
            }
            object.distinct = Some(seen.len() as f64);
        }
        object.dependencies.clear();
        if object.kinds & KIND_DEPENDENCIES != 0 {
            for (from_slot, &from) in positions.iter().enumerate() {
                for (to_slot, &to) in positions.iter().enumerate() {
                    if from_slot != to_slot {
                        object
                            .dependencies
                            .push((from_slot, to_slot, dependency_degree(rows, from, to)));
                    }
                }
            }
        }
        writer.put(&key(&object.name), &encode(&object))?;
    }
    Ok(())
}

/// Forgets the extended statistics of a dropped table.
pub(crate) fn forget_table(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    for object in every(writer)?.remove(table).unwrap_or_default() {
        writer.delete(&key(&object.name))?;
    }
    Ok(())
}

/// The combined selectivity of equalities on several columns of one table, given each
/// column's own selectivity: PostgreSQL's dependency formula
/// `P(a, b) = P(a) * (f + (1 - f) * P(b))` for the strongest dependency `a -> b` an object
/// records between two of the columns, the rest multiplied in as independent.
pub(crate) fn combined_selectivity(objects: &[ExtendedObject], equalities: &[(String, f64)]) -> f64 {
    let independent: f64 = equalities.iter().map(|(_, share)| share).product();
    let mut best = independent;
    for object in objects {
        for (from, to, degree) in &object.dependencies {
            let (Some(from_name), Some(to_name)) = (object.columns.get(*from), object.columns.get(*to)) else {
                continue;
            };
            let share_of = |name: &str| equalities.iter().find(|(column, _)| column == name).map(|(_, share)| *share);
            let (Some(from_share), Some(to_share)) = (share_of(from_name), share_of(to_name)) else {
                continue;
            };
            let rest: f64 = equalities
                .iter()
                .filter(|(column, _)| column != from_name && column != to_name)
                .map(|(_, share)| share)
                .product();
            let combined = from_share * (degree + (1.0 - degree) * to_share) * rest;
            if combined > best {
                best = combined;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::{ExtendedObject, combined_selectivity, decode, dependency_degree, encode};
    use crate::types::Value;

    #[test]
    fn a_city_decides_its_country_but_not_the_other_way() {
        let rows: Vec<Vec<Value>> = vec![
            vec![Value::Text("paris".into()), Value::Text("fr".into())],
            vec![Value::Text("paris".into()), Value::Text("fr".into())],
            vec![Value::Text("lyon".into()), Value::Text("fr".into())],
            vec![Value::Text("rome".into()), Value::Text("it".into())],
        ];
        let rows: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
        assert!((dependency_degree(&rows, 0, 1) - 1.0).abs() < 1e-9);
        assert!((dependency_degree(&rows, 1, 0) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_full_dependency_keeps_the_stronger_selectivity_and_records_round_trip() {
        let object = ExtendedObject {
            name: "s".into(),
            table: "t".into(),
            columns: vec!["city".into(), "country".into()],
            kinds: 3,
            distinct: Some(3.0),
            dependencies: vec![(0, 1, 1.0), (1, 0, 0.25)],
        };
        let combined = combined_selectivity(
            std::slice::from_ref(&object),
            &[("city".into(), 0.01), ("country".into(), 0.2)],
        );
        assert!((combined - 0.01).abs() < 1e-12);
        assert_eq!(decode("s", &encode(&object)).unwrap(), object);
    }
}
