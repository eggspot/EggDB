use super::{PUBLIC, RECORD_VERSION, missing_role, role_closure, stored_role};
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use std::collections::BTreeSet;

pub(crate) const OBJECT_GRANT_SPACE: u8 = 0x1D;

/// The objects other than tables that `GRANT` stores privileges on (D1190).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Schema,
    Type,
    Parameter,
    Tablespace,
}

impl ObjectKind {
    fn byte(self) -> u8 {
        match self {
            Self::Schema => 1,
            Self::Type => 2,
            Self::Parameter => 3,
            Self::Tablespace => 4,
        }
    }

    /// The privileges this kind of object takes, as PostgreSQL lists them.
    pub(crate) fn privileges(self) -> &'static [ObjectPrivilege] {
        match self {
            Self::Schema => &[ObjectPrivilege::Usage, ObjectPrivilege::Create],
            Self::Type => &[ObjectPrivilege::Usage],
            Self::Parameter => &[ObjectPrivilege::Set, ObjectPrivilege::AlterSystem],
            Self::Tablespace => &[ObjectPrivilege::Create],
        }
    }

    /// Its word in `GRANT ... ON <word>`.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Type => "type",
            Self::Parameter => "parameter",
            Self::Tablespace => "tablespace",
        }
    }
}

/// A privilege on a schema, a type, a parameter or a tablespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectPrivilege {
    Usage,
    Create,
    Set,
    AlterSystem,
}

impl ObjectPrivilege {
    pub(crate) fn bit(self) -> u8 {
        match self {
            Self::Usage => 1,
            Self::Create => 2,
            Self::Set => 4,
            Self::AlterSystem => 8,
        }
    }

    /// Its name in error messages: `USAGE`, `ALTER SYSTEM`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Usage => "USAGE",
            Self::Create => "CREATE",
            Self::Set => "SET",
            Self::AlterSystem => "ALTER SYSTEM",
        }
    }
}

/// One stored grant on an object: who holds which privileges, and which of them they may grant
/// on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectGrant {
    pub(crate) grantee: String,
    pub(crate) privileges: u8,
    pub(crate) grantable: u8,
}

fn prefix(kind: ObjectKind, object: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(OBJECT_GRANT_SPACE);
    key.push(kind.byte());
    key.extend_from_slice(object.as_bytes());
    key.push(0);
    key
}

fn key(kind: ObjectKind, object: &str, grantee: &str) -> Vec<u8> {
    let mut key = prefix(kind, object);
    key.extend_from_slice(grantee.as_bytes());
    key
}

fn read(reader: &dyn KeyValueRead, key: &[u8]) -> SqlResult<(u8, u8)> {
    match reader.get(key)?.as_deref() {
        Some([RECORD_VERSION, held, grantable]) => Ok((*held, *grantable)),
        Some(_) => Err(crate::encoding::corrupt("object grant")),
        None => Ok((0, 0)),
    }
}

/// What a `GRANT` or `REVOKE` on objects asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ObjectChange<'a> {
    pub(crate) granting: bool,
    pub(crate) grant_option: bool,
    pub(crate) kind: ObjectKind,
    pub(crate) privileges: &'a [ObjectPrivilege],
    pub(crate) objects: &'a [String],
    pub(crate) grantees: &'a [String],
}

/// Stores `change`: the privileges granted (with the right to grant them on when
/// `grant_option`), revoked, or only that right revoked (`REVOKE GRANT OPTION FOR`).
pub(crate) fn change(writer: &mut dyn KeyValueWrite, change: &ObjectChange<'_>) -> SqlResult<()> {
    let bits = change
        .privileges
        .iter()
        .fold(0u8, |bits, privilege| bits | privilege.bit());
    for privilege in change.privileges {
        if !change.kind.privileges().contains(privilege) {
            return Err(SqlError::new(
                SqlState::INVALID_GRANT_OPERATION,
                format!(
                    "invalid privilege type {} for {}",
                    privilege.name(),
                    change.kind.keyword()
                ),
            ));
        }
    }
    for object in change.objects {
        if change.kind == ObjectKind::Schema && !crate::schemas::schema_exists(writer, object)? {
            return Err(SqlError::new(
                SqlState::INVALID_SCHEMA_NAME,
                format!("schema \"{object}\" does not exist"),
            ));
        }
        for grantee in change.grantees {
            if grantee != PUBLIC && stored_role(writer, grantee)?.is_none() {
                return Err(missing_role(grantee));
            }
            let key = key(change.kind, object, grantee);
            let (held, grantable) = read(writer, &key)?;
            let (held, grantable) = match (change.granting, change.grant_option) {
                (true, true) => (held | bits, grantable | bits),
                (true, false) => (held | bits, grantable),
                (false, true) => (held, grantable & !bits),
                (false, false) => (held & !bits, grantable & !bits),
            };
            match held {
                0 => writer.delete(&key)?,
                _ => writer.put(&key, &[RECORD_VERSION, held, grantable & held])?,
            }
        }
    }
    Ok(())
}

/// Every grant stored on `object`, in grantee order.
pub(crate) fn grants_on(
    reader: &dyn KeyValueRead,
    kind: ObjectKind,
    object: &str,
) -> SqlResult<Vec<ObjectGrant>> {
    let prefix = prefix(kind, object);
    let mut grants = Vec::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let grantee = key.get(prefix.len()..).unwrap_or_default();
            if let (Ok(grantee), [RECORD_VERSION, held, grantable]) =
                (std::str::from_utf8(grantee), value)
                && !grantee.contains('\0')
            {
                grants.push(ObjectGrant {
                    grantee: grantee.to_owned(),
                    privileges: *held,
                    grantable: *grantable,
                });
            }
            Ok(true)
        },
    )?;
    Ok(grants)
}

/// Whether `role`, a role it belongs to, or `PUBLIC` holds `privilege` on `object` (with the
/// right to grant it on, when `with_option`).
pub(crate) fn holds(
    reader: &dyn KeyValueRead,
    role: &str,
    kind: ObjectKind,
    object: &str,
    privilege: ObjectPrivilege,
    with_option: bool,
) -> SqlResult<bool> {
    let held: BTreeSet<String> = role_closure(reader, role)?;
    for member in &held {
        let (privileges, grantable) = read(reader, &key(kind, object, member))?;
        let bits = if with_option { grantable } else { privileges };
        if bits & privilege.bit() != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Forgets every grant on `object`: when it is dropped, so one made later under its name starts
/// with none.
pub(crate) fn forget(writer: &mut dyn KeyValueWrite, kind: ObjectKind, object: &str) -> SqlResult<()> {
    let prefix = prefix(kind, object);
    let mut keys = Vec::new();
    writer.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, _| {
            keys.push(key.to_vec());
            Ok(true)
        },
    )?;
    for key in keys {
        writer.delete(&key)?;
    }
    Ok(())
}
