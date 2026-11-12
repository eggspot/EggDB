//! SQL Server's `hierarchyid`: a position in a tree written `/1/3/2.1/`, stored as bytes that
//! sort in depth-first order, so a parent sorts before its children and siblings in their
//! numeric order, as SQL Server sorts them (D797).

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

pub(crate) const PARSE: &str = "hierarchyid_parse";
pub(crate) const GET_ROOT: &str = "hierarchyid_get_root";
const TO_STRING: &str = "hierarchyid_to_string";
const GET_LEVEL: &str = "hierarchyid_get_level";
const GET_ANCESTOR: &str = "hierarchyid_get_ancestor";
const GET_DESCENDANT: &str = "hierarchyid_get_descendant";
const IS_DESCENDANT_OF: &str = "hierarchyid_is_descendant_of";
const GET_REPARENTED_VALUE: &str = "hierarchyid_get_reparented_value";
const END_OF_PATH: u8 = 0;
const END_OF_STEP: u8 = 1;
const NUMBER: u8 = 2;
const MOST_LEVELS: usize = 1_000;

const METHODS: [(&str, &str); 6] = [
    ("tostring", TO_STRING),
    ("getlevel", GET_LEVEL),
    ("getancestor", GET_ANCESTOR),
    ("getdescendant", GET_DESCENDANT),
    ("isdescendantof", IS_DESCENDANT_OF),
    ("getreparentedvalue", GET_REPARENTED_VALUE),
];

type Step = Vec<i64>;
type Path = Vec<Step>;

/// The internal function an instance method of `hierarchyid` calls, its target first.
pub(crate) fn method_function(name: &str) -> Option<&'static str> {
    let lowered = name.to_ascii_lowercase();
    METHODS
        .iter()
        .find(|(method, _)| *method == lowered)
        .map(|(_, function)| *function)
}

/// The internal function a static method (`hierarchyid::GetRoot()`, `hierarchyid::Parse(s)`)
/// calls.
pub(crate) fn static_function(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "getroot" => Some(GET_ROOT),
        "parse" => Some(PARSE),
        _ => None,
    }
}

fn invalid(text: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!(
            "24001: SqlHierarchyId.Parse failed because the input string '{text}' is not a valid string representation of a SqlHierarchyId node."
        ),
    )
    .with_sqlserver_code(6522)
}

fn damaged() -> SqlError {
    SqlError::new(
        SqlState::INVALID_BINARY_REPRESENTATION,
        "the hierarchyid value is damaged",
    )
}

pub(crate) fn parse(text: &str) -> SqlResult<Path> {
    let trimmed = text.trim();
    if trimmed == "/" {
        return Ok(Vec::new());
    }
    let inner = trimmed
        .strip_prefix('/')
        .and_then(|rest| rest.strip_suffix('/'))
        .ok_or_else(|| invalid(text))?;
    let mut path = Vec::new();
    for step in inner.split('/') {
        let numbers = step
            .split('.')
            .map(|number| number.parse::<i64>().map_err(|_| invalid(text)))
            .collect::<SqlResult<Step>>()?;
        if numbers.is_empty() {
            return Err(invalid(text));
        }
        path.push(numbers);
    }
    if path.len() > MOST_LEVELS {
        return Err(invalid(text));
    }
    Ok(path)
}

pub(crate) fn format(path: &[Step]) -> String {
    let mut out = String::from("/");
    for step in path {
        let numbers: Vec<String> = step.iter().map(i64::to_string).collect();
        out.push_str(&numbers.join("."));
        out.push('/');
    }
    out
}

