use super::CatalogType;
use super::oids;
use super::privilege_inquiry::{is_postgresql_space, unrecognized};
use super::snapshot::{self, CatalogSnapshot, SchemaEntry};
use super::types::oid_number;
use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::json_functions::RecordSink;
use crate::set_returning::RowSink;
use crate::types::{DefinedType, SqlType, Value};

/// PostgreSQL's `aclexplode(aclitem[])`, the set-returning function that lists each privilege of
/// an access control list as one row.
pub(crate) const EXPLODE: &str = "aclexplode";

/// The columns of `aclexplode`'s rows, with their types.
pub(crate) const EXPLODED_COLUMNS: [(&str, SqlType); 4] = [
    ("grantor", SqlType::Catalog(CatalogType::Oid)),
    ("grantee", SqlType::Catalog(CatalogType::Oid)),
    ("privilege_type", SqlType::Text),
    ("is_grantable", SqlType::Boolean),
];

const DEFAULT: &str = "acldefault";
const MAKE_ITEM: &str = "makeaclitem";

const NO_RIGHTS: u16 = 0;
const INSERT: u16 = 1;
const SELECT: u16 = 1 << 1;
const UPDATE: u16 = 1 << 2;
const DELETE: u16 = 1 << 3;
const TRUNCATE: u16 = 1 << 4;
const REFERENCES: u16 = 1 << 5;
const TRIGGER: u16 = 1 << 6;
const EXECUTE: u16 = 1 << 7;
const USAGE: u16 = 1 << 8;
const CREATE: u16 = 1 << 9;
const TEMPORARY: u16 = 1 << 10;
const CONNECT: u16 = 1 << 11;
const SET: u16 = 1 << 12;
const ALTER_SYSTEM: u16 = 1 << 13;
const RELATION_RIGHTS: u16 = INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER;

const RIGHTS: [(u16, char, &str); 14] = [
    (INSERT, 'a', "INSERT"),
    (SELECT, 'r', "SELECT"),
    (UPDATE, 'w', "UPDATE"),
    (DELETE, 'd', "DELETE"),
    (TRUNCATE, 'D', "TRUNCATE"),
    (REFERENCES, 'x', "REFERENCES"),
    (TRIGGER, 't', "TRIGGER"),
    (EXECUTE, 'X', "EXECUTE"),
    (USAGE, 'U', "USAGE"),
    (CREATE, 'C', "CREATE"),
    (TEMPORARY, 'T', "TEMPORARY"),
    (CONNECT, 'c', "CONNECT"),
    (SET, 's', "SET"),
    (ALTER_SYSTEM, 'A', "ALTER SYSTEM"),
];

const RIGHT_LETTERS: &str = "arwdDxtXUCTcsA";

const NAMED_RIGHTS: [(&str, u16); 16] = [
    ("SELECT", SELECT),
    ("INSERT", INSERT),
    ("UPDATE", UPDATE),
    ("DELETE", DELETE),
    ("TRUNCATE", TRUNCATE),
    ("REFERENCES", REFERENCES),
    ("TRIGGER", TRIGGER),
    ("EXECUTE", EXECUTE),
    ("USAGE", USAGE),
    ("CREATE", CREATE),
    ("TEMP", TEMPORARY),
    ("TEMPORARY", TEMPORARY),
    ("CONNECT", CONNECT),
    ("SET", SET),
    ("ALTER SYSTEM", ALTER_SYSTEM),
    ("RULE", NO_RIGHTS),
];

const PUBLIC_GRANTEE: u32 = 0;
const MAX_NAME_BYTES: usize = 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AclItem {
    grantee: u32,
    grantor: u32,
    rights: u16,
    grant_options: u16,
}

struct Roles<'s> {
    snapshot: &'s CatalogSnapshot,
}

impl Roles<'_> {
    fn oid_named(&self, name: &str) -> Option<u32> {
        if name == self.snapshot.user {
            return Some(oids::SESSION_USER);
        }
        if let Some((oid, _)) = oids::PREDEFINED_ROLES
            .iter()
            .find(|(_, predefined)| *predefined == name)
        {
            return Some(*oid);
        }
        self.snapshot
            .stored_roles
            .iter()
            .any(|role| role.name == name)
            .then(|| crate::roles::role_oid(name))
    }

    fn name_of(&self, oid: u32) -> Option<String> {
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
                    .find(|role| {
                        role.name != self.snapshot.user && crate::roles::role_oid(&role.name) == oid
                    })
                    .map(|role| role.name.clone())
            })
    }

    fn written(&self, oid: u32) -> String {
        self.name_of(oid).map_or_else(
            || oid.to_string(),
            |name| crate::roles::acl_role_name(&name),
        )
    }

    fn oid_written(&self, name: &str) -> SqlResult<u32> {
        if let Some(oid) = self.oid_named(name) {
            return Ok(oid);
        }
        if !name.is_empty()
            && name.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(oid) = name.parse::<u32>()
        {
            return Ok(oid);
        }
        Err(SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("role \"{name}\" does not exist"),
        ))
    }
}

