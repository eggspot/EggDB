use crate::error::SqlResult;
use crate::types::{SqlType, Value};

/// The internal function a MySQL `SET` column's implicit check calls: whether every
/// comma-separated element of the value is one of the members.
pub(crate) const SET_MEMBERS_FUNCTION: &str = "eggdb_mysql_set_members";

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
    (name == SET_MEMBERS_FUNCTION).then_some(SqlType::Boolean)
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
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
    fn set_values_must_be_made_of_members() {
        assert!(set_members_valid("a,bb", "a,bb,ccc"));
        assert!(set_members_valid("", "a,bb,ccc"));
        assert!(set_members_valid("A", "a,bb,ccc"));
        assert!(!set_members_valid("a,d", "a,bb,ccc"));
    }
}