pub(crate) fn encode(path: &[Step]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() * 10 + 1);
    for step in path {
        for number in step {
            out.push(NUMBER);
            out.extend_from_slice(&((*number as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
        }
        out.push(END_OF_STEP);
    }
    out.push(END_OF_PATH);
    out
}

pub(crate) fn decode(bytes: &[u8]) -> SqlResult<Path> {
    let mut path = Vec::new();
    let mut step = Vec::new();
    let mut at = 0;
    loop {
        match bytes.get(at) {
            Some(&NUMBER) => {
                let word: [u8; 8] = bytes
                    .get(at + 1..at + 9)
                    .and_then(|word| word.try_into().ok())
                    .ok_or_else(damaged)?;
                step.push((u64::from_be_bytes(word) ^ 0x8000_0000_0000_0000) as i64);
                at += 9;
            }
            Some(&END_OF_STEP) if !step.is_empty() => {
                path.push(std::mem::take(&mut step));
                at += 1;
            }
            Some(&END_OF_PATH) if step.is_empty() && at + 1 == bytes.len() => return Ok(path),
            _ => return Err(damaged()),
        }
        if path.len() > MOST_LEVELS {
            return Err(damaged());
        }
    }
}

fn path_of(value: &Value) -> SqlResult<Option<Path>> {
    match value {
        Value::Null => Ok(None),
        Value::Bytea(bytes) => decode(bytes).map(Some),
        other => match other.to_text() {
            Some(text) => parse(&text).map(Some),
            None => Ok(None),
        },
    }
}

fn hierarchy(path: &[Step]) -> Value {
    Value::Bytea(encode(path))
}

fn next_after(step: &[i64]) -> Step {
    vec![step.first().copied().unwrap_or(0).saturating_add(1)]
}

fn before(step: &[i64]) -> Step {
    vec![step.first().copied().unwrap_or(0).saturating_sub(1)]
}

fn between(low: &[i64], high: &[i64]) -> Step {
    let (Some(&low_first), Some(&high_first)) = (low.first(), high.first()) else {
        return low.to_vec();
    };
    if high_first - low_first > 1 {
        return vec![low_first + 1];
    }
    let mut middle = low.to_vec();
    match middle.last_mut() {
        Some(last) if low.len() > 1 => *last += 1,
        _ => middle.push(1),
    }
    middle
}

fn descendant(parent: &[Step], low: Option<&Path>, high: Option<&Path>) -> SqlResult<Path> {
    let child_of = |child: &Path| child.len() == parent.len() + 1 && child.starts_with(parent);
    let wrong = || {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "6522: GetDescendant failed because its arguments are not children of the node, or the first is not below the second.",
        )
        .with_sqlserver_code(6522)
    };
    let last = |path: &Path| path.last().cloned().unwrap_or_default();
    let step = match (low, high) {
        (None, None) => vec![1],
        (Some(low), None) if child_of(low) => next_after(&last(low)),
        (None, Some(high)) if child_of(high) => before(&last(high)),
        (Some(low), Some(high))
            if child_of(low) && child_of(high) && encode(low) < encode(high) =>
        {
            between(&last(low), &last(high))
        }
        _ => return Err(wrong()),
    };
    let mut child = parent.to_vec();
    child.push(step);
    Ok(child)
}

fn level(value: usize) -> Value {
    Value::SmallInt(i16::try_from(value).unwrap_or(i16::MAX))
}

/// Runs one of `hierarchyid`'s methods, by its internal name.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let outcome = match name {
        GET_ROOT => Ok(hierarchy(&[])),
        PARSE => match values {
            [Value::Null] => Ok(Value::Null),
            [text] => parse(&text.to_text().unwrap_or_default()).map(|path| hierarchy(&path)),
            _ => return None,
        },
        TO_STRING | GET_LEVEL => match values {
            [value] => path_of(value).map(|path| match path {
                None => Value::Null,
                Some(path) if name == TO_STRING => Value::Text(format(&path)),
                Some(path) => level(path.len()),
            }),
            _ => return None,
        },
        GET_ANCESTOR => match values {
            [node, generations] => (|| {
                let (Some(path), Some(generations)) = (path_of(node)?, generations.as_integer())
                else {
                    return Ok(Value::Null);
                };
                let Ok(generations) = usize::try_from(generations) else {
                    return Ok(Value::Null);
                };
                Ok(match path.len().checked_sub(generations) {
                    Some(keep) => hierarchy(&path[..keep]),
                    None => Value::Null,
                })
            })(),
            _ => return None,
        },
        IS_DESCENDANT_OF => match values {
            [node, parent] => (|| {
                let (Some(node), Some(parent)) = (path_of(node)?, path_of(parent)?) else {
                    return Ok(Value::Null);
                };
                Ok(Value::Boolean(node.starts_with(&parent)))
            })(),
            _ => return None,
        },
        GET_DESCENDANT => match values {
            [parent, low, high] => (|| {
                let Some(parent) = path_of(parent)? else {
                    return Ok(Value::Null);
                };
                let child = descendant(&parent, path_of(low)?.as_ref(), path_of(high)?.as_ref())?;
                Ok(hierarchy(&child))
            })(),
            _ => return None,
        },
        GET_REPARENTED_VALUE => match values {
            [node, old_root, new_root] => (|| {
                let (Some(node), Some(old_root), Some(new_root)) =
                    (path_of(node)?, path_of(old_root)?, path_of(new_root)?)
                else {
                    return Ok(Value::Null);
                };
                if !node.starts_with(&old_root) {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "6522: GetReparentedValue failed because the old root is not an ancestor of the node.",
                    )
                    .with_sqlserver_code(6522));
                }
                let mut moved = new_root;
                moved.extend_from_slice(&node[old_root.len()..]);
                Ok(hierarchy(&moved))
            })(),
            _ => return None,
        },
        _ => return None,
    };
    Some(outcome)
}