fn invalid_text(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, message)
}

fn read_name(text: &str) -> SqlResult<(String, &str)> {
    let text = text.trim_start_matches(is_postgresql_space);
    let mut name = String::new();
    let mut in_quotes = false;
    let mut end = text.len();
    let mut characters = text.char_indices().peekable();
    while let Some((at, character)) = characters.next() {
        if character == '"' {
            if characters
                .next_if(|(_, following)| *following == '"')
                .is_none()
            {
                in_quotes = !in_quotes;
                continue;
            }
        } else if !in_quotes && !character.is_ascii_alphanumeric() && character != '_' {
            end = at;
            break;
        }
        if name.len() + character.len_utf8() > MAX_NAME_BYTES {
            return Err(
                SqlError::new(SqlState::NAME_TOO_LONG, "identifier too long").with_hint(format!(
                    "Identifier must be less than {} characters.",
                    MAX_NAME_BYTES + 1
                )),
            );
        }
        name.push(character);
    }
    let rest = text
        .get(end..)
        .unwrap_or_default()
        .trim_start_matches(is_postgresql_space);
    Ok((name, rest))
}

fn right_lettered(letter: char) -> SqlResult<u16> {
    RIGHTS
        .iter()
        .find(|(_, found, _)| *found == letter)
        .map(|(right, ..)| *right)
        .ok_or_else(|| {
            invalid_text(format!(
                "invalid mode character: must be one of \"{RIGHT_LETTERS}\""
            ))
        })
}

type RoleLookup<'l> = dyn Fn(&str) -> SqlResult<u32> + 'l;

fn read_item<'t>(role_oid: &RoleLookup<'_>, text: &'t str) -> SqlResult<(AclItem, &'t str)> {
    let (mut grantee, mut rest) = read_name(text)?;
    if !rest.starts_with('=') {
        if grantee != "group" && grantee != "user" {
            return Err(
                invalid_text(format!("unrecognized key word: \"{grantee}\""))
                    .with_hint("ACL key word must be \"group\" or \"user\"."),
            );
        }
        (grantee, rest) = read_name(rest)?;
        if grantee.is_empty() {
            return Err(invalid_text("missing name")
                .with_hint("A name must follow the \"group\" or \"user\" key word."));
        }
    }
    let Some(privileges) = rest.strip_prefix('=') else {
        return Err(invalid_text("missing \"=\" sign"));
    };
    let mut rights = NO_RIGHTS;
    let mut grant_options = NO_RIGHTS;
    let mut last_read = NO_RIGHTS;
    let mut end = privileges.len();
    for (at, character) in privileges.char_indices() {
        if character == '*' {
            grant_options |= last_read;
        } else if character.is_ascii_alphabetic() {
            last_read = right_lettered(character)?;
        } else {
            end = at;
            break;
        }
        rights |= last_read;
    }
    let rest = privileges.get(end..).unwrap_or_default();
    let grantee = if grantee.is_empty() {
        PUBLIC_GRANTEE
    } else {
        role_oid(&grantee)?
    };
    let (grantor, rest) = match rest.strip_prefix('/') {
        Some(after_slash) => {
            let (grantor, rest) = read_name(after_slash)?;
            if grantor.is_empty() {
                return Err(invalid_text("a name must follow the \"/\" sign"));
            }
            (role_oid(&grantor)?, rest)
        }
        None => (oids::SESSION_USER, rest),
    };
    Ok((
        AclItem {
            grantee,
            grantor,
            rights,
            grant_options,
        },
        rest,
    ))
}

fn whole_item(role_oid: &RoleLookup<'_>, text: &str) -> SqlResult<AclItem> {
    let (item, rest) = read_item(role_oid, text)?;
    if rest.trim_start_matches(is_postgresql_space).is_empty() {
        Ok(item)
    } else {
        Err(invalid_text(
            "extra garbage at the end of the ACL specification",
        ))
    }
}

fn written_item(roles: &Roles<'_>, item: &AclItem) -> String {
    let mut out = String::new();
    if item.grantee != PUBLIC_GRANTEE {
        out.push_str(&roles.written(item.grantee));
    }
    out.push('=');
    for (right, letter, _) in RIGHTS {
        if item.rights & right != 0 {
            out.push(letter);
        }
        if item.grant_options & right != 0 {
            out.push('*');
        }
    }
    out.push('/');
    out.push_str(&roles.written(item.grantor));
    out
}

fn acl_array(roles: &Roles<'_>, items: &[AclItem]) -> Value {
    Value::Array(Box::new(ArrayValue::new(
        ArrayElement::AclItem,
        items
            .iter()
            .map(|item| Value::Text(written_item(roles, item)))
            .collect(),
    )))
}

