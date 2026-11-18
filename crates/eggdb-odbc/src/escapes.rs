//! ODBC escape sequences (D1255): what applications write in braces, translated into the
//! connection's own SQL before a statement runs. `{d '...'}`, `{t '...'}` and `{ts '...'}` are
//! date and time literals, `{fn NAME(...)}` a scalar function from ODBC's list, `{oj ...}` an
//! outer join, `{escape '...'}` a `LIKE` escape character and `{call ...}` a procedure call.
//! Text in quotes is left alone; an escape inside another is translated first.

/// The family of SQL a connection speaks, which decides the words an escape becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Sqlite,
    MySql,
    Other,
}

impl Family {
    pub(crate) fn of(dialect: &str) -> Self {
        match dialect {
            "sqlite" => Self::Sqlite,
            "mysql" | "mariadb" => Self::MySql,
            _ => Self::Other,
        }
    }
}

/// `SQL_STRING_FUNCTIONS`: the `SQL_FN_STR_*` bits of the functions translated.
pub(crate) const STRING_FUNCTIONS: u32 =
    0x1 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x200 | 0x400 | 0x800 | 0x1000 | 0x2000 | 0x4000 | 0x40000 | 0x10_0000 | 0x20_0000 | 0x40_0000;
/// `SQL_NUMERIC_FUNCTIONS`: the `SQL_FN_NUM_*` bits.
pub(crate) const NUMERIC_FUNCTIONS: u32 = 0x1 | 0x20 | 0x100 | 0x200 | 0x400 | 0x800 | 0x1000 | 0x4000 | 0x1_0000 | 0x8_0000 | 0x10_0000 | 0x40_0000;
/// `SQL_TIMEDATE_FUNCTIONS`: the `SQL_FN_TD_*` bits.
pub(crate) const TIMEDATE_FUNCTIONS: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x200 | 0x400 | 0x800 | 0x1000 | 0x2_0000 | 0x4_0000 | 0x8_0000;
/// `SQL_SYSTEM_FUNCTIONS`: `SQL_FN_SYS_IFNULL`.
pub(crate) const SYSTEM_FUNCTIONS: u32 = 0x4;
/// `SQL_CONVERT_FUNCTIONS`: `SQL_FN_CVT_CONVERT` and `SQL_FN_CVT_CAST`.
pub(crate) const CONVERT_FUNCTIONS: u32 = 0x1 | 0x2;

/// `text` with every escape sequence outside quotes translated for `family`.
///
/// # Errors
///
/// An escape without its closing brace, or one whose keyword ODBC does not define.
pub(crate) fn translated(text: &str, family: Family) -> Result<String, String> {
    if !text.contains('{') {
        return Ok(text.to_owned());
    }
    let characters: Vec<char> = text.chars().collect();
    let mut at = 0;
    translate_until(&characters, &mut at, family, false)
}

fn translate_until(characters: &[char], at: &mut usize, family: Family, inside: bool) -> Result<String, String> {
    let mut out = String::new();
    while *at < characters.len() {
        let character = characters[*at];
        match character {
            '\'' | '"' => {
                out.push(character);
                *at += 1;
                while *at < characters.len() {
                    let quoted = characters[*at];
                    out.push(quoted);
                    *at += 1;
                    if quoted == character {
                        break;
                    }
                }
            }
            '{' => {
                *at += 1;
                let body = translate_until(characters, at, family, true)?;
                out.push_str(&escape(body.trim(), family)?);
            }
            '}' if inside => {
                *at += 1;
                return Ok(out);
            }
            _ => {
                out.push(character);
                *at += 1;
            }
        }
    }
    match inside {
        true => Err("an ODBC escape sequence has no closing brace".to_owned()),
        false => Ok(out),
    }
}

fn split_keyword(body: &str) -> (String, &str) {
    let end = body.find(|character: char| character.is_whitespace() || character == '(').unwrap_or(body.len());
    (body[..end].to_ascii_lowercase(), body[end..].trim_start())
}

fn escape(body: &str, family: Family) -> Result<String, String> {
    if let Some(call) = body.strip_prefix('?').map(str::trim_start).and_then(|rest| rest.strip_prefix('=')) {
        let (_, procedure) = split_keyword(call.trim_start());
        return Ok(format!("SELECT {}", with_parentheses(procedure)));
    }
    let (keyword, rest) = split_keyword(body);
    let literal = |kind: &str| match family {
        Family::Sqlite => rest.to_owned(),
        _ => format!("{kind} {rest}"),
    };
    Ok(match keyword.as_str() {
        "d" => literal("DATE"),
        "t" => literal("TIME"),
        "ts" => literal("TIMESTAMP"),
        "oj" => rest.to_owned(),
        "escape" => format!("ESCAPE {rest}"),
        "call" => format!("CALL {}", with_parentheses(rest)),
        "fn" => function(rest, family),
        "interval" => format!("INTERVAL {rest}"),
        other => return Err(format!("\"{other}\" is not an ODBC escape sequence")),
    })
}

