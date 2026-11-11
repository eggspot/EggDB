use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

const LONGEST_RANDOM_STRING: usize = 4_000;
const ORACLE_INVALID_SQL_NAME: u16 = 44_003;
const ORACLE_INVALID_QUALIFIED_NAME: u16 = 44_004;
const ORACLE_NUMERIC_OR_VALUE: u16 = 6_502;
const JARO_WINKLER_PREFIX: usize = 4;
const LONGEST_RANDOM_BYTES: usize = 32_767;
const GET_TIME_WRAP: i64 = 1 << 31;
const CRYPTO_CONSTANTS: [(&str, i64); 11] = [
    ("hash_md4", 1),
    ("hash_md5", 2),
    ("hash_sh1", 3),
    ("hash_sh256", 4),
    ("hash_sh384", 5),
    ("hash_sh512", 6),
    ("hmac_md5", 1),
    ("hmac_sh1", 2),
    ("hmac_sh256", 3),
    ("hmac_sh384", 4),
    ("hmac_sh512", 5),
];
const JARO_WINKLER_SCALE: f64 = 0.1;

/// The internal function and argument counts for a function of `DBMS_RANDOM`, `DBMS_ASSERT` or
/// `UTL_MATCH`; `None` for another name.
pub(crate) fn internal_name(package: &str, function: &str) -> Option<(&'static str, usize, usize)> {
    Some(match (package, function) {
        ("dbms_random", "value") => ("oracle_dbms_random_value", 0, 2),
        ("dbms_random", "normal") => ("oracle_dbms_random_normal", 0, 0),
        ("dbms_random", "random") => ("oracle_dbms_random_random", 0, 0),
        ("dbms_random", "string") => ("oracle_dbms_random_string", 2, 2),
        ("dbms_assert", "noop") => ("oracle_dbms_assert_noop", 1, 1),
        ("dbms_assert", "simple_sql_name") => ("oracle_dbms_assert_simple_sql_name", 1, 1),
        ("dbms_assert", "qualified_sql_name") => ("oracle_dbms_assert_qualified_sql_name", 1, 1),
        ("dbms_assert", "enquote_name") => ("oracle_dbms_assert_enquote_name", 1, 2),
        ("dbms_assert", "enquote_literal") => ("oracle_dbms_assert_enquote_literal", 1, 1),
        ("utl_match", "edit_distance") => ("oracle_utl_match_edit_distance", 2, 2),
        ("utl_match", "edit_distance_similarity") => {
            ("oracle_utl_match_edit_distance_similarity", 2, 2)
        }
        ("utl_match", "jaro_winkler") => ("oracle_utl_match_jaro_winkler", 2, 2),
        ("utl_match", "jaro_winkler_similarity") => {
            ("oracle_utl_match_jaro_winkler_similarity", 2, 2)
        }
        ("dbms_crypto", "hash") => ("oracle_dbms_crypto_hash", 2, 2),
        ("dbms_crypto", "mac") => ("oracle_dbms_crypto_mac", 3, 3),
        ("dbms_crypto", "randombytes") => ("oracle_dbms_crypto_randombytes", 1, 1),
        ("dbms_utility", "get_time") => ("oracle_dbms_utility_get_time", 0, 0),
        _ => return None,
    })
}

/// Whether `function` of `package` may be written without parentheses, as
/// `DBMS_RANDOM.VALUE` often is.
pub(crate) fn takes_no_parentheses(package: &str, function: &str) -> bool {
    (package == "dbms_random" && matches!(function, "value" | "normal" | "random"))
        || (package == "dbms_utility" && function == "get_time")
}

/// The value of a package constant such as `DBMS_CRYPTO.HASH_SH256`; `None` for another name.
pub(crate) fn constant(package: &str, name: &str) -> Option<i64> {
    (package == "dbms_crypto")
        .then(|| {
            CRYPTO_CONSTANTS
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, value)| *value)
        })
        .flatten()
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "oracle_dbms_random_value"
        | "oracle_dbms_random_normal"
        | "oracle_utl_match_jaro_winkler" => SqlType::Double,
        "oracle_dbms_random_random"
        | "oracle_dbms_utility_get_time"
        | "oracle_utl_match_edit_distance"
        | "oracle_utl_match_edit_distance_similarity"
        | "oracle_utl_match_jaro_winkler_similarity" => SqlType::Numeric(None),
        "oracle_dbms_random_string"
        | "oracle_dbms_assert_noop"
        | "oracle_dbms_assert_simple_sql_name"
        | "oracle_dbms_assert_qualified_sql_name"
        | "oracle_dbms_assert_enquote_name"
        | "oracle_dbms_assert_enquote_literal" => SqlType::Text,
        "oracle_dbms_crypto_hash" | "oracle_dbms_crypto_mac" | "oracle_dbms_crypto_randombytes" => {
            SqlType::Bytea
        }
        _ => return None,
    })
}