fn listed_items(roles: &Roles<'_>, listed: &[Value]) -> SqlResult<Vec<AclItem>> {
    listed
        .iter()
        .map(|item| match item {
            Value::Text(text) => whole_item(&|name: &str| roles.oid_written(name), text),
            Value::Null => Err(SqlError::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "ACL arrays must not contain null values",
            )),
            other => Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("ACL array contains wrong data type {}", other.type_name()),
            )),
        })
        .collect()
}

fn items_of(roles: &Roles<'_>, value: &Value) -> SqlResult<Vec<AclItem>> {
    match value {
        Value::Array(array) => listed_items(roles, &array.items),
        Value::Text(text) => listed_items(
            roles,
            &crate::arrays::parse_text(text, ArrayElement::AclItem)?.items,
        ),
        other => Err(no_such_function(EXPLODE, &[other.sql_type()])),
    }
}

fn no_such_function(name: &str, types: &[Option<SqlType>]) -> SqlError {
    let types: Vec<String> = types
        .iter()
        .map(|sql_type| sql_type.map_or_else(|| "unknown".to_owned(), SqlType::name))
        .collect();
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name}({}) does not exist", types.join(", ")),
    )
    .with_hint(
        "No function matches the given name and argument types. You might need to add explicit type casts.",
    )
}

/// The result type of `aclexplode`: a record, for an `aclitem[]` (or a quoted array literal).
pub(crate) fn explode_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [None | Some(SqlType::Array(ArrayElement::AclItem))] => {
            Ok(SqlType::Defined(DefinedType::RECORD))
        }
        [Some(sql_type)] if sql_type.is_character() => Ok(SqlType::Defined(DefinedType::RECORD)),
        _ => Err(no_such_function(EXPLODE, types)),
    }
}

/// `aclexplode`'s rows as PostgreSQL lists them: each item of the list in order, and within it
/// each privilege in the order of its bit (`INSERT`, `SELECT`, `UPDATE`, ...), as grantor OID,
/// grantee OID (0 for `PUBLIC`), privilege name and whether it carries the grant option.
pub(crate) fn exploded_records(values: &[Value], sink: &mut RecordSink<'_>) -> SqlResult<()> {
    let [value] = values else {
        let types: Vec<Option<SqlType>> = values.iter().map(Value::sql_type).collect();
        return Err(no_such_function(EXPLODE, &types));
    };
    if value.is_null() {
        return Ok(());
    }
    let snapshot = snapshot::require()?;
    let roles = Roles {
        snapshot: &snapshot,
    };
    for item in items_of(&roles, value)? {
        for (right, _, name) in RIGHTS {
            if item.rights & right != 0 {
                sink(vec![
                    Value::Oid(item.grantor),
                    Value::Oid(item.grantee),
                    Value::Text(name.to_owned()),
                    Value::Boolean(item.grant_options & right != 0),
                ])?;
            }
        }
    }
    Ok(())
}

/// `aclexplode` in a select list: each row as a record whose fields are named as its columns.
pub(crate) fn exploded_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    exploded_records(values, &mut |fields| {
        sink(crate::records::record_value(
            crate::records::RECORD_OID,
            fields,
            EXPLODED_COLUMNS
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
        ))
    })
}

fn object_defaults(kind: &str) -> SqlResult<(u16, u16)> {
    let letter = kind.chars().next().unwrap_or_default();
    Ok(match letter {
        'c' => (NO_RIGHTS, NO_RIGHTS),
        'r' => (NO_RIGHTS, RELATION_RIGHTS),
        's' => (NO_RIGHTS, USAGE | SELECT | UPDATE),
        'd' => (TEMPORARY | CONNECT, CREATE | TEMPORARY | CONNECT),
        'f' => (EXECUTE, EXECUTE),
        'l' => (USAGE, USAGE),
        'L' => (NO_RIGHTS, SELECT | UPDATE),
        'n' => (NO_RIGHTS, USAGE | CREATE),
        'p' => (NO_RIGHTS, SET | ALTER_SYSTEM),
        't' => (NO_RIGHTS, CREATE),
        'F' | 'S' => (NO_RIGHTS, USAGE),
        'T' => (USAGE, USAGE),
        other => {
            let shown = if other == '\0' {
                String::new()
            } else {
                other.to_string()
            };
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                format!("unrecognized object type abbreviation: {shown}"),
            ));
        }
    })
}

fn oid_of(value: &Value) -> SqlResult<u32> {
    oid_number(value)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| SqlError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "OID out of range"))
}

