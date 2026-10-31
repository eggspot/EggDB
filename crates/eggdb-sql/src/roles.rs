use crate::ast::{Name, Statement};
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use eggdb_crypto::{base64_decode, base64_encode, hmac_sha256, pbkdf2_hmac_sha256, sha256};
use std::collections::BTreeSet;

const ROLE_SPACE: u8 = 0x11;
const GRANT_SPACE: u8 = 0x12;
const RECORD_VERSION: u8 = 1;
const LOGIN: u8 = 1;
const SUPERUSER: u8 = 2;
const SCRAM_ITERATIONS: u32 = 4096;
const SCRAM_SALT_BYTES: usize = 16;
const MAX_MEMBERSHIP_DEPTH: usize = 32;
const MAX_NAME_BYTES: usize = 63;

pub(crate) const PUBLIC: &str = "public";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
}

impl Privilege {
    pub(crate) const ALL: [Self; 7] = [
        Self::Select,
        Self::Insert,
        Self::Update,
        Self::Delete,
        Self::Truncate,
        Self::References,
        Self::Trigger,
    ];

    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Truncate => "truncate",
            Self::References => "references",
            Self::Trigger => "trigger",
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RoleOptions {
    pub(crate) login: Option<bool>,
    pub(crate) superuser: Option<bool>,
    pub(crate) password: Option<Option<String>>,
    pub(crate) member_of: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RoleStatement {
    Create {
        name: Name,
        options: RoleOptions,
        if_not_exists: bool,
    },
    Alter {
        name: Name,
        options: RoleOptions,
    },
    Drop {
        names: Vec<Name>,
        if_exists: bool,
    },
    Privileges {
        granting: bool,
        privileges: Vec<Privilege>,
        tables: Vec<Name>,
        grantees: Vec<String>,
    },
    Membership {
        granting: bool,
        roles: Vec<String>,
        members: Vec<String>,
    },
}

impl RoleStatement {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::Create { .. } => "CREATE ROLE",
            Self::Alter { .. } => "ALTER ROLE",
            Self::Drop { .. } => "DROP ROLE",
            Self::Privileges { granting: true, .. } | Self::Membership { granting: true, .. } => {
                "GRANT"
            }
            Self::Privileges { .. } | Self::Membership { .. } => "REVOKE",
        }
    }

    pub(crate) fn tables_mut(&mut self) -> &mut [Name] {
        match self {
            Self::Privileges { tables, .. } => tables,
            _ => &mut [],
        }
    }
}

/// A role as the catalog keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRole {
    /// The role's name.
    pub name: String,
    /// Whether the role may log in.
    pub login: bool,
    /// Whether the role bypasses every privilege check.
    pub superuser: bool,
    /// The SCRAM-SHA-256 verifier in PostgreSQL's `pg_authid.rolpassword` form
    /// (`SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`), when the role has a password.
    pub scram_verifier: Option<String>,
    /// The roles this role is a member of.
    pub member_of: Vec<String>,
}

