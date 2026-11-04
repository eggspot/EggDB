use crate::ast::{Expr, ExprKind, Name, SelectItem};
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;

pub(crate) const POLICY_SPACE: u8 = 0x13;
pub(crate) const ROW_SECURITY_SPACE: u8 = 0x14;
const RECORD_VERSION: u8 = 1;
const ENABLED: u8 = 1;
const FORCED: u8 = 2;

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
        [
            Self::All,
            Self::Select,
            Self::Insert,
            Self::Update,
            Self::Delete,
        ]
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
    CreatePolicy {
        table: Name,
        policy: Policy,
    },
    DropPolicy {
        table: Name,
        name: Name,
        if_exists: bool,
    },
    Switch {
        table: Name,
        enabled: bool,
    },
    Force {
        table: Name,
        forced: bool,
    },
}

/// Whether a table applies row security (`ENABLE ROW LEVEL SECURITY`) and whether it applies
/// it to the table's owner too (`FORCE ROW LEVEL SECURITY`), as `pg_class.relrowsecurity` and
/// `relforcerowsecurity` show them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RowSecuritySwitch {
    pub(crate) enabled: bool,
    pub(crate) forced: bool,
}

impl RowSecurityStatement {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::CreatePolicy { .. } => "CREATE POLICY",
            Self::DropPolicy { .. } => "DROP POLICY",
            Self::Switch { .. } | Self::Force { .. } => "ALTER TABLE",
        }
    }

    pub(crate) fn table_mut(&mut self) -> &mut Name {
        match self {
            Self::CreatePolicy { table, .. }
            | Self::DropPolicy { table, .. }
            | Self::Switch { table, .. }
            | Self::Force { table, .. } => table,
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
    let mut out = vec![
        RECORD_VERSION,
        policy.command.code(),
        u8::from(policy.permissive),
    ];
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
            String::from_utf8(text.to_vec())
                .map(Some)
                .map_err(|_| corrupt())
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

pub(crate) fn stored_conditions(key: &[u8], value: &[u8]) -> Option<SqlResult<Vec<String>>> {
    let (&space, rest) = key.strip_prefix(NAMESPACE)?.split_first()?;
    if space != POLICY_SPACE {
        return None;
    }
    let name = match rest.iter().rposition(|&byte| byte == 0) {
        Some(end) => String::from_utf8_lossy(rest.get(end + 1..).unwrap_or_default()),
        None => return Some(Err(corrupt())),
    };
    Some(decode(&name, value).map(|policy| policy.using.into_iter().chain(policy.check).collect()))
}

fn policy_records(reader: &dyn KeyValueRead, table: &str) -> SqlResult<Vec<(String, Vec<u8>)>> {
    let prefix = policy_prefix(table);
    let mut found = Vec::new();
    let mut damaged = false;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let name = key.get(prefix.len()..).unwrap_or_default();
            if name.contains(&0) {
                return Ok(true);
            }
            let Ok(name) = std::str::from_utf8(name) else {
                damaged = true;
                return Ok(false);
            };
            found.push((name.to_owned(), value.to_vec()));
            Ok(true)
        },
    )?;
    if damaged {
        return Err(corrupt());
    }
    Ok(found)
}

pub(crate) fn policies(reader: &dyn KeyValueRead, table: &str) -> SqlResult<Vec<Policy>> {
    policy_records(reader, table)?
        .into_iter()
        .map(|(name, value)| decode(&name, &value))
        .collect()
}

fn switch_flags(reader: &dyn KeyValueRead, table: &str) -> SqlResult<u8> {
    match reader.get(&switch_key(table))?.as_deref() {
        None => Ok(0),
        Some([RECORD_VERSION, flags]) => Ok(*flags),
        Some(_) => Err(corrupt()),
    }
}

/// The row security switches of `table`. The `FORCE` flag is a second bit in the switch record
/// of D186, so a release that knows only `ENABLE` still reads and applies the first bit.
pub(crate) fn switch_state(reader: &dyn KeyValueRead, table: &str) -> SqlResult<RowSecuritySwitch> {
    let flags = switch_flags(reader, table)?;
    Ok(RowSecuritySwitch {
        enabled: flags & ENABLED != 0,
        forced: flags & FORCED != 0,
    })
}

fn change_switch(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    flag: u8,
    on: bool,
) -> SqlResult<()> {
    let current = switch_flags(writer, table)?;
    let changed = if on { current | flag } else { current & !flag };
    if changed == 0 {
        writer.delete(&switch_key(table)).map(|_| ())
    } else {
        writer.put(&switch_key(table), &[RECORD_VERSION, changed])
    }
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
        | RowSecurityStatement::Switch { table, .. }
        | RowSecurityStatement::Force { table, .. } => table,
    };
    if writer
        .get(&crate::encoding::table_key(&table.text))?
        .is_none()
    {
        return Err(missing_table(table));
    }
    match statement {
        RowSecurityStatement::CreatePolicy { table, policy } => {
            if policy.name.is_empty() || policy.name.contains('\0') {
                return Err(SqlError::new(
                    SqlState::INVALID_NAME,
                    format!("invalid policy name \"{}\"", policy.name.escape_default()),
                ));
            }
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
            change_switch(writer, &table.text, ENABLED, *enabled)?;
        }
        RowSecurityStatement::Force { table, forced } => {
            change_switch(writer, &table.text, FORCED, *forced)?;
        }
    }
    Ok(QueryResult::command(statement.tag()))
}

