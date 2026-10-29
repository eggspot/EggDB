use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

pub(crate) const MAX_RAISED_MESSAGE_CHARACTERS: usize = 2047;
const SHORTENED_MESSAGE_CHARACTERS: usize = 2044;
const USER_MESSAGE_NUMBER: u32 = 50_000;
const FIRST_MESSAGE_NUMBER: i64 = 13_000;
const HIGHEST_USER_SEVERITY: i64 = 18;
const HIGHEST_SEVERITY: i64 = 25;
const HIGHEST_STATE: i64 = 255;
const NULL_ARGUMENT: &str = "(null)";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CaughtError {
    pub(crate) number: u32,
    pub(crate) severity: u8,
    pub(crate) state: u8,
    pub(crate) message: String,
    pub(crate) line: u32,
}

impl CaughtError {
    pub(crate) fn of(error: &SqlError, line: u32) -> Self {
        Self {
            number: error.sqlserver_number().unwrap_or(USER_MESSAGE_NUMBER),
            severity: error.severity().unwrap_or(16),
            state: error.sqlserver_state().unwrap_or(1),
            message: error.message().to_owned(),
            line,
        }
    }

    pub(crate) fn rethrown(&self) -> SqlError {
        SqlError::raised_in_sqlserver(
            self.number,
            self.severity,
            self.state,
            self.message.clone(),
            true,
        )
    }
}

pub(crate) fn line_of(text: &str, offset: usize) -> u32 {
    let before = text.as_bytes().get(..offset.min(text.len())).unwrap_or_default();
    let breaks = before.iter().filter(|&&byte| byte == b'\n').count();
    u32::try_from(breaks + 1).unwrap_or(u32::MAX)
}

pub(crate) fn nesting_too_deep() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32).",
    )
    .with_sqlserver_code(217)
}

fn integer_argument(value: &Value, what: &str) -> SqlResult<Option<i64>> {
    if value.is_null() {
        return Ok(None);
    }
    let converted = crate::sqlserver_conversion::implicit(value.clone(), SqlType::BigInt)
        .map_err(|_| {
            SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("The {what} must be an integer."),
            )
            .with_sqlserver_code(206)
        })?;
    Ok(converted.as_integer())
}

fn text_argument(value: &Value) -> SqlResult<Option<String>> {
    if value.is_null() {
        return Ok(None);
    }
    crate::sqlserver_conversion::text_of(value, None, None).map(Some)
}

fn shortened(message: String) -> String {
    if message.chars().count() <= MAX_RAISED_MESSAGE_CHARACTERS {
        return message;
    }
    let mut kept: String = message.chars().take(SHORTENED_MESSAGE_CHARACTERS).collect();
    kept.push_str("...");
    kept
}

pub(crate) struct RaisedMessage {
    pub(crate) number: u32,
    pub(crate) severity: u8,
    pub(crate) state: u8,
    pub(crate) text: String,
}

pub(crate) fn raised_message(
    message: &Value,
    severity: &Value,
    state: &Value,
    arguments: &[Value],
) -> SqlResult<RaisedMessage> {
    let severity = integer_argument(severity, "severity of RAISERROR")?
        .unwrap_or(0)
        .clamp(0, HIGHEST_SEVERITY);
    let state = match integer_argument(state, "state of RAISERROR")?.unwrap_or(1) {
        negative if negative < 0 => 1,
        state if state > HIGHEST_STATE => {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("The state {state} given to RAISERROR is outside the range 0 through 255."),
            ));
        }
        state => state,
    };
    if severity > HIGHEST_USER_SEVERITY {
        return Err(SqlError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            "Error severity levels greater than 18 can only be specified by members of the sysadmin role, using the WITH LOG option.",
        )
        .with_sqlserver_code(2754));
    }
    let (severity, state) = (severity as u8, state as u8);
    if let Some(number) = message.as_integer() {
        if number < FIRST_MESSAGE_NUMBER || number == i64::from(USER_MESSAGE_NUMBER) {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "Error number {number} is invalid. The number must be from 13000 through 2147483647 and it cannot be 50000."
                ),
            )
            .with_sqlserver_code(2732));
        }
        return Err(SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!(
                "Error {number}, severity {severity}, state {state} was raised, but no message with that error number was found in sys.messages. If error is larger than 50000, make sure the user-defined message is added using sp_addmessage."
            ),
        )
        .with_sqlserver_code(18054));
    }
    let template = text_argument(message)?.unwrap_or_default();
    Ok(RaisedMessage {
        number: USER_MESSAGE_NUMBER,
        severity,
        state,
        text: shortened(formatted_message(&template, arguments)?),
    })
}