fn number(value: i64) -> Value {
    Value::Numeric(crate::numeric::Numeric::from_i64(value))
}

fn unit_random() -> SqlResult<f64> {
    let mut bytes = [0u8; 8];
    crate::random::fill(&mut bytes)?;
    Ok((u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64)
}

fn random_string(option: &str, length: usize) -> SqlResult<String> {
    let alphabet: Vec<char> = match option.chars().next().map(|kind| kind.to_ascii_lowercase()) {
        Some('u') => ('A'..='Z').collect(),
        Some('l') => ('a'..='z').collect(),
        Some('a') => ('A'..='Z').chain('a'..='z').collect(),
        Some('x') => ('A'..='Z').chain('0'..='9').collect(),
        _ => (' '..='~').collect(),
    };
    let mut out = String::with_capacity(length);
    let mut bytes = vec![0u8; length * 2];
    crate::random::fill(&mut bytes)?;
    for pair in bytes.chunks(2) {
        let index = usize::from(u16::from_le_bytes([pair[0], pair[1]])) % alphabet.len();
        out.push(alphabet[index]);
    }
    Ok(out)
}

fn invalid_name(code: u16, name: &str) -> SqlError {
    let what = if code == ORACLE_INVALID_SQL_NAME {
        "invalid SQL name"
    } else {
        "invalid qualified SQL name"
    };
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("ORA-{code:05}: {what}: {name}"),
    )
    .with_oracle_code(code)
}

fn simple_name(name: &str) -> bool {
    let name = name.trim();
    if let Some(inner) = name.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
        return !inner.is_empty() && !inner.contains('"');
    }
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_alphabetic())
        && characters.all(|character| character.is_alphanumeric() || "_$#".contains(character))
}

fn qualified_name(name: &str) -> bool {
    let (local, link) = match name.split_once('@') {
        Some((local, link)) => (local, Some(link)),
        None => (name, None),
    };
    local.split('.').all(simple_name) && link.is_none_or(|link| link.split('.').all(simple_name))
}

fn edit_distance(left: &[char], right: &[char]) -> usize {
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, left_char) in left.iter().enumerate() {
        let mut current = vec![row + 1; right.len() + 1];
        for (column, right_char) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != right_char);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        previous = current;
    }
    previous[right.len()]
}

