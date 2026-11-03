use super::{
    GRANT_SPACE, PUBLIC, Privilege, acl_text, grant_key, grant_prefix, missing_role,
    permission_denied, role_closure, stored_role, table_level_held,
};
use crate::ast::Name;
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{Relation, TableCache};
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use std::collections::BTreeSet;

pub(crate) const COLUMN_RECORD_VERSION: u8 = 2;

pub(crate) const COLUMN_PRIVILEGES: [Privilege; 4] = [
    Privilege::Select,
    Privilege::Insert,
    Privilege::Update,
    Privilege::References,
];

fn column_grant_key(table: &str, grantee: &str, column: &str) -> Vec<u8> {
    let mut key = grant_key(table, grantee);
    key.push(0);
    key.extend_from_slice(column.as_bytes());
    key
}

struct StoredColumnGrant {
    key: Vec<u8>,
    grantee: String,
    column: String,
    privileges: u8,
}

fn damaged_grant() -> SqlError {
    crate::encoding::corrupt("grant")
}

fn column_grants_of(reader: &dyn KeyValueRead, table: &str) -> SqlResult<Vec<StoredColumnGrant>> {
    let prefix = grant_prefix(table);
    let mut found = Vec::new();
    let mut damaged = false;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let rest = key.get(prefix.len()..).unwrap_or_default();
            let Some(split) = rest.iter().position(|byte| *byte == 0) else {
                return Ok(true);
            };
            let (Some(grantee), Some(column)) = (rest.get(..split), rest.get(split + 1..)) else {
                return Ok(true);
            };
            let (Ok(grantee), Ok(column), [COLUMN_RECORD_VERSION, privileges]) = (
                std::str::from_utf8(grantee),
                std::str::from_utf8(column),
                value,
            ) else {
                damaged = true;
                return Ok(false);
            };
            found.push(StoredColumnGrant {
                key: key.to_vec(),
                grantee: grantee.to_owned(),
                column: column.to_owned(),
                privileges: *privileges,
            });
            Ok(true)
        },
    )?;
    if damaged {
        return Err(damaged_grant());
    }
    Ok(found)
}

/// The columns of one relation a session may use with one privilege: every column when the
/// session holds the privilege on the whole relation, or only the columns granted one by one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnReach {
    Every,
    Only(BTreeSet<String>),
}

impl ColumnReach {
    pub(crate) fn allows(&self, column: &str) -> bool {
        match self {
            Self::Every => true,
            Self::Only(columns) => columns.contains(column),
        }
    }
}

/// What the session may do with `privilege` on the columns of `table`. Fails with `42501` when
/// it holds the privilege neither on the table nor on any of its columns.
pub(crate) fn column_reach(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    privilege: Privilege,
) -> SqlResult<ColumnReach> {
    let Some(user) = settings.checked_role.as_deref() else {
        return Ok(ColumnReach::Every);
    };
    if crate::temporary::is_temporary_name(table) {
        return Ok(ColumnReach::Every);
    }
    let held = role_closure(reader, user)?;
    if table_level_held(reader, &held, table, privilege)? {
        return Ok(ColumnReach::Every);
    }
    let granted: BTreeSet<String> = column_grants_of(reader, table)?
        .into_iter()
        .filter(|grant| held.contains(&grant.grantee) && grant.privileges & privilege.bit() != 0)
        .map(|grant| grant.column)
        .collect();
    if granted.is_empty() {
        return Err(permission_denied(table));
    }
    Ok(ColumnReach::Only(granted))
}

/// Checks that the session may use `privilege` on each of `columns` of `table`. An empty list
/// passes when the session holds the privilege on the table or on any one of its columns, as
/// `INSERT ... DEFAULT VALUES` and `SELECT count(*)` do on PostgreSQL.
pub(crate) fn check_columns<'c>(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    privilege: Privilege,
    columns: impl IntoIterator<Item = &'c str>,
) -> SqlResult<()> {
    let reach = column_reach(reader, settings, table, privilege)?;
    for column in columns {
        if !reach.allows(column) {
            return Err(permission_denied(table));
        }
    }
    Ok(())
}

pub(crate) fn relation_columns(
    reader: &dyn KeyValueRead,
    table: &str,
) -> SqlResult<Option<Vec<String>>> {
    let mut cache = TableCache::default();
    Ok(match cache.relation(reader, table)? {
        Some(Relation::Table(definition)) => Some(
            definition
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
        ),
        Some(Relation::View(view)) => Some(
            view.columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
        ),
        None => None,
    })
}

fn invalid_column_privilege(privilege: Privilege) -> SqlError {
    SqlError::new(
        SqlState::INVALID_GRANT_OPERATION,
        format!(
            "invalid privilege type {} for column",
            privilege.keyword().to_ascii_uppercase()
        ),
    )
}

