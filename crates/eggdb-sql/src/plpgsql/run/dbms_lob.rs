use super::super::frame::RoutineFrame;
use super::{Flow, Run, Target, output_target, parsed};
use crate::ast::Statement;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

const PACKAGE: &str = "dbms_lob";
const OWNER: &str = "sys";
const ORACLE_NO_DATA_FOUND: u16 = 1403;
const ORACLE_INVALID_ARGUMENT: u16 = 21560;

#[derive(Debug, Clone, PartialEq)]
enum Lob {
    Characters(Vec<char>),
    Bytes(Vec<u8>),
}

impl Lob {
    fn of(value: &Value, like: Option<&Lob>) -> Lob {
        match value {
            Value::Bytea(bytes) => Lob::Bytes(bytes.clone()),
            Value::Null => match like {
                Some(Lob::Bytes(_)) => Lob::Bytes(Vec::new()),
                _ => Lob::Characters(Vec::new()),
            },
            other => match like {
                Some(Lob::Bytes(_)) => Lob::Bytes(other.to_text().unwrap_or_default().into_bytes()),
                _ => Lob::Characters(other.to_text().unwrap_or_default().chars().collect()),
            },
        }
    }

    fn len(&self) -> usize {
        match self {
            Lob::Characters(characters) => characters.len(),
            Lob::Bytes(bytes) => bytes.len(),
        }
    }

    fn value(self) -> Value {
        match self {
            Lob::Characters(characters) => Value::Text(characters.into_iter().collect()),
            Lob::Bytes(bytes) => Value::Bytea(bytes),
        }
    }

    fn slice(&self, from: usize, count: usize) -> Lob {
        let end = from.saturating_add(count).min(self.len());
        let from = from.min(end);
        match self {
            Lob::Characters(characters) => Lob::Characters(characters[from..end].to_vec()),
            Lob::Bytes(bytes) => Lob::Bytes(bytes[from..end].to_vec()),
        }
    }

    fn truncate(&mut self, length: usize) {
        match self {
            Lob::Characters(characters) => characters.truncate(length),
            Lob::Bytes(bytes) => bytes.truncate(length),
        }
    }

    fn overwrite(&mut self, at: usize, source: &Lob) {
        match (self, source) {
            (Lob::Characters(characters), Lob::Characters(written)) => {
                if characters.len() < at {
                    characters.resize(at, ' ');
                }
                for (index, character) in written.iter().enumerate() {
                    match characters.get_mut(at + index) {
                        Some(slot) => *slot = *character,
                        None => characters.push(*character),
                    }
                }
            }
            (Lob::Bytes(bytes), Lob::Bytes(written)) => {
                if bytes.len() < at {
                    bytes.resize(at, 0);
                }
                for (index, byte) in written.iter().enumerate() {
                    match bytes.get_mut(at + index) {
                        Some(slot) => *slot = *byte,
                        None => bytes.push(*byte),
                    }
                }
            }
            (Lob::Characters(characters), Lob::Bytes(written)) => {
                let text: Vec<char> = String::from_utf8_lossy(written).chars().collect();
                Lob::Characters(std::mem::take(characters))
                    .overwritten(at, &Lob::Characters(text))
                    .into_characters(characters);
            }
            (Lob::Bytes(bytes), Lob::Characters(written)) => {
                let raw: Vec<u8> = written.iter().collect::<String>().into_bytes();
                let mut whole = Lob::Bytes(std::mem::take(bytes));
                whole.overwrite(at, &Lob::Bytes(raw));
                if let Lob::Bytes(done) = whole {
                    *bytes = done;
                }
            }
        }
    }

    fn overwritten(mut self, at: usize, source: &Lob) -> Lob {
        self.overwrite(at, source);
        self
    }

    fn into_characters(self, into: &mut Vec<char>) {
        if let Lob::Characters(characters) = self {
            *into = characters;
        }
    }

    fn blank(&self, count: usize) -> Lob {
        match self {
            Lob::Characters(_) => Lob::Characters(vec![' '; count]),
            Lob::Bytes(_) => Lob::Bytes(vec![0; count]),
        }
    }
}

struct LobCall {
    member: String,
    arguments: Vec<String>,
}

impl LobCall {
    fn of(text: &str) -> Option<Self> {
        let text = text.trim();
        if !text.get(..4)?.eq_ignore_ascii_case("call") {
            return None;
        }
        let rest = text.get(4..)?;
        let open = rest.find('(')?;
        let close = rest.rfind(')')?;
        let name = rest.get(..open)?.trim().replace('"', "").to_ascii_lowercase();
        let parts: Vec<&str> = name.split('.').map(str::trim).collect();
        let member = match parts.as_slice() {
            [PACKAGE, member] | [OWNER, PACKAGE, member] => (*member).to_owned(),
            _ => return None,
        };
        Some(Self {
            member,
            arguments: split_arguments(rest.get(open + 1..close)?),
        })
    }
}

fn split_arguments(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quoted = false;
    for character in text.chars() {
        match character {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                parts.push(std::mem::take(&mut current).trim().to_owned());
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_owned());
    }
    parts
}

fn wrong_arguments(member: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "PLS-00306: wrong number or types of arguments in call to '{}'",
            member.to_ascii_uppercase()
        ),
    )
}

fn invalid_argument(what: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("ORA-{ORACLE_INVALID_ARGUMENT:05}: invalid argument value for {what}"),
    )
    .with_oracle_code(ORACLE_INVALID_ARGUMENT)
}

fn count_of(value: &Value, what: &str) -> SqlResult<usize> {
    let number = value
        .to_text()
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|number| number.is_finite() && *number >= 0.0)
        .ok_or_else(|| invalid_argument(what))?;
    Ok(number as usize)
}

