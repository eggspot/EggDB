use eggdb_sql::Value;
use std::time::Duration;

pub(super) const SOURCE: &str = "source";
pub(super) const IDENTITY: &str = "identity";
pub(super) const SLOT: &str = "slot";
pub(super) const PUBLICATION: &str = "publication";
pub(super) const PUBLICATION_CREATED: &str = "publication_created";
pub(super) const LSN: &str = "lsn";
pub(super) const POSTGRES: &str = "postgres";
pub(super) const SQL_SERVER: &str = "sqlserver";
pub(super) const READ_STATEMENT: &str = "SELECT item, value FROM eggdb_follow.state";
pub(super) const DEFAULT_RECONNECT_SECONDS: u64 = 300;
const TABLE_PREFIX: &str = "table ";
const LONGEST_RECONNECT_DELAY_SECONDS: u64 = 60;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SavedFollow {
    items: Vec<(String, String)>,
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

pub(super) fn table_item(table: &str) -> String {
    format!("{TABLE_PREFIX}{table}")
}

pub(super) fn update_statement(item: &str, value: &str) -> String {
    format!(
        "UPDATE eggdb_follow.state SET value = {} WHERE item = {}",
        literal(value),
        literal(item)
    )
}

impl SavedFollow {
    pub(super) fn new(source: &str, identity: &str) -> Self {
        let mut saved = Self::default();
        saved.set(SOURCE, source);
        saved.set(IDENTITY, identity);
        saved
    }

    pub(super) fn from_rows(rows: Vec<Vec<Value>>) -> Self {
        let items = rows
            .into_iter()
            .filter_map(|row| {
                let mut values = row.into_iter();
                match (values.next(), values.next()) {
                    (Some(Value::Text(item)), Some(Value::Text(value))) => Some((item, value)),
                    _ => None,
                }
            })
            .collect();
        Self { items }
    }

    pub(super) fn set(&mut self, item: &str, value: &str) {
        match self.items.iter_mut().find(|(name, _)| name == item) {
            Some((_, old)) => *old = value.to_owned(),
            None => self.items.push((item.to_owned(), value.to_owned())),
        }
    }

    pub(super) fn get(&self, item: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|(name, _)| name == item)
            .map(|(_, value)| value.as_str())
    }

    pub(super) fn table(&self, table: &str) -> Option<&str> {
        self.get(&table_item(table))
    }

    pub(super) fn create_statements(&self) -> Vec<String> {
        let mut statements = vec![
            "CREATE SCHEMA IF NOT EXISTS eggdb_follow".to_owned(),
            "CREATE TABLE eggdb_follow.state (item varchar(1000) PRIMARY KEY, value varchar(4000) NOT NULL)"
                .to_owned(),
        ];
        statements.extend(self.items.iter().map(|(item, value)| {
            format!(
                "INSERT INTO eggdb_follow.state (item, value) VALUES ({}, {})",
                literal(item),
                literal(value)
            )
        }));
        statements
    }

    pub(super) fn refusal(&self, source: &str, identity: &str, file: &str) -> Option<String> {
        let saved_source = self.get(SOURCE).unwrap_or_default();
        if saved_source != source {
            return Some(format!(
                "{file} was filled by a follow from {}, not from this {} source; follow into a new file with --create",
                shown_source(saved_source),
                shown_source(source)
            ));
        }
        let saved_identity = self.get(IDENTITY).unwrap_or_default();
        (saved_identity != identity).then(|| {
            format!(
                "{file} follows another database ({saved_identity}), not this one ({identity}); resume it from that source, or follow into a new file with --create"
            )
        })
    }
}

fn shown_source(source: &str) -> &str {
    match source {
        POSTGRES => "PostgreSQL",
        SQL_SERVER => "SQL Server",
        _ => "an unknown source",
    }
}

pub(super) fn reconnect_delay(attempt: u32) -> Duration {
    let doubled = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_secs(doubled.min(LONGEST_RECONNECT_DELAY_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_follow_is_written_as_rows_and_read_back() {
        let mut saved = SavedFollow::new(POSTGRES, "system 7 database app");
        saved.set(LSN, "0/16B3748");
        saved.set(LSN, "0/16B3750");
        saved.set(&table_item("dbo.o'brien"), "change_tracking 12");
        let statements = saved.create_statements();
        assert_eq!(statements.len(), 2 + 4);
        assert!(statements[5].contains("'table dbo.o''brien'"));
        let rows = vec![
            vec![Value::Text(SOURCE.to_owned()), Value::Text(POSTGRES.to_owned())],
            vec![Value::Text(LSN.to_owned()), Value::Text("0/16B3750".to_owned())],
            vec![Value::Null, Value::Text("ignored".to_owned())],
        ];
        let read = SavedFollow::from_rows(rows);
        assert_eq!(read.get(LSN), Some("0/16B3750"));
        assert_eq!(read.get(SLOT), None);
        assert_eq!(saved.table("dbo.o'brien"), Some("change_tracking 12"));
    }

    #[test]
    fn a_position_update_is_one_statement() {
        assert_eq!(
            update_statement(LSN, "0/1"),
            "UPDATE eggdb_follow.state SET value = '0/1' WHERE item = 'lsn'"
        );
    }

    #[test]
    fn another_source_or_database_is_refused() {
        let saved = SavedFollow::new(POSTGRES, "system 7 database app");
        assert_eq!(saved.refusal(POSTGRES, "system 7 database app", "f.db"), None);
        assert!(
            saved
                .refusal(POSTGRES, "system 8 database app", "f.db")
                .unwrap()
                .contains("follows another database")
        );
        assert!(
            saved
                .refusal(SQL_SERVER, "server s database app", "f.db")
                .unwrap()
                .contains("from PostgreSQL, not from this SQL Server source")
        );
    }

    #[test]
    fn reconnect_delays_double_up_to_a_minute() {
        let seconds: Vec<u64> = (1..=9).map(|attempt| reconnect_delay(attempt).as_secs()).collect();
        assert_eq!(seconds, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(reconnect_delay(200).as_secs(), 60);
    }
}