pub(crate) fn thrown(number: &Value, message: &Value, state: &Value) -> SqlResult<SqlError> {
    let number = integer_argument(number, "error number of THROW")?.unwrap_or(0);
    if !(i64::from(USER_MESSAGE_NUMBER)..=i64::from(i32::MAX)).contains(&number) {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "Error number {number} in the THROW statement is outside the valid range. Specify an error number in the valid range of 50000 to 2147483647."
            ),
        )
        .with_sqlserver_code(35100));
    }
    let state = integer_argument(state, "state of THROW")?.unwrap_or(0);
    if !(0..=HIGHEST_STATE).contains(&state) {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "State {state} in the THROW statement is outside the valid range. Specify a state in the valid range of 0 to 255."
            ),
        ));
    }
    let message = shortened(text_argument(message)?.unwrap_or_default());
    Ok(SqlError::raised_in_sqlserver(
        number as u32,
        16,
        state as u8,
        message,
        true,
    ))
}

#[derive(Default)]
struct Specification {
    left: bool,
    plus: bool,
    zero: bool,
    blank: bool,
    alternate: bool,
    width: Option<usize>,
    precision: Option<usize>,
}

fn mismatched(argument: usize) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "The data type of substitution parameter {argument} does not match the expected type of the format specification."
        ),
    )
    .with_sqlserver_code(2786)
}

fn padded(body: String, sign: &str, specification: &Specification, numeric: bool) -> String {
    let length = body.chars().count() + sign.chars().count();
    let width = specification.width.unwrap_or(0);
    if length >= width {
        return format!("{sign}{body}");
    }
    let fill = width - length;
    if specification.left {
        return format!("{sign}{body}{}", " ".repeat(fill));
    }
    if specification.zero && numeric && specification.precision.is_none() {
        return format!("{sign}{}{body}", "0".repeat(fill));
    }
    format!("{}{sign}{body}", " ".repeat(fill))
}

fn digits_with_precision(digits: String, precision: Option<usize>) -> String {
    match precision {
        Some(wanted) if digits.len() < wanted => {
            format!("{}{digits}", "0".repeat(wanted - digits.len()))
        }
        _ => digits,
    }
}

fn integer_of(value: Option<&Value>, argument: usize) -> SqlResult<Option<i64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_integer().map(Some).ok_or_else(|| mismatched(argument)),
    }
}

