use super::RegKind;
use super::oids;
use super::snapshot::{self, CatalogSnapshot, RelationEntry, RelationSource};
use super::types::{oid_number, split_object_name};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::roles::{PUBLIC, Privilege};
use crate::types::Value;

const INQUIRIES: [&str; 8] = [
    "has_table_privilege",
    "has_column_privilege",
    "has_any_column_privilege",
    "has_schema_privilege",
    "has_database_privilege",
    "has_sequence_privilege",
    "has_function_privilege",
    "pg_has_role",
];

const GRANT_OPTION: &str = " WITH GRANT OPTION";
const ADMIN_OPTION: &str = " WITH ADMIN OPTION";
const SEQUENCE_COLUMNS: [&str; 3] = ["last_value", "log_cnt", "is_called"];
const EVERY_ROLE_USES_EVERY_SEQUENCE: bool = true;
const EVERY_ROLE_EXECUTES_EVERY_ROUTINE: bool = true;
const EVERY_ROLE_USES_EVERY_SCHEMA: bool = true;

/// Whether `name` is one of PostgreSQL's privilege inquiry functions this module answers.
pub(crate) fn is_inquiry(name: &str) -> bool {
    INQUIRIES.contains(&name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Table(Privilege),
    Maintain,
    Rule,
    Usage,
    Create,
    Connect,
    Temporary,
    Execute,
    Member,
    Set,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    asked: Asked,
    with_option: bool,
}

const TABLE_PRIVILEGES: [(&str, Asked); 9] = [
    ("SELECT", Asked::Table(Privilege::Select)),
    ("INSERT", Asked::Table(Privilege::Insert)),
    ("UPDATE", Asked::Table(Privilege::Update)),
    ("DELETE", Asked::Table(Privilege::Delete)),
    ("TRUNCATE", Asked::Table(Privilege::Truncate)),
    ("REFERENCES", Asked::Table(Privilege::References)),
    ("TRIGGER", Asked::Table(Privilege::Trigger)),
    ("MAINTAIN", Asked::Maintain),
    ("RULE", Asked::Rule),
];

const COLUMN_PRIVILEGES: [(&str, Asked); 4] = [
    ("SELECT", Asked::Table(Privilege::Select)),
    ("INSERT", Asked::Table(Privilege::Insert)),
    ("UPDATE", Asked::Table(Privilege::Update)),
    ("REFERENCES", Asked::Table(Privilege::References)),
];

const SEQUENCE_PRIVILEGES: [(&str, Asked); 3] = [
    ("USAGE", Asked::Usage),
    ("SELECT", Asked::Table(Privilege::Select)),
    ("UPDATE", Asked::Table(Privilege::Update)),
];

const SCHEMA_PRIVILEGES: [(&str, Asked); 2] = [("CREATE", Asked::Create), ("USAGE", Asked::Usage)];

const DATABASE_PRIVILEGES: [(&str, Asked); 4] = [
    ("CREATE", Asked::Create),
    ("CONNECT", Asked::Connect),
    ("TEMPORARY", Asked::Temporary),
    ("TEMP", Asked::Temporary),
];

const FUNCTION_PRIVILEGES: [(&str, Asked); 1] = [("EXECUTE", Asked::Execute)];

const ROLE_PRIVILEGES: [(&str, Asked); 3] = [
    ("MEMBER", Asked::Member),
    ("USAGE", Asked::Usage),
    ("SET", Asked::Set),
];

fn is_postgresql_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn unrecognized(chunk: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("unrecognized privilege type: \"{chunk}\""),
    )
}

fn matches_ignoring_case(text: &str, head: &str, tail: &str) -> bool {
    text.len() == head.len() + tail.len()
        && text
            .get(..head.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(head))
        && text
            .get(head.len()..)
            .is_some_and(|found| found.eq_ignore_ascii_case(tail))
}

fn parse_one(chunk: &str, known: &[(&str, Asked)], options: &[&str]) -> Option<Wanted> {
    known.iter().find_map(|(name, asked)| {
        if chunk.eq_ignore_ascii_case(name) {
            return Some(Wanted {
                asked: *asked,
                with_option: false,
            });
        }
        options
            .iter()
            .any(|option| matches_ignoring_case(chunk, name, option))
            .then_some(Wanted {
                asked: *asked,
                with_option: true,
            })
    })
}