fn jaro_winkler(left: &[char], right: &[char]) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let window = (left.len().max(right.len()) / 2).saturating_sub(1);
    let mut left_matched = vec![false; left.len()];
    let mut right_matched = vec![false; right.len()];
    let mut matches = 0usize;
    for (index, character) in left.iter().enumerate() {
        let from = index.saturating_sub(window);
        let to = (index + window + 1).min(right.len());
        for other in from..to {
            if !right_matched[other] && right[other] == *character {
                left_matched[index] = true;
                right_matched[other] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let left_order: Vec<char> = left
        .iter()
        .zip(&left_matched)
        .filter(|(_, matched)| **matched)
        .map(|(character, _)| *character)
        .collect();
    let right_order: Vec<char> = right
        .iter()
        .zip(&right_matched)
        .filter(|(_, matched)| **matched)
        .map(|(character, _)| *character)
        .collect();
    let transpositions = left_order
        .iter()
        .zip(&right_order)
        .filter(|(a, b)| a != b)
        .count()
        / 2;
    let matches = matches as f64;
    let jaro = (matches / left.len() as f64
        + matches / right.len() as f64
        + (matches - transpositions as f64) / matches)
        / 3.0;
    let prefix = left
        .iter()
        .zip(right)
        .take(JARO_WINKLER_PREFIX)
        .take_while(|(a, b)| a == b)
        .count();
    jaro + prefix as f64 * JARO_WINKLER_SCALE * (1.0 - jaro)
}

fn text(value: &Value) -> Option<String> {
    value.to_text()
}

/// Runs the internal function `name` on `values`; `None` when `name` is not one of these
/// packages' functions.
pub(crate) fn call(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !name.starts_with("oracle_dbms_random_")
        && !name.starts_with("oracle_dbms_assert_")
        && !name.starts_with("oracle_utl_match_")
        && !name.starts_with("oracle_dbms_crypto_")
        && !name.starts_with("oracle_dbms_utility_")
    {
        return None;
    }
    Some(evaluate(name, values))
}

fn evaluate(name: &str, values: &[Value]) -> SqlResult<Value> {
    match (name, values) {
        ("oracle_dbms_random_value", []) => Ok(Value::Double(unit_random()?)),
        ("oracle_dbms_random_value", [low, high]) => {
            let (Some(low), Some(high)) = (low.as_float(), high.as_float()) else {
                return Ok(Value::Null);
            };
            Ok(Value::Double(low + (high - low) * unit_random()?))
        }
        ("oracle_dbms_random_normal", []) => {
            let first = unit_random()?.max(f64::MIN_POSITIVE);
            let second = unit_random()?;
            Ok(Value::Double(
                (-2.0 * first.ln()).sqrt() * (std::f64::consts::TAU * second).cos(),
            ))
        }
        ("oracle_dbms_random_random", []) => {
            let mut bytes = [0u8; 4];
            crate::random::fill(&mut bytes)?;
            Ok(number(i64::from(i32::from_le_bytes(bytes))))
        }
        ("oracle_dbms_random_string", [option, length]) => {
            let option = text(option).unwrap_or_default();
            let length = length
                .as_float()
                .filter(|length| length.is_finite() && *length > 0.0)
                .map_or(0, |length| (length as usize).min(LONGEST_RANDOM_STRING));
            if length == 0 {
                return Ok(Value::Null);
            }
            Ok(Value::Text(random_string(&option, length)?))
        }
        ("oracle_dbms_assert_noop", [value]) => Ok(value.clone()),
        ("oracle_dbms_assert_simple_sql_name", [value]) => match text(value) {
            Some(name) if simple_name(&name) => Ok(Value::Text(name)),
            other => Err(invalid_name(ORACLE_INVALID_SQL_NAME, &other.unwrap_or_default())),
        },
        ("oracle_dbms_assert_qualified_sql_name", [value]) => match text(value) {
            Some(name) if qualified_name(&name) => Ok(Value::Text(name)),
            other => Err(invalid_name(
                ORACLE_INVALID_QUALIFIED_NAME,
                &other.unwrap_or_default(),
            )),
        },
        ("oracle_dbms_assert_enquote_name", [value, rest @ ..]) => {
            let Some(name) = text(value) else {
                return Ok(Value::Text("\"\"".to_owned()));
            };
            if name.starts_with('"') && name.ends_with('"') && name.len() >= 2 {
                return Ok(Value::Text(name));
            }
            if name.contains('"') {
                return Err(invalid_name(ORACLE_INVALID_SQL_NAME, &name));
            }
            let capitalize = !matches!(rest.first(), Some(Value::Boolean(false)));
            let shown = if capitalize { name.to_uppercase() } else { name };
            Ok(Value::Text(format!("\"{shown}\"")))
        }
        ("oracle_dbms_assert_enquote_literal", [value]) => {
            let literal = text(value).unwrap_or_default();
            let inner = literal
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
                .unwrap_or(&literal);
            if inner.replace("''", "").contains('\'') {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "ORA-06502: PL/SQL: numeric or value error",
                )
                .with_oracle_code(ORACLE_NUMERIC_OR_VALUE));
            }
            Ok(Value::Text(format!("'{inner}'")))
        }
        ("oracle_utl_match_edit_distance", [left, right]) => {
            let (Some(left), Some(right)) = (text(left), text(right)) else {
                return Ok(number(-1));
            };
            let left: Vec<char> = left.chars().collect();
            let right: Vec<char> = right.chars().collect();
            Ok(number(edit_distance(&left, &right) as i64))
        }
        ("oracle_utl_match_edit_distance_similarity", [left, right]) => {
            let left: Vec<char> = text(left).unwrap_or_default().chars().collect();
            let right: Vec<char> = text(right).unwrap_or_default().chars().collect();
            let longest = left.len().max(right.len());
            if longest == 0 {
                return Ok(number(100));
            }
            let distance = edit_distance(&left, &right);
            Ok(number(100 - (distance * 100 / longest) as i64))
        }
        ("oracle_utl_match_jaro_winkler", [left, right]) => {
            let left: Vec<char> = text(left).unwrap_or_default().chars().collect();
            let right: Vec<char> = text(right).unwrap_or_default().chars().collect();
            Ok(Value::Double(jaro_winkler(&left, &right)))
        }
        ("oracle_utl_match_jaro_winkler_similarity", [left, right]) => {
            let left: Vec<char> = text(left).unwrap_or_default().chars().collect();
            let right: Vec<char> = text(right).unwrap_or_default().chars().collect();
            Ok(number((jaro_winkler(&left, &right) * 100.0).floor() as i64))
        }
        ("oracle_dbms_crypto_hash", [source, kind]) => {
            let Some(bytes) = crypto_input(source) else {
                return Ok(Value::Null);
            };
            let digest = match kind.as_integer() {
                Some(2) => eggdb_crypto::md5_checksum(&bytes).to_vec(),
                Some(3) => eggdb_crypto::sha1(&bytes).to_vec(),
                Some(4) => eggdb_crypto::sha256(&bytes).to_vec(),
                Some(5) => eggdb_crypto::sha384(&bytes).to_vec(),
                Some(6) => eggdb_crypto::sha512(&bytes).to_vec(),
                _ => return Err(unsupported_algorithm()),
            };
            Ok(Value::Bytea(digest))
        }
        ("oracle_dbms_crypto_mac", [source, kind, key]) => {
            let (Some(bytes), Some(key)) = (crypto_input(source), crypto_input(key)) else {
                return Ok(Value::Null);
            };
            let mac = match kind.as_integer() {
                Some(2) => eggdb_crypto::hmac::<eggdb_crypto::Sha1>(&key, &bytes).to_vec(),
                Some(3) => eggdb_crypto::hmac::<eggdb_crypto::Sha256>(&key, &bytes).to_vec(),
                Some(4) => eggdb_crypto::hmac::<eggdb_crypto::Sha384>(&key, &bytes).to_vec(),
                Some(5) => eggdb_crypto::hmac::<eggdb_crypto::Sha512>(&key, &bytes).to_vec(),
                _ => return Err(unsupported_algorithm()),
            };
            Ok(Value::Bytea(mac))
        }
        ("oracle_dbms_crypto_randombytes", [count]) => {
            let count = count
                .as_float()
                .filter(|count| count.is_finite() && *count >= 1.0)
                .map_or(0, |count| count as usize);
            if count == 0 || count > LONGEST_RANDOM_BYTES {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "ORA-28234: key length too short",
                )
                .with_oracle_code(28_234));
            }
            let mut bytes = vec![0u8; count];
            crate::random::fill(&mut bytes)?;
            Ok(Value::Bytea(bytes))
        }
        ("oracle_dbms_utility_get_time", []) => {
            let hundredths = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| i64::try_from(since.as_millis() / 10).unwrap_or(0));
            Ok(number(hundredths.rem_euclid(GET_TIME_WRAP)))
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("wrong number of arguments for {name}"),
        )),
    }
}