fn role_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(ROLE_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

fn grant_prefix(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(GRANT_SPACE);
    key.extend_from_slice(table.as_bytes());
    key.push(0);
    key
}

fn grant_key(table: &str, grantee: &str) -> Vec<u8> {
    let mut key = grant_prefix(table);
    key.extend_from_slice(grantee.as_bytes());
    key
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn encode_role(role: &StoredRole) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    out.push(
        if role.login { LOGIN } else { 0 } | if role.superuser { SUPERUSER } else { 0 },
    );
    put_text(&mut out, role.scram_verifier.as_deref().unwrap_or(""));
    out.extend_from_slice(&(role.member_of.len() as u32).to_le_bytes());
    for parent in &role.member_of {
        put_text(&mut out, parent);
    }
    out
}

fn corrupt_role() -> SqlError {
    crate::encoding::corrupt("role")
}

fn read_u32(bytes: &[u8], at: &mut usize) -> SqlResult<u32> {
    let word = bytes.get(*at..*at + 4).ok_or_else(corrupt_role)?;
    *at += 4;
    Ok(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

fn read_text(bytes: &[u8], at: &mut usize) -> SqlResult<String> {
    let length = read_u32(bytes, at)? as usize;
    let text = bytes
        .get(*at..at.checked_add(length).ok_or_else(corrupt_role)?)
        .ok_or_else(corrupt_role)?;
    *at += length;
    String::from_utf8(text.to_vec()).map_err(|_| corrupt_role())
}

fn decode_role(name: &str, bytes: &[u8]) -> SqlResult<StoredRole> {
    let [RECORD_VERSION, flags, ..] = bytes else {
        return Err(corrupt_role());
    };
    let mut at = 2;
    let verifier = read_text(bytes, &mut at)?;
    let count = read_u32(bytes, &mut at)? as usize;
    if count > bytes.len() {
        return Err(corrupt_role());
    }
    let mut member_of = Vec::with_capacity(count);
    for _ in 0..count {
        member_of.push(read_text(bytes, &mut at)?);
    }
    if at != bytes.len() {
        return Err(corrupt_role());
    }
    Ok(StoredRole {
        name: name.to_owned(),
        login: flags & LOGIN != 0,
        superuser: flags & SUPERUSER != 0,
        scram_verifier: (!verifier.is_empty()).then_some(verifier),
        member_of,
    })
}

/// The stored role named `name`, if there is one.
pub fn stored_role(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Option<StoredRole>> {
    reader
        .get(&role_key(name))?
        .map(|bytes| decode_role(name, &bytes))
        .transpose()
}

/// Every stored role, in name order.
pub fn stored_roles(reader: &dyn KeyValueRead) -> SqlResult<Vec<StoredRole>> {
    let prefix = role_key("");
    let mut roles = Vec::new();
    let mut failure = None;
    reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |key, value| {
        let decoded = std::str::from_utf8(&key[prefix.len()..])
            .map_err(|_| corrupt_role())
            .and_then(|name| decode_role(name, value));
        match decoded {
            Ok(role) => {
                roles.push(role);
                Ok(true)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        }
    })?;
    failure.map_or(Ok(roles), Err)
}

/// Derives the SCRAM-SHA-256 verifier of `password` in `pg_authid.rolpassword`'s form, with a
/// fresh random salt.
pub(crate) fn scram_verifier_text(password: &str) -> SqlResult<String> {
    if password.starts_with("SCRAM-SHA-256$") {
        parse_scram_verifier(password).ok_or_else(|| {
            SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "the password looks like a SCRAM-SHA-256 verifier but is not a valid one",
            )
        })?;
        return Ok(password.to_owned());
    }
    if !password.is_ascii() {
        return Err(SqlError::not_supported(
            "passwords outside ASCII need SASLprep, which is not implemented yet",
        ));
    }
    let mut salt = [0u8; SCRAM_SALT_BYTES];
    crate::random::fill(&mut salt)?;
    let mut salted = [0u8; 32];
    pbkdf2_hmac_sha256(password.as_bytes(), &salt, SCRAM_ITERATIONS, &mut salted);
    let stored_key = sha256(&hmac_sha256(&salted, b"Client Key"));
    let server_key = hmac_sha256(&salted, b"Server Key");
    Ok(format!(
        "SCRAM-SHA-256${SCRAM_ITERATIONS}:{}${}:{}",
        base64_encode(&salt),
        base64_encode(&stored_key),
        base64_encode(&server_key)
    ))
}

/// The parts of a verifier in `pg_authid.rolpassword`'s form: iterations, salt, stored key and
/// server key.
pub type ScramParts = (u32, Vec<u8>, [u8; 32], [u8; 32]);

/// Splits a SCRAM-SHA-256 verifier written as `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`
/// (base64 parts), or `None` when it is not one.
pub fn parse_scram_verifier(text: &str) -> Option<ScramParts> {
    let rest = text.strip_prefix("SCRAM-SHA-256$")?;
    let (head, keys) = rest.split_once('$')?;
    let (iterations, salt) = head.split_once(':')?;
    let (stored, server) = keys.split_once(':')?;
    let iterations: u32 = iterations.parse().ok().filter(|count| *count > 0)?;
    let salt = base64_decode(salt.as_bytes()).ok().filter(|salt| !salt.is_empty())?;
    let stored: [u8; 32] = base64_decode(stored.as_bytes()).ok()?.try_into().ok()?;
    let server: [u8; 32] = base64_decode(server.as_bytes()).ok()?.try_into().ok()?;
    Some((iterations, salt, stored, server))
}

fn checked_name(name: &Name) -> SqlResult<()> {
    if name.text.is_empty() || name.text.len() > MAX_NAME_BYTES || name.text.contains('\0') {
        return Err(SqlError::new(
            SqlState::INVALID_NAME,
            format!("invalid role name \"{}\"", name.text.escape_default()),
        )
        .at(name.position));
    }
    if name.text == PUBLIC {
        return Err(SqlError::new(
            SqlState::RESERVED_NAME,
            "role name \"public\" is reserved",
        )
        .at(name.position));
    }
    Ok(())
}

fn missing_role(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("role \"{name}\" does not exist"),
    )
}

fn apply_options(role: &mut StoredRole, options: &RoleOptions) -> SqlResult<()> {
    if let Some(login) = options.login {
        role.login = login;
    }
    if let Some(superuser) = options.superuser {
        role.superuser = superuser;
    }
    if let Some(password) = &options.password {
        role.scram_verifier = password.as_deref().map(scram_verifier_text).transpose()?;
    }
    Ok(())
}

fn require_superuser(settings: &Settings, what: &str) -> SqlResult<()> {
    if settings.checked_role.is_some() {
        return Err(SqlError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied to {what}"),
        )
        .with_hint("Only a superuser may manage roles and privileges."));
    }
    Ok(())
}

pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    settings: &Settings,
    statement: &RoleStatement,
) -> SqlResult<QueryResult> {
    let tag = statement.tag();
    require_superuser(settings, &tag.to_ascii_lowercase())?;
    match statement {
        RoleStatement::Create {
            name,
            options,
            if_not_exists,
        } => {
            checked_name(name)?;
            if stored_role(writer, &name.text)?.is_some() {
                if *if_not_exists {
                    return Ok(QueryResult::command(tag));
                }
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("role \"{}\" already exists", name.text),
                )
                .at(name.position));
            }
            let mut role = StoredRole {
                name: name.text.clone(),
                login: false,
                superuser: false,
                scram_verifier: None,
                member_of: Vec::new(),
            };
            apply_options(&mut role, options)?;
            for parent in &options.member_of {
                if stored_role(writer, parent)?.is_none() {
                    return Err(missing_role(parent));
                }
                if !role.member_of.contains(parent) {
                    role.member_of.push(parent.clone());
                }
            }
            writer.put(&role_key(&role.name), &encode_role(&role))?;
        }
        RoleStatement::Alter { name, options } => {
            let mut role = stored_role(writer, &name.text)?
                .ok_or_else(|| missing_role(&name.text).at(name.position))?;
            apply_options(&mut role, options)?;
            writer.put(&role_key(&role.name), &encode_role(&role))?;
        }
        RoleStatement::Drop { names, if_exists } => drop_roles(writer, names, *if_exists)?,
        RoleStatement::Privileges {
            granting,
            privileges,
            tables,
            grantees,
        } => change_privileges(writer, *granting, privileges, tables, grantees)?,
        RoleStatement::Membership {
            granting,
            roles,
            members,
        } => change_membership(writer, *granting, roles, members)?,
    }
    Ok(QueryResult::command(tag))
}