fn parse_privileges(
    text: &str,
    known: &[(&str, Asked)],
    options: &[&str],
) -> SqlResult<Vec<Wanted>> {
    text.split(',')
        .map(|chunk| {
            let chunk = chunk.trim_matches(is_postgresql_space);
            parse_one(chunk, known, options).ok_or_else(|| unrecognized(chunk))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Holder {
    Superuser,
    Role(String),
}

impl Holder {
    fn checked_role(&self) -> Option<&str> {
        match self {
            Self::Superuser => None,
            Self::Role(role) => Some(role),
        }
    }
}

fn missing_role(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("role \"{name}\" does not exist"),
    )
}

fn object_oid(value: &Value) -> Option<u32> {
    match value {
        Value::Text(_) => None,
        other => oid_number(other).and_then(|number| u32::try_from(number).ok()),
    }
}

fn written_name(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        other => other.to_text().unwrap_or_default(),
    }
}

struct Inquiry<'s> {
    snapshot: &'s CatalogSnapshot,
}

impl Inquiry<'_> {
    fn session(&self) -> Holder {
        match self.snapshot.settings.checked_role.as_deref() {
            None => Holder::Superuser,
            Some(role) => Holder::Role(role.to_owned()),
        }
    }

    fn stored_holder(&self, name: &str) -> SqlResult<Option<Holder>> {
        Ok(
            crate::roles::stored_role(&self.snapshot.access, name)?.map(|role| {
                if role.superuser {
                    Holder::Superuser
                } else {
                    Holder::Role(role.name)
                }
            }),
        )
    }

    fn holder_named(&self, name: &str, public_allowed: bool) -> SqlResult<Holder> {
        if public_allowed && name == PUBLIC {
            return Ok(Holder::Role(PUBLIC.to_owned()));
        }
        if let Some(holder) = self.stored_holder(name)? {
            return Ok(holder);
        }
        if self.snapshot.settings.checked_role.is_none() && name == self.snapshot.user {
            return Ok(Holder::Superuser);
        }
        if oids::PREDEFINED_ROLES
            .iter()
            .any(|(_, predefined)| *predefined == name)
        {
            return Ok(Holder::Role(name.to_owned()));
        }
        Err(missing_role(name))
    }

    fn role_name_with_oid(&self, oid: u32) -> Option<String> {
        if oid == oids::SESSION_USER {
            return Some(self.snapshot.user.clone());
        }
        oids::PREDEFINED_ROLES
            .iter()
            .find(|(found, _)| *found == oid)
            .map(|(_, name)| (*name).to_owned())
            .or_else(|| {
                self.snapshot
                    .stored_roles
                    .iter()
                    .find(|role| crate::roles::role_oid(&role.name) == oid)
                    .map(|role| role.name.clone())
            })
    }

    fn holder_with_oid(&self, oid: u32) -> SqlResult<Holder> {
        if oid == oids::SESSION_USER {
            return Ok(self.session());
        }
        match self.role_name_with_oid(oid) {
            Some(name) => self.holder_named(&name, false),
            None => Ok(Holder::Role(PUBLIC.to_owned())),
        }
    }

    fn holder(&self, value: &Value, public_allowed: bool) -> SqlResult<Holder> {
        match object_oid(value) {
            Some(oid) => self.holder_with_oid(oid),
            None => self.holder_named(&written_name(value), public_allowed),
        }
    }

    fn relation_named(&self, text: &str) -> SqlResult<usize> {
        let (schema, name) = split_object_name(text, RegKind::Class)?;
        self.snapshot
            .find_relation(schema.as_deref(), &name)
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation \"{text}\" does not exist"),
                )
            })
    }

    fn relation(&self, value: &Value) -> SqlResult<Option<&RelationEntry>> {
        let at = match object_oid(value) {
            Some(oid) => self.snapshot.relation_index(oid),
            None => Some(self.relation_named(&written_name(value))?),
        };
        Ok(at.and_then(|at| self.snapshot.relations.get(at)))
    }

    fn table_privilege(
        &self,
        holder: &Holder,
        entry: &RelationEntry,
        wanted: Wanted,
    ) -> SqlResult<bool> {
        if wanted.asked == Asked::Rule {
            return Ok(false);
        }
        let Some(role) = holder.checked_role() else {
            return Ok(true);
        };
        if wanted.with_option {
            return Ok(false);
        }
        let Asked::Table(privilege) = wanted.asked else {
            return Ok(false);
        };
        match &entry.source {
            RelationSource::Catalog(_) => Ok(privilege == Privilege::Select),
            RelationSource::Index { .. } => Ok(false),
            RelationSource::Table(_) | RelationSource::View(_) | RelationSource::Sequence(_) => {
                crate::roles::holds_table_privilege(
                    &self.snapshot.access,
                    Some(role),
                    &entry.internal,
                    privilege,
                )
            }
        }
    }

    fn column_reach(
        &self,
        holder: &Holder,
        entry: &RelationEntry,
        wanted: Wanted,
    ) -> SqlResult<Option<crate::roles::ColumnReach>> {
        let Some(role) = holder.checked_role() else {
            return Ok(Some(crate::roles::ColumnReach::Every));
        };
        let Asked::Table(privilege) = wanted.asked else {
            return Ok(None);
        };
        if wanted.with_option {
            return Ok(None);
        }
        match &entry.source {
            RelationSource::Catalog(_) => {
                Ok((privilege == Privilege::Select).then_some(crate::roles::ColumnReach::Every))
            }
            RelationSource::Index { .. } => Ok(None),
            RelationSource::Table(_) | RelationSource::View(_) | RelationSource::Sequence(_) => {
                crate::roles::column_privilege_reach(
                    &self.snapshot.access,
                    Some(role),
                    &entry.internal,
                    privilege,
                )
            }
        }
    }

    fn any_table_privilege(
        &self,
        holder: &Holder,
        entry: &RelationEntry,
        wanted: &[Wanted],
    ) -> SqlResult<bool> {
        for wanted in wanted {
            if self.table_privilege(holder, entry, *wanted)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_table_privilege(
        &self,
        holder: &Holder,
        object: &Value,
        list: &Value,
    ) -> SqlResult<Value> {
        let entry = self.relation(object)?;
        let wanted = parse_privileges(&written_name(list), &TABLE_PRIVILEGES, &[GRANT_OPTION])?;
        answer(entry, |entry| {
            self.any_table_privilege(holder, entry, &wanted)
        })
    }

    fn has_column_privilege(
        &self,
        holder: &Holder,
        object: &Value,
        column: &Value,
        list: &Value,
    ) -> SqlResult<Value> {
        let entry = self.relation(object)?;
        let column = match (entry, column) {
            (Some(entry), Value::Text(name)) => {
                if !relation_columns(entry).contains(&name.as_str()) {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!(
                            "column \"{name}\" of relation \"{}\" does not exist",
                            entry.name
                        ),
                    ));
                }
                Some(name.clone())
            }
            (Some(entry), number) => number
                .as_integer()
                .and_then(|number| usize::try_from(number).ok())
                .and_then(|number| number.checked_sub(1))
                .and_then(|at| attribute_numbered(entry, at)),
            (None, _) => None,
        };
        let wanted = parse_privileges(&written_name(list), &COLUMN_PRIVILEGES, &[GRANT_OPTION])?;
        let (Some(entry), Some(column)) = (entry, column) else {
            return Ok(Value::Null);
        };
        for wanted in wanted {
            if self
                .column_reach(holder, entry, wanted)?
                .is_some_and(|reach| reach.allows(&column))
            {
                return Ok(Value::Boolean(true));
            }
        }
        Ok(Value::Boolean(false))
    }

    fn has_any_column_privilege(
        &self,
        holder: &Holder,
        object: &Value,
        list: &Value,
    ) -> SqlResult<Value> {
        let entry = self.relation(object)?;
        let wanted = parse_privileges(&written_name(list), &COLUMN_PRIVILEGES, &[GRANT_OPTION])?;
        answer(entry, |entry| {
            for wanted in &wanted {
                if self.column_reach(holder, entry, *wanted)?.is_some() {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    fn has_sequence_privilege(
        &self,
        holder: &Holder,
        object: &Value,
        list: &Value,
    ) -> SqlResult<Value> {
        let entry = self.relation(object)?;
        if let Some(entry) = entry
            && !matches!(entry.source, RelationSource::Sequence(_))
        {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a sequence", entry.name),
            ));
        }
        let wanted = parse_privileges(&written_name(list), &SEQUENCE_PRIVILEGES, &[GRANT_OPTION])?;
        answer(entry, |entry| {
            for wanted in &wanted {
                if self.sequence_privilege(holder, entry, *wanted)? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    fn sequence_privilege(
        &self,
        holder: &Holder,
        entry: &RelationEntry,
        wanted: Wanted,
    ) -> SqlResult<bool> {
        match (holder, wanted) {
            (Holder::Superuser, _) => Ok(true),
            (
                _,
                Wanted {
                    with_option: true, ..
                },
            ) => Ok(false),
            (
                _,
                Wanted {
                    asked: Asked::Table(Privilege::Select),
                    ..
                },
            ) => self.table_privilege(holder, entry, wanted),
            (_, _) => Ok(EVERY_ROLE_USES_EVERY_SEQUENCE),
        }
    }

    fn has_schema_privilege(
        &self,
        holder: &Holder,
        object: &Value,
        list: &Value,
    ) -> SqlResult<Value> {
        let schema = match object_oid(object) {
            Some(oid) => self.snapshot.schema(oid).map(|schema| schema.name.clone()),
            None => {
                let name = written_name(object);
                if self.snapshot.schema_named(&name).is_none() {
                    return Err(SqlError::new(
                        SqlState::INVALID_SCHEMA_NAME,
                        format!("schema \"{name}\" does not exist"),
                    ));
                }
                Some(name)
            }
        };
        let wanted = parse_privileges(&written_name(list), &SCHEMA_PRIVILEGES, &[GRANT_OPTION])?;
        answer(schema.as_ref(), |_| {
            Ok(wanted
                .iter()
                .any(|wanted| holds_object_privilege(holder, *wanted, false)))
        })
    }

    fn database_named(&self, oid: Option<u32>, name: &str) -> Option<String> {
        let current = &self.snapshot.database;
        let others = self
            .snapshot
            .settings
            .database_names
            .iter()
            .filter(|other| *other != current);
        match oid {
            Some(oids::DATABASE) => Some(current.clone()),
            Some(oid) => others
                .into_iter()
                .find(|other| super::relations::other_database_oid(other) == oid)
                .cloned(),
            None if name == current => Some(current.clone()),
            None => others.into_iter().find(|other| *other == name).cloned(),
        }
    }

    fn has_database_privilege(
        &self,
        holder: &Holder,
        object: &Value,
        list: &Value,
    ) -> SqlResult<Value> {
        let oid = object_oid(object);
        let name = written_name(object);
        let database = self.database_named(oid, &name);
        if oid.is_none() && database.is_none() {
            return Err(SqlError::new(
                SqlState::INVALID_CATALOG_NAME,
                format!("database \"{name}\" does not exist"),
            ));
        }
        let wanted = parse_privileges(&written_name(list), &DATABASE_PRIVILEGES, &[GRANT_OPTION])?;
        let current = database.as_deref() == Some(self.snapshot.database.as_str());
        answer(database.as_ref(), |_| {
            Ok(wanted
                .iter()
                .any(|wanted| holds_object_privilege(holder, *wanted, current)))
        })
    }

    fn function_exists(&self, text: &str) -> SqlResult<bool> {
        let missing = || {
            SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {text} does not exist"),
            )
        };
        let Some(open) = text.find('(') else {
            return Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "expected a left parenthesis",
            ));
        };
        let (name, arguments) = text.split_at(open);
        let Some(arguments) = arguments
            .trim_end_matches(is_postgresql_space)
            .strip_prefix('(')
            .and_then(|inner| inner.strip_suffix(')'))
        else {
            return Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "expected a right parenthesis",
            ));
        };
        let count = if arguments.trim_matches(is_postgresql_space).is_empty() {
            0
        } else {
            arguments.split(',').count()
        };
        let (schema, name) = split_object_name(name, RegKind::Proc)?;
        let in_catalog = schema
            .as_deref()
            .is_none_or(|schema| schema == crate::schemas::CATALOG_SCHEMA);
        let builtin = oids::BUILTIN_FUNCTIONS.iter().any(|function| {
            in_catalog && function.name == name && function.argument_types.len() == count
        });
        let routine = self
            .snapshot
            .routines_named(schema.as_deref(), &name)
            .iter()
            .any(|routine| {
                routine
                    .definition
                    .parameters
                    .iter()
                    .filter(|parameter| parameter.mode != crate::routines::ParameterMode::Out)
                    .count()
                    == count
            });
        if builtin || routine {
            Ok(true)
        } else {
            Err(missing())
        }
    }

    fn has_function_privilege(
        &self,
        holder: &Holder,
        object: &Value,
        list: &Value,
    ) -> SqlResult<Value> {
        let exists = match object_oid(object) {
            Some(oid) => {
                self.snapshot.routine(oid).is_some()
                    || oids::BUILTIN_FUNCTIONS
                        .iter()
                        .any(|function| function.oid == oid)
            }
            None => self.function_exists(&written_name(object))?,
        };
        let wanted = parse_privileges(&written_name(list), &FUNCTION_PRIVILEGES, &[GRANT_OPTION])?;
        answer(exists.then_some(()), |()| {
            Ok(wanted
                .iter()
                .any(|wanted| holds_object_privilege(holder, *wanted, false)))
        })
    }

    fn target_role(&self, value: &Value) -> SqlResult<Option<String>> {
        match object_oid(value) {
            Some(oid) => Ok(self.role_name_with_oid(oid)),
            None => {
                let name = written_name(value);
                self.holder_named(&name, false)?;
                Ok(Some(name))
            }
        }
    }

    fn pg_has_role(&self, holder: &Holder, role: &Value, list: &Value) -> SqlResult<Value> {
        let target = self.target_role(role)?;
        let wanted = parse_privileges(
            &written_name(list),
            &ROLE_PRIVILEGES,
            &[GRANT_OPTION, ADMIN_OPTION],
        )?;
        let Some(target) = target else {
            return Ok(Value::Boolean(false));
        };
        let Some(member) = holder.checked_role() else {
            return Ok(Value::Boolean(true));
        };
        if wanted.iter().all(|wanted| wanted.with_option) {
            return Ok(Value::Boolean(false));
        }
        let held = crate::roles::role_closure(&self.snapshot.access, member)?;
        Ok(Value::Boolean(held.contains(&target)))
    }
}

fn holds_object_privilege(holder: &Holder, wanted: Wanted, connected_here: bool) -> bool {
    let Holder::Role(role) = holder else {
        return true;
    };
    if wanted.with_option {
        return false;
    }
    match wanted.asked {
        Asked::Create | Asked::Temporary => crate::roles::may_define_objects(Some(role)),
        Asked::Connect => connected_here,
        Asked::Usage => EVERY_ROLE_USES_EVERY_SCHEMA,
        Asked::Execute => EVERY_ROLE_EXECUTES_EVERY_ROUTINE,
        Asked::Table(_) | Asked::Maintain | Asked::Rule | Asked::Member | Asked::Set => false,
    }
}

fn answer<T>(object: Option<T>, decide: impl FnOnce(T) -> SqlResult<bool>) -> SqlResult<Value> {
    match object {
        Some(object) => decide(object).map(Value::Boolean),
        None => Ok(Value::Null),
    }
}

fn relation_columns(entry: &RelationEntry) -> Vec<&str> {
    match &entry.source {
        RelationSource::Table(table) => table
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .filter(|name| !crate::temp_tables::is_hidden_column(name))
            .collect(),
        RelationSource::View(view) => view
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect(),
        RelationSource::Sequence(_) => SEQUENCE_COLUMNS.to_vec(),
        RelationSource::Catalog(relation) => relation
            .columns()
            .iter()
            .map(|column| column.name)
            .collect(),
        RelationSource::Index { .. } => Vec::new(),
    }
}

fn attribute_numbered(entry: &RelationEntry, at: usize) -> Option<String> {
    match &entry.source {
        RelationSource::Table(table) => table
            .columns
            .get(at)
            .map(|column| column.name.as_str())
            .filter(|name| !crate::temp_tables::is_hidden_column(name))
            .map(str::to_owned),
        _ => relation_columns(entry)
            .get(at)
            .map(|name| (*name).to_owned()),
    }
}

fn wrong_arguments(name: &str, values: &[Value]) -> SqlError {
    let types: Vec<String> = values.iter().map(Value::type_name).collect();
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name}({}) does not exist", types.join(", ")),
    )
}

