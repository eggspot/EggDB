//! Settings a role gets when it logs in (D1313): PostgreSQL's `ALTER ROLE ... SET`, kept per
//! role and database as `pg_db_role_setting` keeps them.

use super::{RECORD_VERSION, missing_role, stored_role};
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};

pub(crate) const ROLE_SETTING_SPACE: u8 = 0x23;
const MAX_SETTINGS_PER_PAIR: usize = 256;

/// What `ALTER ROLE ... SET` or `RESET` changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingChange {
    Set { name: String, value: String },
    Reset { name: String },
    ResetAll,
}

/// The role (`None` for `ALTER ROLE ALL`), the database (`None` for every database) and the
/// change of one `ALTER ROLE ... SET`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleSettingStatement {
    pub(crate) role: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) change: SettingChange,
}

fn pair_prefix(role: Option<&str>, database: Option<&str>) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(ROLE_SETTING_SPACE);
    key.extend_from_slice(role.unwrap_or_default().as_bytes());
    key.push(0);
    key.extend_from_slice(database.unwrap_or_default().as_bytes());
    key.push(0);
    key
}

fn setting_key(role: Option<&str>, database: Option<&str>, name: &str) -> Vec<u8> {
    let mut key = pair_prefix(role, database);
    key.extend_from_slice(name.as_bytes());
    key
}

/// The settings stored for one role and database pair, in name order.
pub(crate) fn settings_of(
    reader: &dyn KeyValueRead,
    role: Option<&str>,
    database: Option<&str>,
) -> SqlResult<Vec<(String, String)>> {
    let prefix = pair_prefix(role, database);
    let mut found = Vec::new();
    reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |key, value| {
        let name = std::str::from_utf8(&key[prefix.len()..]).map_err(|_| crate::encoding::corrupt("role setting"))?;
        let [RECORD_VERSION, text @ ..] = value else {
            return Err(crate::encoding::corrupt("role setting"));
        };
        let text = std::str::from_utf8(text).map_err(|_| crate::encoding::corrupt("role setting"))?;
        found.push((name.to_owned(), text.to_owned()));
        Ok(found.len() < MAX_SETTINGS_PER_PAIR)
    })?;
    Ok(found)
}

/// Every pair with settings, as `(role, database, settings)` with empty names for "all".
pub(crate) fn every_setting(reader: &dyn KeyValueRead) -> SqlResult<Vec<(String, String, Vec<(String, String)>)>> {
    let mut prefix = NAMESPACE.to_vec();
    prefix.push(ROLE_SETTING_SPACE);
    let mut rows: Vec<(String, String, Vec<(String, String)>)> = Vec::new();
    reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |key, value| {
        let rest = &key[prefix.len()..];
        let mut parts = rest.splitn(3, |byte| *byte == 0);
        let (Some(role), Some(database), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(crate::encoding::corrupt("role setting"));
        };
        let text = |bytes: &[u8]| {
            std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| crate::encoding::corrupt("role setting"))
        };
        let [RECORD_VERSION, stored @ ..] = value else {
            return Err(crate::encoding::corrupt("role setting"));
        };
        let (role, database, name, stored) = (text(role)?, text(database)?, text(name)?, text(stored)?);
        match rows.last_mut() {
            Some((last_role, last_database, settings)) if *last_role == role && *last_database == database => {
                settings.push((name, stored));
            }
            _ => rows.push((role, database, vec![(name, stored)])),
        }
        Ok(true)
    })?;
    Ok(rows)
}

/// Stores `statement`'s change.
pub(crate) fn change(writer: &mut dyn KeyValueWrite, statement: &RoleSettingStatement) -> SqlResult<()> {
    let role = statement.role.as_deref();
    if let Some(name) = role
        && stored_role(writer, name)?.is_none()
    {
        return Err(missing_role(name));
    }
    let database = statement.database.as_deref();
    match &statement.change {
        SettingChange::Set { name, value } => {
            if name.contains('\0') || value.contains('\0') {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "a setting's name and value may not hold NUL",
                ));
            }
            let mut stored = vec![RECORD_VERSION];
            stored.extend_from_slice(value.as_bytes());
            writer.put(&setting_key(role, database, &name.to_ascii_lowercase()), &stored)
        }
        SettingChange::Reset { name } => writer.delete(&setting_key(role, database, &name.to_ascii_lowercase())),
        SettingChange::ResetAll => {
            for (name, _) in settings_of(writer, role, database)? {
                writer.delete(&setting_key(role, database, &name))?;
            }
            Ok(())
        }
    }
}

/// The settings a session of `role` on `database` starts with, in the order PostgreSQL applies
/// them so the most specific wins: every role on the database, then the role everywhere, then
/// the role on the database.
pub(crate) fn login_settings(reader: &dyn KeyValueRead, role: &str, database: &str) -> SqlResult<Vec<(String, String)>> {
    let mut applied = settings_of(reader, None, Some(database))?;
    applied.extend(settings_of(reader, Some(role), None)?);
    applied.extend(settings_of(reader, Some(role), Some(database))?);
    Ok(applied)
}

/// Forgets every setting of `role`, when it is dropped.
pub(crate) fn forget_role(writer: &mut dyn KeyValueWrite, role: &str) -> SqlResult<()> {
    for (stored_role, database, settings) in every_setting(writer)? {
        if stored_role != role {
            continue;
        }
        let database = (!database.is_empty()).then_some(database.as_str());
        for (name, _) in settings {
            writer.delete(&setting_key(Some(role), database, &name))?;
        }
    }
    Ok(())
}