fn offset_of(value: &Value, what: &str) -> SqlResult<usize> {
    let offset = count_of(value, what)?;
    if offset == 0 {
        return Err(invalid_argument(what));
    }
    Ok(offset - 1)
}

impl Run<'_> {
    fn lob_targets(&self, text: &str, member: &str) -> SqlResult<Vec<Option<Target>>> {
        let Statement::Call(call) =
            parsed(text, &self.settings, &RoutineFrame::default(), self.dialect())?
        else {
            return Err(wrong_arguments(member));
        };
        Ok(call.arguments.iter().map(output_target).collect())
    }

    fn lob_argument(&mut self, arguments: &[String], at: usize, member: &str) -> SqlResult<Value> {
        let text = arguments.get(at).ok_or_else(|| wrong_arguments(member))?;
        self.text_value(text)
    }

    pub(super) fn dbms_lob_call(&mut self, text: &str) -> SqlResult<Option<Flow>> {
        let Some(call) = LobCall::of(text) else {
            return Ok(None);
        };
        let member = call.member.as_str();
        if matches!(member, "open" | "close" | "fileclose" | "fileopen") {
            return Ok(Some(Flow::Next));
        }
        if !matches!(
            member,
            "createtemporary" | "freetemporary" | "append" | "writeappend" | "write" | "trim"
                | "erase" | "read" | "copy"
        ) {
            return Ok(None);
        }
        let targets = self.lob_targets(text, member)?;
        let target = |at: usize| -> SqlResult<Target> {
            targets
                .get(at)
                .cloned()
                .flatten()
                .ok_or_else(|| wrong_arguments(member))
        };
        let arguments = call.arguments;
        let lob_value = self.lob_argument(&arguments, 0, member)?;
        let mut lob = Lob::of(&lob_value, None);
        match member {
            "createtemporary" => {
                self.assign(&target(0)?, Lob::of(&Value::Null, Some(&lob)).value())?;
            }
            "freetemporary" => self.assign(&target(0)?, Value::Null)?,
            "append" => {
                let source = self.lob_argument(&arguments, 1, member)?;
                let at = lob.len();
                lob.overwrite(at, &Lob::of(&source, Some(&lob)));
                self.assign(&target(0)?, lob.value())?;
            }
            "writeappend" => {
                let amount = count_of(&self.lob_argument(&arguments, 1, member)?, "amount")?;
                let buffer = self.lob_argument(&arguments, 2, member)?;
                let written = Lob::of(&buffer, Some(&lob)).slice(0, amount);
                let at = lob.len();
                lob.overwrite(at, &written);
                self.assign(&target(0)?, lob.value())?;
            }
            "write" => {
                let amount = count_of(&self.lob_argument(&arguments, 1, member)?, "amount")?;
                let offset = offset_of(&self.lob_argument(&arguments, 2, member)?, "offset")?;
                let buffer = self.lob_argument(&arguments, 3, member)?;
                let written = Lob::of(&buffer, Some(&lob)).slice(0, amount);
                lob.overwrite(offset, &written);
                self.assign(&target(0)?, lob.value())?;
            }
            "trim" => {
                let length = count_of(&self.lob_argument(&arguments, 1, member)?, "newlen")?;
                if length > lob.len() {
                    return Err(invalid_argument("newlen"));
                }
                lob.truncate(length);
                self.assign(&target(0)?, lob.value())?;
            }
            "erase" => {
                let amount = count_of(&self.lob_argument(&arguments, 1, member)?, "amount")?;
                let offset = match arguments.get(2) {
                    Some(_) => offset_of(&self.lob_argument(&arguments, 2, member)?, "offset")?,
                    None => 0,
                };
                let erased = amount.min(lob.len().saturating_sub(offset));
                let blank = lob.blank(erased);
                lob.overwrite(offset, &blank);
                self.assign(&target(0)?, lob.value())?;
                if let Ok(amount_target) = target(1) {
                    self.assign(&amount_target, Value::BigInt(erased as i64))?;
                }
            }
            "read" => {
                let amount = count_of(&self.lob_argument(&arguments, 1, member)?, "amount")?;
                let offset = offset_of(&self.lob_argument(&arguments, 2, member)?, "offset")?;
                if offset >= lob.len() {
                    return Err(SqlError::new(
                        SqlState::NO_DATA_FOUND,
                        format!("ORA-{ORACLE_NO_DATA_FOUND:05}: no data found"),
                    )
                    .with_oracle_code(ORACLE_NO_DATA_FOUND));
                }
                let read = lob.slice(offset, amount);
                let count = read.len();
                self.assign(&target(3)?, read.value())?;
                if let Ok(amount_target) = target(1) {
                    self.assign(&amount_target, Value::BigInt(count as i64))?;
                }
            }
            _ => {
                let source = self.lob_argument(&arguments, 1, member)?;
                let amount = count_of(&self.lob_argument(&arguments, 2, member)?, "amount")?;
                let destination_offset = match arguments.get(3) {
                    Some(_) => offset_of(&self.lob_argument(&arguments, 3, member)?, "dest_offset")?,
                    None => 0,
                };
                let source_offset = match arguments.get(4) {
                    Some(_) => offset_of(&self.lob_argument(&arguments, 4, member)?, "src_offset")?,
                    None => 0,
                };
                let copied = Lob::of(&source, Some(&lob)).slice(source_offset, amount);
                lob.overwrite(destination_offset, &copied);
                self.assign(&target(0)?, lob.value())?;
            }
        }
        Ok(Some(Flow::Next))
    }
}