fn with_parentheses(call: &str) -> String {
    match call.contains('(') {
        true => call.to_owned(),
        false => format!("{call}()"),
    }
}

fn arguments(list: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut current = String::new();
    for character in list.chars() {
        match (quote, character) {
            (Some(open), _) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(character);
                current.push(character);
            }
            (None, '(') => {
                depth += 1;
                current.push(character);
            }
            (None, ')') => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            (None, ',') if depth == 0 => found.push(std::mem::take(&mut current).trim().to_owned()),
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() || !found.is_empty() {
        found.push(current.trim().to_owned());
    }
    found
}

fn sql_type(odbc: &str) -> Option<&'static str> {
    Some(match odbc.trim().to_ascii_uppercase().trim_start_matches("SQL_") {
        "BIGINT" => "BIGINT",
        "INTEGER" => "INTEGER",
        "SMALLINT" | "TINYINT" => "SMALLINT",
        "BIT" => "BOOLEAN",
        "CHAR" | "VARCHAR" | "LONGVARCHAR" | "WCHAR" | "WVARCHAR" | "WLONGVARCHAR" => "VARCHAR",
        "DECIMAL" | "NUMERIC" => "NUMERIC",
        "DOUBLE" | "FLOAT" => "DOUBLE PRECISION",
        "REAL" => "REAL",
        "DATE" | "TYPE_DATE" => "DATE",
        "TIME" | "TYPE_TIME" => "TIME",
        "TIMESTAMP" | "TYPE_TIMESTAMP" => "TIMESTAMP",
        _ => return None,
    })
}

fn date_part(part: &str, sqlite_format: &str, value: &str, family: Family) -> String {
    match family {
        Family::Sqlite => format!("CAST(strftime('{sqlite_format}', {value}) AS INTEGER)"),
        _ => format!("EXTRACT({part} FROM {value})"),
    }
}

fn function(call: &str, family: Family) -> String {
    let Some(open) = call.find('(') else {
        return nullary(call.trim(), family).unwrap_or_else(|| call.to_owned());
    };
    let name = call[..open].trim().to_ascii_uppercase();
    let list = call[open + 1..].trim_end().strip_suffix(')').unwrap_or(&call[open + 1..]);
    let args = arguments(list);
    let one = |index: usize| args.get(index).cloned().unwrap_or_default();
    let sqlite = family == Family::Sqlite;
    let mysql = family == Family::MySql;
    match (name.as_str(), args.len()) {
        ("CONCAT", 2) if mysql => format!("concat({}, {})", one(0), one(1)),
        ("CONCAT", 2) => format!("({} || {})", one(0), one(1)),
        ("LCASE", 1) => format!("lower({})", one(0)),
        ("UCASE", 1) => format!("upper({})", one(0)),
        ("LTRIM", 1) => format!("ltrim({})", one(0)),
        ("RTRIM", 1) => format!("rtrim({})", one(0)),
        ("LENGTH", 1) => format!("length(rtrim({}))", one(0)),
        ("CHAR_LENGTH" | "CHARACTER_LENGTH", 1) => format!("length({})", one(0)),
        ("OCTET_LENGTH", 1) if sqlite => format!("length(CAST({} AS BLOB))", one(0)),
        ("OCTET_LENGTH", 1) => format!("octet_length({})", one(0)),
        ("SUBSTRING", 2 | 3) => format!("substr({})", args.join(", ")),
        ("LEFT", 2) => format!("substr({}, 1, {})", one(0), one(1)),
        ("RIGHT", 2) if sqlite => format!("substr({}, -({}))", one(0), one(1)),
        ("RIGHT", 2) => format!("right({}, {})", one(0), one(1)),
        ("REPLACE", 3) => format!("replace({})", args.join(", ")),
        ("LOCATE", 2) if sqlite => format!("instr({}, {})", one(1), one(0)),
        ("LOCATE", 2) if mysql => format!("locate({}, {})", one(0), one(1)),
        ("LOCATE", 2) => format!("strpos({}, {})", one(1), one(0)),
        ("ASCII", 1) if sqlite => format!("unicode({})", one(0)),
        ("ASCII", 1) => format!("ascii({})", one(0)),
        ("CHAR", 1) if sqlite || mysql => format!("char({})", one(0)),
        ("CHAR", 1) => format!("chr({})", one(0)),
        ("SPACE", 1) if sqlite => format!("printf('%*s', {}, '')", one(0)),
        ("SPACE", 1) => format!("repeat(' ', {})", one(0)),
        ("ABS", 1) => format!("abs({})", one(0)),
        ("CEILING", 1) => format!("ceil({})", one(0)),
        ("FLOOR", 1) => format!("floor({})", one(0)),
        ("EXP", 1) => format!("exp({})", one(0)),
        ("LOG", 1) => format!("ln({})", one(0)),
        ("LOG10", 1) => format!("log10({})", one(0)),
        ("MOD", 2) if sqlite => format!("({} % {})", one(0), one(1)),
        ("MOD", 2) => format!("mod({}, {})", one(0), one(1)),
        ("POWER", 2) => format!("power({}, {})", one(0), one(1)),
        ("ROUND", 1 | 2) => format!("round({})", args.join(", ")),
        ("SIGN", 1) => format!("sign({})", one(0)),
        ("SQRT", 1) => format!("sqrt({})", one(0)),
        ("PI", 0) => "pi()".to_owned(),
        ("YEAR", 1) => date_part("YEAR", "%Y", &one(0), family),
        ("MONTH", 1) => date_part("MONTH", "%m", &one(0), family),
        ("DAYOFMONTH", 1) => date_part("DAY", "%d", &one(0), family),
        ("HOUR", 1) => date_part("HOUR", "%H", &one(0), family),
        ("MINUTE", 1) => date_part("MINUTE", "%M", &one(0), family),
        ("SECOND", 1) => date_part("SECOND", "%S", &one(0), family),
        ("IFNULL", 2) => format!("coalesce({}, {})", one(0), one(1)),
        ("CONVERT", 2) => match sql_type(&one(1)) {
            Some(target) => format!("CAST({} AS {target})", one(0)),
            None => call.to_owned(),
        },
        (_, 0) => nullary(&name, family).unwrap_or_else(|| call.to_owned()),
        _ => call.to_owned(),
    }
}