fn drop_roles(writer: &mut dyn KeyValueWrite, names: &[Name], if_exists: bool) -> SqlResult<()> {
    for name in names {
        if stored_role(writer, &name.text)?.is_none() {
            if if_exists {
                continue;
            }
            return Err(missing_role(&name.text).at(name.position));
        }
        let granted = granted_tables(writer, &name.text)?;
        if let Some(table) = granted.first() {
            return Err(SqlError::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "role \"{}\" cannot be dropped because some objects depend on it",
                    name.text
                ),
            )
            .with_detail(format!("privileges for table {table}"))
            .at(name.position));
        }
        writer.delete(&role_key(&name.text))?;
        for mut role in stored_roles(writer)? {
            let before = role.member_of.len();
            role.member_of.retain(|parent| parent != &name.text);
            if role.member_of.len() != before {
                writer.put(&role_key(&role.name), &encode_role(&role))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn forget_table(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    crate::store::delete_prefix(writer, &grant_prefix(table))
}

fn granted_tables(reader: &dyn KeyValueRead, grantee: &str) -> SqlResult<Vec<String>> {
    let mut prefix = NAMESPACE.to_vec();
    prefix.push(GRANT_SPACE);
    let mut tables = Vec::new();
    reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |key, _| {
        let rest = &key[prefix.len()..];
        if let Some(split) = rest.iter().position(|byte| *byte == 0)
            && &rest[split + 1..] == grantee.as_bytes()
        {
            tables.push(String::from_utf8_lossy(&rest[..split]).into_owned());
        }
        Ok(true)
    })?;
    Ok(tables)
}

fn change_privileges(
    writer: &mut dyn KeyValueWrite,
    granting: bool,
    privileges: &[Privilege],
    tables: &[Name],
    grantees: &[String],
) -> SqlResult<()> {
    let bits = privileges.iter().fold(0u8, |bits, privilege| bits | privilege.bit());
    for table in tables {
        if writer.get(&crate::encoding::table_key(&table.text))?.is_none() {
            return Err(SqlError::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", table.text),
            )
            .at(table.position));
        }
        for grantee in grantees {
            if grantee != PUBLIC && stored_role(writer, grantee)?.is_none() {
                return Err(missing_role(grantee));
            }
            let key = grant_key(&table.text, grantee);
            let current = match writer.get(&key)?.as_deref() {
                Some([RECORD_VERSION, held]) => *held,
                Some(_) => return Err(crate::encoding::corrupt("grant")),
                None => 0,
            };
            let changed = if granting { current | bits } else { current & !bits };
            if changed == 0 {
                writer.delete(&key)?;
            } else {
                writer.put(&key, &[RECORD_VERSION, changed])?;
            }
        }
    }
    Ok(())
}