/// The SQL type a `hierarchyid` method returns.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    match name {
        TO_STRING => Some(SqlType::Text),
        GET_LEVEL => Some(SqlType::SmallInt),
        IS_DESCENDANT_OF => Some(SqlType::Boolean),
        GET_ROOT | PARSE | GET_ANCESTOR | GET_DESCENDANT | GET_REPARENTED_VALUE => {
            Some(SqlType::Bytea)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(text: &str) -> Value {
        Value::Bytea(encode(&parse(text).expect("a path")))
    }

    fn shown(value: Value) -> String {
        match function(TO_STRING, &[value]) {
            Some(Ok(Value::Text(text))) => text,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn paths_round_trip_and_sort_depth_first() {
        for text in ["/", "/1/", "/1/3/", "/2/1.1/", "/-1/"] {
            assert_eq!(format(&parse(text).unwrap()), text);
            assert_eq!(decode(&encode(&parse(text).unwrap())).unwrap(), parse(text).unwrap());
        }
        let order = ["/", "/1/", "/1/1/", "/1/1.1/", "/1/2/", "/2/", "/10/"];
        for pair in order.windows(2) {
            assert!(encode(&parse(pair[0]).unwrap()) < encode(&parse(pair[1]).unwrap()), "{pair:?}");
        }
        assert!(parse("1/2").is_err());
        assert!(parse("/a/").is_err());
        assert!(decode(&[NUMBER, 1]).is_err());
    }

    #[test]
    fn methods_walk_the_tree() {
        let manager = node("/1/");
        let report = node("/1/2/");
        assert_eq!(function(GET_LEVEL, &[report.clone()]).unwrap().unwrap(), Value::SmallInt(2));
        assert_eq!(shown(function(GET_ANCESTOR, &[report.clone(), Value::Integer(1)]).unwrap().unwrap()), "/1/");
        assert_eq!(
            function(IS_DESCENDANT_OF, &[report.clone(), manager.clone()]).unwrap().unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            shown(function(GET_DESCENDANT, &[manager.clone(), report.clone(), Value::Null]).unwrap().unwrap()),
            "/1/3/"
        );
        assert_eq!(
            shown(function(GET_DESCENDANT, &[manager.clone(), node("/1/2/"), node("/1/3/")]).unwrap().unwrap()),
            "/1/2.1/"
        );
        assert_eq!(
            shown(function(GET_REPARENTED_VALUE, &[report, manager, node("/5/")]).unwrap().unwrap()),
            "/5/2/"
        );
    }

    #[test]
    fn a_hierarchyid_column_sorts_depth_first_and_answers_its_methods() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::with_dialect(&store, crate::Dialect::SqlServer);
        let mut run = |sql: &str| {
            let mut outputs = Vec::new();
            session
                .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
                .unwrap_or_else(|error| panic!("{sql}: {error}"));
            outputs.pop().unwrap_or_default()
        };
        run("CREATE TABLE org (node hierarchyid PRIMARY KEY, name nvarchar(50))");
        run("INSERT INTO org VALUES (hierarchyid::Parse('/10/'), 'ops'), (hierarchyid::GetRoot(), 'ceo'), (hierarchyid::Parse('/2/'), 'cto'), (hierarchyid::Parse('/2/1/'), 'dev')");
        let listed = run("SELECT node.ToString() AS path, node.GetLevel() AS depth FROM org ORDER BY node");
        let paths: Vec<&str> = listed.lines().skip(1).filter_map(|line| line.split(" | ").next()).collect();
        assert_eq!(&paths[..4], &["/", "/2/", "/2/1/", "/10/"]);
        let below = run("SELECT name FROM org WHERE node.IsDescendantOf(hierarchyid::Parse('/2/')) = 1 ORDER BY node");
        assert!(below.contains("cto") && below.contains("dev") && !below.contains("ops"));
    }
}

