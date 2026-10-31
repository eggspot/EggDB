use crate::ast::{Expr, ExprKind, Name};
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};

const POLICY_SPACE: u8 = 0x13;
const ROW_SECURITY_SPACE: u8 = 0x14;
const RECORD_VERSION: u8 = 1;
const ENABLED: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyCommand {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCommand {
    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(code: u8) -> Option<Self> {
        [Self::All, Self::Select, Self::Insert, Self::Update, Self::Delete]
            .into_iter()
            .find(|command| command.code() == code)
    }

    fn covers(self, wanted: Self) -> bool {
        self == Self::All || self == wanted
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Policy {
    pub(crate) name: String,
    pub(crate) command: PolicyCommand,
    pub(crate) permissive: bool,
    pub(crate) roles: Vec<String>,
    pub(crate) using: Option<String>,
    pub(crate) check: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowSecurityStatement {
    CreatePolicy { table: Name, policy: Policy },
    DropPolicy { table: Name, name: Name, if_exists: bool },
    Switch { table: Name, enabled: bool },
}

impl RowSecurityStatement {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::CreatePolicy { .. } => "CREATE POLICY",
            Self::DropPolicy { .. } => "DROP POLICY",
            Self::Switch { .. } => "ALTER TABLE",
        }
    }

    pub(crate) fn table_mut(&mut self) -> &mut Name {
        match self {
            Self::CreatePolicy { table, .. }
            | Self::DropPolicy { table, .. }
            | Self::Switch { table, .. } => table,
        }
    }
}

fn policy_prefix(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(POLICY_SPACE);
    key.extend_from_slice(table.as_bytes());
    key.push(0);
    key
}

fn policy_key(table: &str, name: &str) -> Vec<u8> {
    let mut key = policy_prefix(table);
    key.extend_from_slice(name.as_bytes());
    key
}

fn switch_key(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(ROW_SECURITY_SPACE);
    key.extend_from_slice(table.as_bytes());
    key
}

fn put_text(out: &mut Vec<u8>, text: Option<&str>) {
    match text {
        None => out.push(0),
        Some(text) => {
            out.push(1);
            out.extend_from_slice(&(text.len() as u32).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
    }
}

fn encode(policy: &Policy) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION, policy.command.code(), u8::from(policy.permissive)];
    out.extend_from_slice(&(policy.roles.len() as u32).to_le_bytes());
    for role in &policy.roles {
        put_text(&mut out, Some(role));
    }
    put_text(&mut out, policy.using.as_deref());
    put_text(&mut out, policy.check.as_deref());
    out
}

fn corrupt() -> SqlError {
    crate::encoding::corrupt("row security policy")
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, count: usize) -> SqlResult<&'a [u8]> {
    let end = at.checked_add(count).ok_or_else(corrupt)?;
    let slice = bytes.get(*at..end).ok_or_else(corrupt)?;
    *at = end;
    Ok(slice)
}

fn take_u32(bytes: &[u8], at: &mut usize) -> SqlResult<usize> {
    let word = take(bytes, at, 4)?;
    Ok(u32::from_le_bytes([word[0], word[1], word[2], word[3]]) as usize)
}

fn take_text(bytes: &[u8], at: &mut usize) -> SqlResult<Option<String>> {
    match take(bytes, at, 1)? {
        [0] => Ok(None),
        [1] => {
            let length = take_u32(bytes, at)?;
            let text = take(bytes, at, length)?;
            String::from_utf8(text.to_vec()).map(Some).map_err(|_| corrupt())
        }
        _ => Err(corrupt()),
    }
}

fn decode(name: &str, bytes: &[u8]) -> SqlResult<Policy> {
    let [RECORD_VERSION, command, permissive, ..] = bytes else {
        return Err(corrupt());
    };
    let command = PolicyCommand::from_code(*command).ok_or_else(corrupt)?;
    let mut at = 3;
    let count = take_u32(bytes, &mut at)?;
    if count > bytes.len() {
        return Err(corrupt());
    }
    let mut roles = Vec::with_capacity(count);
    for _ in 0..count {
        roles.push(take_text(bytes, &mut at)?.ok_or_else(corrupt)?);
    }
    let using = take_text(bytes, &mut at)?;
    let check = take_text(bytes, &mut at)?;
    if at != bytes.len() {
        return Err(corrupt());
    }
    Ok(Policy {
        name: name.to_owned(),
        command,
        permissive: *permissive != 0,
        roles,
        using,
        check,
    })
}

pub(crate) fn policies(reader: &dyn KeyValueRead, table: &str) -> SqlResult<Vec<Policy>> {
    let prefix = policy_prefix(table);
    let mut found = Vec::new();
    let mut failure = None;
    reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |key, value| {
        let decoded = std::str::from_utf8(&key[prefix.len()..])
            .map_err(|_| corrupt())
            .and_then(|name| decode(name, value));
        match decoded {
            Ok(policy) => {
                found.push(policy);
                Ok(true)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        }
    })?;
    failure.map_or(Ok(found), Err)
}

pub(crate) fn is_enabled(reader: &dyn KeyValueRead, table: &str) -> SqlResult<bool> {
    Ok(matches!(
        reader.get(&switch_key(table))?.as_deref(),
        Some([RECORD_VERSION, flags]) if flags & ENABLED != 0
    ))
}

fn missing_table(table: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("relation \"{}\" does not exist", table.text),
    )
    .at(table.position)
}

pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    settings: &Settings,
    statement: &RowSecurityStatement,
) -> SqlResult<QueryResult> {
    if settings.checked_role.is_some() {
        return Err(SqlError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            "permission denied: only a superuser may change row security",
        ));
    }
    let table = match statement {
        RowSecurityStatement::CreatePolicy { table, .. }
        | RowSecurityStatement::DropPolicy { table, .. }
        | RowSecurityStatement::Switch { table, .. } => table,
    };
    if writer.get(&crate::encoding::table_key(&table.text))?.is_none() {
        return Err(missing_table(table));
    }
    match statement {
        RowSecurityStatement::CreatePolicy { table, policy } => {
            let key = policy_key(&table.text, &policy.name);
            if writer.get(&key)?.is_some() {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "policy \"{}\" for table \"{}\" already exists",
                        policy.name, table.text
                    ),
                ));
            }
            for role in &policy.roles {
                if role != crate::roles::PUBLIC
                    && crate::roles::stored_role(writer, role)?.is_none()
                {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("role \"{role}\" does not exist"),
                    ));
                }
            }
            writer.put(&key, &encode(policy))?;
        }
        RowSecurityStatement::DropPolicy {
            table,
            name,
            if_exists,
        } => {
            let key = policy_key(&table.text, &name.text);
            if writer.get(&key)?.is_none() {
                if *if_exists {
                    return Ok(QueryResult::command(statement.tag()));
                }
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "policy \"{}\" for table \"{}\" does not exist",
                        name.text, table.text
                    ),
                )
                .at(name.position));
            }
            writer.delete(&key)?;
        }
        RowSecurityStatement::Switch { table, enabled } => {
            if *enabled {
                writer.put(&switch_key(&table.text), &[RECORD_VERSION, ENABLED])?;
            } else {
                writer.delete(&switch_key(&table.text))?;
            }
        }
    }
    Ok(QueryResult::command(statement.tag()))
}

pub(crate) fn forget_table(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    crate::store::delete_prefix(writer, &policy_prefix(table))?;
    writer.delete(&switch_key(table))
}

fn applicable(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    command: PolicyCommand,
) -> SqlResult<Option<Vec<Policy>>> {
    let Some(user) = settings.checked_role.as_deref() else {
        return Ok(None);
    };
    if !is_enabled(reader, table)? {
        return Ok(None);
    }
    let held = crate::roles::role_closure(reader, user)?;
    Ok(Some(
        policies(reader, table)?
            .into_iter()
            .filter(|policy| policy.command.covers(command))
            .filter(|policy| policy.roles.iter().any(|role| held.contains(role)))
            .collect(),
    ))
}

fn combined(policies: &[Policy], clause: impl Fn(&Policy) -> Option<String>) -> String {
    let permissive: Vec<String> = policies
        .iter()
        .filter(|policy| policy.permissive)
        .map(|policy| format!("({})", clause(policy).unwrap_or_else(|| "true".to_owned())))
        .collect();
    let mut condition = if permissive.is_empty() {
        "false".to_owned()
    } else {
        format!("({})", permissive.join(" OR "))
    };
    for policy in policies.iter().filter(|policy| !policy.permissive) {
        condition.push_str(&format!(
            " AND ({})",
            clause(policy).unwrap_or_else(|| "true".to_owned())
        ));
    }
    condition
}

/// The condition rows of `table` must meet for the session's role to see them with `command`,
/// as SQL text, or `None` when row security does not apply to this session or table.
pub(crate) fn visible_rows(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    command: PolicyCommand,
) -> SqlResult<Option<String>> {
    Ok(applicable(reader, settings, table, command)?
        .map(|policies| combined(&policies, |policy| policy.using.clone())))
}

/// Refuses an `INSERT` or `UPDATE` under row security unless the role's policies let every new
/// row through (`WITH CHECK (true)`): checks on new rows are not evaluated yet.
pub(crate) fn check_new_rows(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    command: PolicyCommand,
) -> SqlResult<()> {
    let Some(policies) = applicable(reader, settings, table, command)? else {
        return Ok(());
    };
    let open = !policies.is_empty()
        && policies.iter().any(|policy| policy.permissive)
        && policies.iter().all(|policy| {
            policy
                .check
                .as_deref()
                .is_some_and(|check| check.trim().eq_ignore_ascii_case("true"))
        });
    if open {
        return Ok(());
    }
    Err(SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!(
            "new row violates row-level security policy for table \"{}\"",
            crate::schemas::display(table)
        ),
    )
    .with_hint("EggDB checks new rows only against policies WITH CHECK (true) so far."))
}

pub(crate) fn condition_expression(text: &str, settings: &Settings) -> SqlResult<Expr> {
    let tokens = crate::lexer::tokenize_in(text, crate::Dialect::PostgreSql)
        .map_err(|_| corrupt())?;
    let mut parser = crate::parser::Parser::new(text, &tokens)
        .with_context(crate::parser::Parameters::Absent, settings)
        .in_dialect(crate::Dialect::PostgreSql);
    let expr = parser.expression().map_err(|_| corrupt())?;
    if !parser.at_end() {
        return Err(corrupt());
    }
    Ok(expr)
}

pub(crate) fn restricted(filter: Option<Expr>, condition: Expr) -> Expr {
    let position = condition.position;
    match filter {
        None => condition,
        Some(filter) => Expr {
            kind: ExprKind::And(vec![filter, condition]),
            position,
        },
    }
}

#[cfg(test)]
mod tests;
