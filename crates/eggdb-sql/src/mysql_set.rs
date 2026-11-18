use crate::error::SqlResult;
use crate::types::{SqlType, Value};

/// The internal function a MySQL `SET` column's implicit check calls: whether every
/// comma-separated element of the value is one of the members.
pub(crate) const SET_MEMBERS_FUNCTION: &str = "eggdb_mysql_set_members";
/// `ENUM` ordering (D1253): the position of its first argument among the others, from 1, 0
/// when it is none of them, NULL for NULL, as MySQL sorts an `ENUM` by its member index.
pub(crate) const ENUM_POSITION_FUNCTION: &str = "eggdb_mysql_enum_position";

/// The internal function a `SET` column's write trigger calls to store a value as MySQL does.
pub(crate) const SET_NORMALIZE_FUNCTION: &str = "eggdb_mysql_set_normalize";

const MOST_MEMBERS: usize = 64;

/// A `SET` value as MySQL stores it: its members in the column's order, each once, spelled
/// as the column declares them; a number is read as a bit mask over the members, bit 0 the
/// first. A value with an element that is not a member is returned unchanged, for the column's
/// check to refuse.
pub(crate) fn normalized(value: &str, members: &str) -> String {
    let members: Vec<&str> = members.split(',').take(MOST_MEMBERS).collect();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let Ok(mask) = value.parse::<u64>() else {
            return value.to_owned();
        };
        let fits = members.len() >= 64 || mask < (1u64 << members.len());
        if !fits {
            return value.to_owned();
        }
        let chosen: Vec<&str> = members
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1u64 << index) != 0)
            .map(|(_, member)| *member)
            .collect();
        return chosen.join(",");
    }
    if value.is_empty() {
        return String::new();
    }
    let mut positions = Vec::new();
    for element in value.split(',') {
        match members.iter().position(|member| member.eq_ignore_ascii_case(element)) {
            Some(position) => positions.push(position),
            None => return value.to_owned(),
        }
    }
    positions.sort_unstable();
    positions.dedup();
    positions
        .into_iter()
        .map(|position| members[position])
        .collect::<Vec<_>>()
        .join(",")
}

/// Whether every element of a `SET` value is a member, compared case-insensitively as MySQL's
/// default collations compare them; the empty set is valid.
pub(crate) fn set_members_valid(value: &str, members: &str) -> bool {
    let members: Vec<&str> = members.split(',').collect();
    value.is_empty()
        || value
            .split(',')
            .all(|element| members.iter().any(|member| member.eq_ignore_ascii_case(element)))
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    match name {
        SET_MEMBERS_FUNCTION => Some(SqlType::Boolean),
        SET_NORMALIZE_FUNCTION => Some(SqlType::Text),
        ENUM_POSITION_FUNCTION => Some(SqlType::Integer),
        _ => None,
    }
}

/// The position of `values[0]` among `values[1..]` (D1253).
pub(crate) fn enum_position(values: &[Value]) -> Value {
    let Some((value, members)) = values.split_first() else {
        return Value::Null;
    };
    let Some(value) = value.to_text() else {
        return Value::Null;
    };
    let position = members
        .iter()
        .position(|member| member.to_text().is_some_and(|member| member.eq_ignore_ascii_case(&value)))
        .map_or(0, |at| at + 1);
    Value::Integer(i32::try_from(position).unwrap_or(i32::MAX))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if name == ENUM_POSITION_FUNCTION {
        return Some(Ok(enum_position(values)));
    }
    if name == SET_NORMALIZE_FUNCTION {
        return Some(Ok(match values {
            [value, members] => match (value.to_text(), members.to_text()) {
                (Some(value), Some(members)) => Value::Text(normalized(&value, &members)),
                _ => Value::Null,
            },
            _ => Value::Null,
        }));
    }
    if name != SET_MEMBERS_FUNCTION {
        return None;
    }
    Some(Ok(match values {
        [value, members] => match (value.to_text(), members.to_text()) {
            (Some(value), Some(members)) => Value::Boolean(set_members_valid(&value, &members)),
            _ => Value::Null,
        },
        _ => Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enum_value_sorts_by_its_member_position() {
        let members = [Value::Text("small".to_owned()), Value::Text("Medium".to_owned()), Value::Text("large".to_owned())];
        let at = |value: Value| enum_position(&std::iter::once(value).chain(members.iter().cloned()).collect::<Vec<_>>());
        assert_eq!(at(Value::Text("large".to_owned())), Value::Integer(3));
        assert_eq!(at(Value::Text("medium".to_owned())), Value::Integer(2));
        assert_eq!(at(Value::Text("".to_owned())), Value::Integer(0));
        assert_eq!(at(Value::Null), Value::Null);
    }

    #[test]
    fn set_values_must_be_made_of_members() {
        assert!(set_members_valid("a,bb", "a,bb,ccc"));
        assert!(set_members_valid("", "a,bb,ccc"));
        assert!(set_members_valid("A", "a,bb,ccc"));
        assert!(!set_members_valid("a,d", "a,bb,ccc"));
    }

    #[test]
    fn set_values_are_stored_in_member_order_once_each() {
        assert_eq!(normalized("ccc,A,a,bb", "a,bb,ccc"), "a,bb,ccc");
        assert_eq!(normalized("", "a,bb,ccc"), "");
        assert_eq!(normalized("5", "a,bb,ccc"), "a,ccc");
        assert_eq!(normalized("8", "a,bb,ccc"), "8");
        assert_eq!(normalized("a,d", "a,bb,ccc"), "a,d");
    }
}