fn crypto_input(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::Bytea(bytes) => Some(bytes.clone()),
        other => other.to_text().map(String::into_bytes),
    }
}

fn unsupported_algorithm() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "ORA-28827: The specified cipher suite is not defined",
    )
    .with_oracle_code(28_827)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_value(text: &str) -> Value {
        Value::Text(text.to_owned())
    }

    #[test]
    fn match_functions_follow_their_definitions() {
        let distance = evaluate(
            "oracle_utl_match_edit_distance",
            &[text_value("kitten"), text_value("sitting")],
        );
        assert_eq!(distance.unwrap(), number(3));
        let similar = evaluate(
            "oracle_utl_match_jaro_winkler_similarity",
            &[text_value("MARTHA"), text_value("MARHTA")],
        );
        assert_eq!(similar.unwrap(), number(96));
    }

    #[test]
    fn crypto_hashes_match_their_published_digests() {
        let digest = evaluate(
            "oracle_dbms_crypto_hash",
            &[Value::Bytea(b"abc".to_vec()), number(4)],
        )
        .unwrap();
        let Value::Bytea(bytes) = digest else {
            panic!("a RAW digest");
        };
        assert_eq!(bytes[..4], [0xba, 0x78, 0x16, 0xbf]);
        assert_eq!(constant("dbms_crypto", "hash_sh256"), Some(4));
        assert!(evaluate("oracle_dbms_crypto_hash", &[Value::Bytea(Vec::new()), number(1)]).is_err());
    }

    #[test]
    fn assert_functions_check_and_quote_names() {
        assert!(evaluate("oracle_dbms_assert_simple_sql_name", &[text_value("emp_1")]).is_ok());
        assert!(evaluate("oracle_dbms_assert_simple_sql_name", &[text_value("1; drop")]).is_err());
        assert!(
            evaluate("oracle_dbms_assert_qualified_sql_name", &[text_value("hr.emp@remote")])
                .is_ok()
        );
        assert_eq!(
            evaluate("oracle_dbms_assert_enquote_name", &[text_value("emp")]).unwrap(),
            text_value("\"EMP\"")
        );
        assert_eq!(
            evaluate("oracle_dbms_assert_enquote_literal", &[text_value("O''Brien")]).unwrap(),
            text_value("'O''Brien'")
        );
        assert!(evaluate("oracle_dbms_assert_enquote_literal", &[text_value("O'Brien")]).is_err());
    }
}