fn nullary(name: &str, family: Family) -> Option<String> {
    let sqlite = family == Family::Sqlite;
    Some(match name.to_ascii_uppercase().as_str() {
        "NOW" | "CURRENT_TIMESTAMP" if sqlite => "datetime('now')".to_owned(),
        "NOW" | "CURRENT_TIMESTAMP" => "CURRENT_TIMESTAMP".to_owned(),
        "CURDATE" | "CURRENT_DATE" if sqlite => "date('now')".to_owned(),
        "CURDATE" | "CURRENT_DATE" => "CURRENT_DATE".to_owned(),
        "CURTIME" | "CURRENT_TIME" if sqlite => "time('now')".to_owned(),
        "CURTIME" | "CURRENT_TIME" => "CURRENT_TIME".to_owned(),
        "PI" => "pi()".to_owned(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_joins_become_the_connection_s_sql() {
        assert_eq!(
            translated("SELECT * FROM t WHERE d = {d '2024-01-02'} AND s = '{d x}'", Family::Other).unwrap(),
            "SELECT * FROM t WHERE d = DATE '2024-01-02' AND s = '{d x}'"
        );
        assert_eq!(translated("SELECT {ts '2024-01-02 03:04:05'}", Family::Sqlite).unwrap(), "SELECT '2024-01-02 03:04:05'");
        assert_eq!(
            translated("SELECT * FROM {oj a LEFT OUTER JOIN b ON a.id = b.id}", Family::Other).unwrap(),
            "SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id"
        );
        assert_eq!(translated("x LIKE 'a\\_%' {escape '\\'}", Family::Other).unwrap(), "x LIKE 'a\\_%' ESCAPE '\\'");
        assert_eq!(translated("{call refresh}", Family::Other).unwrap(), "CALL refresh()");
        assert_eq!(translated("{?= call total(1)}", Family::Other).unwrap(), "SELECT total(1)");
        assert!(translated("SELECT {fn UCASE(name)", Family::Other).is_err());
        assert!(translated("{zz 1}", Family::Other).is_err());
    }

    #[test]
    fn functions_become_each_family_s_own() {
        assert_eq!(translated("{fn UCASE({fn LTRIM(name)})}", Family::Other).unwrap(), "upper(ltrim(name))");
        assert_eq!(translated("{fn CONCAT(a, 'x,y')}", Family::MySql).unwrap(), "concat(a, 'x,y')");
        assert_eq!(translated("{fn CONCAT(a, b)}", Family::Sqlite).unwrap(), "(a || b)");
        assert_eq!(translated("{fn LOCATE('b', s)}", Family::Sqlite).unwrap(), "instr(s, 'b')");
        assert_eq!(translated("{fn YEAR(d)}", Family::Sqlite).unwrap(), "CAST(strftime('%Y', d) AS INTEGER)");
        assert_eq!(translated("{fn YEAR(d)}", Family::Other).unwrap(), "EXTRACT(YEAR FROM d)");
        assert_eq!(translated("{fn NOW()}", Family::Other).unwrap(), "CURRENT_TIMESTAMP");
        assert_eq!(translated("{fn CONVERT(x, SQL_INTEGER)}", Family::Other).unwrap(), "CAST(x AS INTEGER)");
        assert_eq!(translated("{fn MYSTERY(x)}", Family::Other).unwrap(), "MYSTERY(x)");
        assert_eq!(translated("SELECT 1", Family::Other).unwrap(), "SELECT 1");
    }
}