pub(crate) fn forget_table(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    for (name, _) in policy_records(writer, table)? {
        writer.delete(&policy_key(table, &name))?;
    }
    writer.delete(&switch_key(table)).map(|_| ())
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

/// Refuses a `MERGE` (and any write that does not evaluate its new rows) under row security
/// unless the role's policies let every new row through (`WITH CHECK (true)`).
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
    Err(new_row_violation(table).with_hint(
        "MERGE checks new rows only against policies WITH CHECK (true) so far; use INSERT and UPDATE, which evaluate each policy.",
    ))
}

fn new_row_violation(table: &str) -> SqlError {
    SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!(
            "new row violates row-level security policy for table \"{}\"",
            crate::schemas::display(table)
        ),
    )
}

const NEW_ROW_CHECK: &str = "\u{2}row security check";
const NEW_ROW_CHECK_ALONE: &str = "\u{2}row security check alone";

/// The condition every row an `INSERT` or `UPDATE` writes must meet under the session's
/// policies, as PostgreSQL's `CREATE POLICY` defines it: each policy's `WITH CHECK`, or its
/// `USING` when it has none, permissive policies joined by `OR` and restrictive ones by `AND`.
/// When the statement returns rows, the new rows must also be ones the role may see. `None`
/// when row security does not apply.
pub(crate) fn new_row_condition(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    command: PolicyCommand,
    returns_rows: bool,
) -> SqlResult<Option<String>> {
    let Some(policies) = applicable(reader, settings, table, command)? else {
        return Ok(None);
    };
    let check = combined(&policies, |policy| {
        policy.check.clone().or_else(|| policy.using.clone())
    });
    let mut condition = format!("({check})");
    if returns_rows
        && let Some(visible) = visible_rows(reader, settings, table, PolicyCommand::Select)?
    {
        condition = format!("{condition} AND ({visible})");
    }
    Ok(Some(condition))
}

/// Appends `check` to a write's `RETURNING` list under a label [`checked_new_rows`] finds, so
/// the executor evaluates it on each new row after `BEFORE` triggers ran.
pub(crate) fn append_new_row_check(returning: &mut Option<Vec<SelectItem>>, check: Expr) {
    let label = if returning.is_some() {
        NEW_ROW_CHECK
    } else {
        NEW_ROW_CHECK_ALONE
    };
    returning
        .get_or_insert_with(Vec::new)
        .push(SelectItem::Expr {
            expr: check,
            alias: Some(label.to_owned()),
        });
}

/// Whether [`append_new_row_check`] added its check to `returning`.
pub(crate) fn has_new_row_check(returning: Option<&[SelectItem]>) -> bool {
    matches!(
        returning.and_then(<[SelectItem]>::last),
        Some(SelectItem::Expr { alias: Some(label), .. })
            if label == NEW_ROW_CHECK || label == NEW_ROW_CHECK_ALONE
    )
}

/// Fails with `42501 new row violates row-level security policy` unless every row `result`
/// returns met the check [`append_new_row_check`] added, then removes the check's column (and
/// the rows, when the statement had no `RETURNING` of its own). When `check_appended` is true
/// and the result lacks the check's column, the write is refused as well.
pub(crate) fn checked_new_rows(
    mut result: QueryResult,
    table: &str,
    check_appended: bool,
) -> SqlResult<QueryResult> {
    if !check_appended {
        return Ok(result);
    }
    let Some(columns) = &mut result.columns else {
        return Err(new_row_violation(table));
    };
    let alone = match columns.last().map(|column| column.name.as_str()) {
        Some(name) if name.eq_ignore_ascii_case(NEW_ROW_CHECK_ALONE) => true,
        Some(name) if name.eq_ignore_ascii_case(NEW_ROW_CHECK) => false,
        _ => return Err(new_row_violation(table)),
    };
    columns.pop();
    for row in &mut result.rows {
        if row.pop() != Some(Value::Boolean(true)) {
            return Err(new_row_violation(table));
        }
    }
    if alone {
        result.columns = None;
        result.rows.clear();
    }
    Ok(result)
}

/// Moves the policies and the row security switch of `from` to `to`, when the table is
/// renamed. The policies of the tables in a schema named like `from` stay where they are.
pub(crate) fn move_table(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    if from == to {
        return Ok(());
    }
    for (name, value) in policy_records(writer, from)? {
        writer.delete(&policy_key(from, &name))?;
        writer.put(&policy_key(to, &name), &value)?;
    }
    if let Some(switch) = writer.get(&switch_key(from))? {
        writer.delete(&switch_key(from))?;
        writer.put(&switch_key(to), &switch)?;
    }
    Ok(())
}

pub(crate) fn condition_expression(text: &str, settings: &Settings) -> SqlResult<Expr> {
    let tokens =
        crate::lexer::tokenize_in(text, crate::Dialect::PostgreSql).map_err(|_| corrupt())?;
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