fn rendered(
    kind: char,
    value: Option<&Value>,
    argument: usize,
    specification: &Specification,
) -> SqlResult<Option<String>> {
    Ok(Some(match kind {
        'd' | 'i' => {
            let Some(number) = integer_of(value, argument)? else {
                return Ok(Some(padded(NULL_ARGUMENT.to_owned(), "", specification, false)));
            };
            let sign = if number < 0 {
                "-"
            } else if specification.plus {
                "+"
            } else if specification.blank {
                " "
            } else {
                ""
            };
            let digits = digits_with_precision(number.unsigned_abs().to_string(), specification.precision);
            padded(digits, sign, specification, true)
        }
        'u' | 'o' | 'x' | 'X' => {
            let Some(number) = integer_of(value, argument)? else {
                return Ok(Some(padded(NULL_ARGUMENT.to_owned(), "", specification, false)));
            };
            let unsigned = if (i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&number) {
                u64::from(number as i32 as u32)
            } else {
                number as u64
            };
            let (digits, prefix) = match kind {
                'u' => (unsigned.to_string(), ""),
                'o' => (format!("{unsigned:o}"), "0"),
                'x' => (format!("{unsigned:x}"), "0x"),
                _ => (format!("{unsigned:X}"), "0X"),
            };
            let digits = digits_with_precision(digits, specification.precision);
            let prefix = if specification.alternate && unsigned != 0 {
                prefix
            } else {
                ""
            };
            padded(digits, prefix, specification, true)
        }
        's' => {
            let text = match value {
                None | Some(Value::Null) => NULL_ARGUMENT.to_owned(),
                Some(Value::Text(text) | Value::Bpchar(text)) => text.clone(),
                Some(_) => return Err(mismatched(argument)),
            };
            let text = match specification.precision {
                Some(most) => text.chars().take(most).collect(),
                None => text,
            };
            padded(text, "", specification, false)
        }
        _ => return Ok(None),
    }))
}

fn number_in(characters: &[char], at: &mut usize) -> Option<usize> {
    let start = *at;
    while characters.get(*at).is_some_and(char::is_ascii_digit) {
        *at += 1;
    }
    if *at == start {
        return None;
    }
    characters
        .get(start..*at)
        .map(|digits| digits.iter().collect::<String>())
        .and_then(|digits| digits.parse().ok())
}

fn take_argument<'a>(arguments: &'a [Value], next: &mut usize) -> (Option<&'a Value>, usize) {
    let value = arguments.get(*next);
    *next += 1;
    (value, *next)
}

pub(crate) fn formatted_message(template: &str, arguments: &[Value]) -> SqlResult<String> {
    let characters: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut next_argument = 0usize;
    let mut at = 0usize;
    while let Some(&character) = characters.get(at) {
        at += 1;
        if character != '%' {
            out.push(character);
            continue;
        }
        if characters.get(at) == Some(&'%') {
            at += 1;
            out.push('%');
            continue;
        }
        let start = at - 1;
        let mut specification = Specification::default();
        while let Some(&flag) = characters.get(at) {
            match flag {
                '-' => specification.left = true,
                '+' => specification.plus = true,
                '0' => specification.zero = true,
                ' ' => specification.blank = true,
                '#' => specification.alternate = true,
                _ => break,
            }
            at += 1;
        }
        if characters.get(at) == Some(&'*') {
            at += 1;
            let (value, argument) = take_argument(arguments, &mut next_argument);
            specification.width = integer_of(value, argument)?.map(|width| width.max(0) as usize);
        } else {
            specification.width = number_in(&characters, &mut at);
        }
        if characters.get(at) == Some(&'.') {
            at += 1;
            if characters.get(at) == Some(&'*') {
                at += 1;
                let (value, argument) = take_argument(arguments, &mut next_argument);
                specification.precision =
                    Some(integer_of(value, argument)?.map_or(0, |precision| precision.max(0) as usize));
            } else {
                specification.precision = Some(number_in(&characters, &mut at).unwrap_or(0));
            }
        }
        if matches!(characters.get(at), Some('h' | 'l')) {
            at += 1;
        }
        let Some(&kind) = characters.get(at) else {
            out.extend(characters.get(start..).unwrap_or_default());
            break;
        };
        at += 1;
        if !matches!(kind, 'd' | 'i' | 'o' | 's' | 'u' | 'x' | 'X') {
            out.extend(characters.get(start..at).unwrap_or_default());
            continue;
        }
        let (value, argument) = take_argument(arguments, &mut next_argument);
        match rendered(kind, value, argument, &specification)? {
            Some(text) => out.push_str(&text),
            None => out.extend(characters.get(start..at).unwrap_or_default()),
        }
    }
    Ok(out)
}