/// Answers one of PostgreSQL's privilege inquiry functions from the grants, roles and
/// row-security records of the statement's catalog snapshot, with the functions that check
/// statements: `true` when the role holds any of the listed privileges, `NULL` when an object
/// named by OID does not exist.
pub(crate) fn function(name: &str, values: &[Value]) -> SqlResult<Value> {
    let snapshot = snapshot::require()?;
    let inquiry = Inquiry {
        snapshot: &snapshot,
    };
    let session = inquiry.session();
    match (name, values) {
        ("has_table_privilege", [object, list]) => {
            inquiry.has_table_privilege(&session, object, list)
        }
        ("has_table_privilege", [role, object, list]) => {
            let holder = inquiry.holder(role, true)?;
            inquiry.has_table_privilege(&holder, object, list)
        }
        ("has_column_privilege", [object, column, list]) => {
            inquiry.has_column_privilege(&session, object, column, list)
        }
        ("has_column_privilege", [role, object, column, list]) => {
            let holder = inquiry.holder(role, true)?;
            inquiry.has_column_privilege(&holder, object, column, list)
        }
        ("has_any_column_privilege", [object, list]) => {
            inquiry.has_any_column_privilege(&session, object, list)
        }
        ("has_any_column_privilege", [role, object, list]) => {
            let holder = inquiry.holder(role, true)?;
            inquiry.has_any_column_privilege(&holder, object, list)
        }
        ("has_sequence_privilege", [object, list]) => {
            inquiry.has_sequence_privilege(&session, object, list)
        }
        ("has_sequence_privilege", [role, object, list]) => {
            let holder = inquiry.holder(role, true)?;
            inquiry.has_sequence_privilege(&holder, object, list)
        }
        ("has_schema_privilege", [object, list]) => {
            inquiry.has_schema_privilege(&session, object, list)
        }
        ("has_schema_privilege", [role, object, list]) => {
            let holder = inquiry.holder(role, true)?;
            inquiry.has_schema_privilege(&holder, object, list)
        }
        ("has_database_privilege", [object, list]) => {
            inquiry.has_database_privilege(&session, object, list)
        }
        ("has_database_privilege", [role, object, list]) => {
            let holder = inquiry.holder(role, true)?;
            inquiry.has_database_privilege(&holder, object, list)
        }
        ("has_function_privilege", [object, list]) => {
            inquiry.has_function_privilege(&session, object, list)
        }
        ("has_function_privilege", [role, object, list]) => {
            let holder = inquiry.holder(role, true)?;
            inquiry.has_function_privilege(&holder, object, list)
        }
        ("pg_has_role", [role, list]) => inquiry.pg_has_role(&session, role, list),
        ("pg_has_role", [member, role, list]) => {
            let holder = inquiry.holder(member, false)?;
            inquiry.pg_has_role(&holder, role, list)
        }
        _ => Err(wrong_arguments(name, values)),
    }
}

#[cfg(test)]
pub(super) fn privilege_list_parses(text: &str, of_role: bool) -> SqlResult<usize> {
    let parsed = if of_role {
        parse_privileges(text, &ROLE_PRIVILEGES, &[GRANT_OPTION, ADMIN_OPTION])
    } else {
        parse_privileges(text, &TABLE_PRIVILEGES, &[GRANT_OPTION])
    };
    parsed.map(|wanted| wanted.len())
}