fn default_acl(roles: &Roles<'_>, kind: &Value, owner: &Value) -> SqlResult<Value> {
    let (public_rights, owner_rights) = object_defaults(&kind.to_text().unwrap_or_default())?;
    let owner = oid_of(owner)?;
    let mut items = Vec::with_capacity(2);
    if public_rights != NO_RIGHTS {
        items.push(AclItem {
            grantee: PUBLIC_GRANTEE,
            grantor: owner,
            rights: public_rights,
            grant_options: NO_RIGHTS,
        });
    }
    if owner_rights != NO_RIGHTS {
        items.push(AclItem {
            grantee: owner,
            grantor: owner,
            rights: owner_rights,
            grant_options: NO_RIGHTS,
        });
    }
    Ok(acl_array(roles, &items))
}

fn named_rights(text: &str) -> SqlResult<u16> {
    text.split(',').try_fold(NO_RIGHTS, |rights, chunk| {
        let chunk = chunk.trim_matches(is_postgresql_space);
        NAMED_RIGHTS
            .iter()
            .find(|(name, _)| chunk.eq_ignore_ascii_case(name))
            .map(|(_, right)| rights | right)
            .ok_or_else(|| unrecognized(chunk))
    })
}

fn made_item(roles: &Roles<'_>, values: [&Value; 4]) -> SqlResult<Value> {
    let [grantee, grantor, privileges, grantable] = values;
    let grantee = oid_of(grantee)?;
    let grantor = oid_of(grantor)?;
    let rights = named_rights(&privileges.to_text().unwrap_or_default())?;
    let grant_options = if matches!(grantable, Value::Boolean(true)) {
        rights
    } else {
        NO_RIGHTS
    };
    Ok(Value::Text(written_item(
        roles,
        &AclItem {
            grantee,
            grantor,
            rights,
            grant_options,
        },
    )))
}

/// Whether `name` is `acldefault` or `makeaclitem`, the scalar functions this module answers.
pub(crate) fn is_scalar(name: &str) -> bool {
    matches!(name, DEFAULT | MAKE_ITEM)
}

/// `acldefault("char", owner)`: the access control list PostgreSQL assumes for an object of that
/// kind whose ACL column is NULL (`PUBLIC`'s item first, then the owner's); and
/// `makeaclitem(grantee, grantor, privileges, grantable)`: one `aclitem` from a comma-separated
/// privilege list. Any NULL argument gives NULL.
pub(crate) fn function(name: &str, values: &[Value]) -> SqlResult<Value> {
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let snapshot = snapshot::require()?;
    let roles = Roles {
        snapshot: &snapshot,
    };
    match (name, values) {
        (DEFAULT, [kind, owner]) => default_acl(&roles, kind, owner),
        (MAKE_ITEM, [grantee, grantor, privileges, grantable]) => {
            made_item(&roles, [grantee, grantor, privileges, grantable])
        }
        _ => {
            let types: Vec<Option<SqlType>> = values.iter().map(Value::sql_type).collect();
            Err(no_such_function(name, &types))
        }
    }
}

/// Reads PostgreSQL's `aclitem` input text (`grantee=privileges/grantor`, an empty grantee for
/// `PUBLIC`, `*` after a privilege for its grant option) and gives it back in output form, with
/// PostgreSQL's errors for a bad item or an unknown role.
pub(crate) fn item_input(text: &str) -> SqlResult<Value> {
    let snapshot = snapshot::require()?;
    let roles = Roles {
        snapshot: &snapshot,
    };
    let item = whole_item(&|name: &str| roles.oid_written(name), text)?;
    Ok(Value::Text(written_item(&roles, &item)))
}

/// `pg_namespace.nspacl`: what the engine enforces on a schema, written as PostgreSQL's ACL: the
/// owner holds `USAGE` and `CREATE`, and `PUBLIC` holds `USAGE` (every role may use every
/// schema); `pg_toast` keeps PostgreSQL's NULL.
pub(crate) fn schema_acl(snapshot: &CatalogSnapshot, schema: &SchemaEntry) -> Value {
    if schema.oid == oids::PG_TOAST {
        return Value::Null;
    }
    let roles = Roles { snapshot };
    acl_array(
        &roles,
        &[
            AclItem {
                grantee: schema.owner,
                grantor: schema.owner,
                rights: USAGE | CREATE,
                grant_options: NO_RIGHTS,
            },
            AclItem {
                grantee: PUBLIC_GRANTEE,
                grantor: schema.owner,
                rights: USAGE,
                grant_options: NO_RIGHTS,
            },
        ],
    )
}

#[cfg(test)]
fn role_numbered_by_length(name: &str) -> SqlResult<u32> {
    Ok(u32::try_from(name.len()).unwrap_or(u32::MAX))
}

#[cfg(test)]
pub(super) fn item_rights(text: &str) -> SqlResult<(u16, u16)> {
    whole_item(&role_numbered_by_length, text).map(|item| (item.rights, item.grant_options))
}