fn change_membership(
    writer: &mut dyn KeyValueWrite,
    granting: bool,
    roles: &[String],
    members: &[String],
) -> SqlResult<()> {
    for parent in roles {
        if stored_role(writer, parent)?.is_none() {
            return Err(missing_role(parent));
        }
        for member in members {
            let mut role = stored_role(writer, member)?.ok_or_else(|| missing_role(member))?;
            if granting {
                if member == parent || role_closure(writer, parent)?.contains(member.as_str()) {
                    return Err(SqlError::new(
                        SqlState::INVALID_GRANT_OPERATION,
                        format!("role \"{parent}\" is a member of role \"{member}\""),
                    ));
                }
                if !role.member_of.contains(parent) {
                    role.member_of.push(parent.clone());
                }
            } else {
                role.member_of.retain(|held| held != parent);
            }
            writer.put(&role_key(member), &encode_role(&role))?;
        }
    }
    Ok(())
}

pub(crate) fn role_closure(reader: &dyn KeyValueRead, user: &str) -> SqlResult<BTreeSet<String>> {
    let mut held = BTreeSet::from([user.to_owned(), PUBLIC.to_owned()]);
    let mut frontier = vec![user.to_owned()];
    for _ in 0..MAX_MEMBERSHIP_DEPTH {
        let mut next = Vec::new();
        for name in frontier {
            if let Some(role) = stored_role(reader, &name)? {
                for parent in role.member_of {
                    if held.insert(parent.clone()) {
                        next.push(parent);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(held)
}

/// Checks that the session may use `privilege` on `table`: always for a superuser session
/// (embedded use and the server's own user), and otherwise when the role, a role it belongs to
/// or `PUBLIC` holds the privilege.
pub(crate) fn check_table(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    privilege: Privilege,
) -> SqlResult<()> {
    let Some(user) = settings.checked_role.as_deref() else {
        return Ok(());
    };
    if crate::temporary::is_temporary_name(table) {
        return Ok(());
    }
    for role in role_closure(reader, user)? {
        if let Some(bytes) = reader.get(&grant_key(table, &role))? {
            match bytes.as_slice() {
                [RECORD_VERSION, held] if held & privilege.bit() != 0 => return Ok(()),
                [RECORD_VERSION, _] => {}
                _ => return Err(crate::encoding::corrupt("grant")),
            }
        }
    }
    Err(SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!(
            "permission denied for table {}",
            crate::schemas::display(table)
        ),
    ))
}

pub(crate) fn target_privilege(statement: &Statement) -> Privilege {
    match statement {
        Statement::Insert(_) | Statement::Copy(_) => Privilege::Insert,
        Statement::Update(_) | Statement::Merge(_) => Privilege::Update,
        Statement::Delete(_) => Privilege::Delete,
        Statement::Truncate(_) => Privilege::Truncate,
        _ => Privilege::Select,
    }
}

/// Refuses statements that change the schema for a session whose privileges are checked: only
/// superusers define objects until object ownership lands.
pub(crate) fn check_statement(settings: &Settings, statement: &Statement) -> SqlResult<()> {
    if settings.checked_role.is_none() {
        return Ok(());
    }
    let defines = matches!(
        statement,
        Statement::CreateTable(_)
            | Statement::AlterTable(_)
            | Statement::CreateIndex(_)
            | Statement::CreateView(_)
            | Statement::AlterView(_)
            | Statement::CreateSequence(_)
            | Statement::AlterSequence(_)
            | Statement::CreateSchema { .. }
            | Statement::RenameSchema { .. }
            | Statement::CreateType(_)
            | Statement::CreateDomain(_)
            | Statement::AlterType(_)
            | Statement::AlterDomain(_)
            | Statement::DropTable { .. }
            | Statement::DropIndex(_)
            | Statement::DropView { .. }
            | Statement::DropSequence { .. }
            | Statement::DropSchema { .. }
            | Statement::DropType { .. }
            | Statement::Comment(_)
            | Statement::CreateTrigger(_)
            | Statement::AlterTrigger(_)
            | Statement::DropTrigger(_)
            | Statement::CreateRoutine(_)
            | Statement::AlterRoutine(_)
            | Statement::DropRoutine(_)
            | Statement::CreatePackage(_)
            | Statement::AlterPackage(_)
            | Statement::DropPackage(_)
            | Statement::Analyze(_)
    );
    if defines {
        return Err(SqlError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            "permission denied for schema public",
        )
        .with_hint("Only a superuser may create, change or drop objects until ownership lands."));
    }
    Ok(())
}

const STORED_ROLE_OIDS: std::ops::Range<u32> = 0x4000_0000..0x7fff_ffff;

pub(crate) fn role_oid(name: &str) -> u32 {
    let hash = name.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    STORED_ROLE_OIDS.start + hash % (STORED_ROLE_OIDS.end - STORED_ROLE_OIDS.start)
}

/// Logs in a stored role: its SCRAM verifier when it may log in and has a password.
pub fn login_verifier(reader: &dyn KeyValueRead, user: &str) -> SqlResult<Option<String>> {
    Ok(stored_role(reader, user)?
        .filter(|role| role.login)
        .and_then(|role| role.scram_verifier))
}

#[cfg(test)]
mod tests;