pub(super) fn change_column_privileges(
    writer: &mut dyn KeyValueWrite,
    granting: bool,
    columns: &[(Privilege, Vec<Name>)],
    tables: &[Name],
    grantees: &[String],
) -> SqlResult<()> {
    for (privilege, _) in columns {
        if !COLUMN_PRIVILEGES.contains(privilege) {
            return Err(invalid_column_privilege(*privilege));
        }
    }
    for table in tables {
        let Some(existing) = relation_columns(writer, &table.text)? else {
            return Err(SqlError::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", table.text),
            )
            .at(table.position));
        };
        for (_, names) in columns {
            for name in names {
                if !existing.contains(&name.text) {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            name.text,
                            crate::schemas::display(&table.text)
                        ),
                    )
                    .at(name.position));
                }
            }
        }
        for grantee in grantees {
            if grantee != PUBLIC && stored_role(writer, grantee)?.is_none() {
                return Err(missing_role(grantee));
            }
            for (privilege, names) in columns {
                for name in names {
                    let key = column_grant_key(&table.text, grantee, &name.text);
                    let current = match writer.get(&key)?.as_deref() {
                        Some([COLUMN_RECORD_VERSION, held]) => *held,
                        Some(_) => return Err(damaged_grant()),
                        None => 0,
                    };
                    let changed = if granting {
                        current | privilege.bit()
                    } else {
                        current & !privilege.bit()
                    };
                    if changed == 0 {
                        writer.delete(&key)?;
                    } else {
                        writer.put(&key, &[COLUMN_RECORD_VERSION, changed])?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn revoke_from_every_column(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    grantee: &str,
    bits: u8,
) -> SqlResult<()> {
    for grant in column_grants_of(writer, table)? {
        if grant.grantee != grantee {
            continue;
        }
        let changed = grant.privileges & !bits;
        if changed == 0 {
            writer.delete(&grant.key)?;
        } else {
            writer.put(&grant.key, &[COLUMN_RECORD_VERSION, changed])?;
        }
    }
    Ok(())
}

/// Removes the column grants on `column` of `table`, when the column is dropped.
pub(crate) fn forget_column(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    column: &str,
) -> SqlResult<()> {
    for grant in column_grants_of(writer, table)? {
        if grant.column == column {
            writer.delete(&grant.key)?;
        }
    }
    Ok(())
}

/// Moves the column grants on `from` of `table` to `to`, when the column is renamed.
pub(crate) fn move_column(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    from: &str,
    to: &str,
) -> SqlResult<()> {
    if from == to {
        return Ok(());
    }
    for grant in column_grants_of(writer, table)? {
        if grant.column == from {
            writer.delete(&grant.key)?;
            writer.put(
                &column_grant_key(table, &grant.grantee, to),
                &[COLUMN_RECORD_VERSION, grant.privileges],
            )?;
        }
    }
    Ok(())
}

/// Moves every grant on `from`, on the table and on its columns, to `to`, when the table is
/// renamed.
pub(crate) fn move_table(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    if from == to {
        return Ok(());
    }
    let old = grant_prefix(from);
    let mut moved = Vec::new();
    writer.scan(
        &old,
        &prefix_end(&old),
        ScanDirection::Forward,
        &mut |key, value| {
            moved.push((key.to_vec(), value.to_vec()));
            Ok(true)
        },
    )?;
    let new = grant_prefix(to);
    for (key, value) in moved {
        let Some(rest) = key.get(old.len()..) else {
            continue;
        };
        let mut renamed = new.clone();
        renamed.extend_from_slice(rest);
        writer.delete(&key)?;
        writer.put(&renamed, &value)?;
    }
    Ok(())
}

/// One grantee's privileges on one column, as `GRANT ... (column)` stored them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnGrant {
    pub(crate) table: String,
    pub(crate) grantee: String,
    pub(crate) column: String,
    pub(crate) privileges: u8,
}

impl ColumnGrant {
    /// The grant in PostgreSQL's `aclitem` text form, as `pg_attribute.attacl` lists it.
    pub(crate) fn acl_item(&self, grantor: &str) -> String {
        acl_text(&self.grantee, self.privileges, grantor)
    }

    pub(crate) fn holds(&self, privilege: Privilege) -> bool {
        self.privileges & privilege.bit() != 0
    }
}

/// Every stored column grant, in table, grantee and then column order.
pub(crate) fn every_column_grant(reader: &dyn KeyValueRead) -> SqlResult<Vec<ColumnGrant>> {
    let mut prefix = NAMESPACE.to_vec();
    prefix.push(GRANT_SPACE);
    let mut grants = Vec::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let rest = key.get(prefix.len()..).unwrap_or_default();
            let mut parts = rest.splitn(3, |byte| *byte == 0);
            if let (Some(table), Some(grantee), Some(column)) =
                (parts.next(), parts.next(), parts.next())
                && let [COLUMN_RECORD_VERSION, held] = value
            {
                grants.push(ColumnGrant {
                    table: String::from_utf8_lossy(table).into_owned(),
                    grantee: String::from_utf8_lossy(grantee).into_owned(),
                    column: String::from_utf8_lossy(column).into_owned(),
                    privileges: *held,
                });
            }
            Ok(true)
        },
    )?;
    Ok(grants)
}

#[cfg(test)]
mod tests;
