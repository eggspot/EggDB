use eggdb_sql::Value;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};

const DESCRIBE_SHORTEST: usize = "desc".len();
const PROMPT_SHORTEST: usize = "pro".len();
const SPOOL_SHORTEST: usize = "spo".len();
const NAME_WIDTH: usize = 41;
const NULL_WIDTH: usize = 8;
const TYPE_WIDTH: usize = 28;
const DEFAULT_SPOOL_EXTENSION: &str = ".lst";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SqlPlusCommand {
    Describe,
    Prompt,
    Spool,
}

impl SqlPlusCommand {
    pub(super) fn named(word: &str) -> Option<Self> {
        let abbreviates =
            |command: &str, shortest: usize| word.len() >= shortest && command.starts_with(word);
        if abbreviates("describe", DESCRIBE_SHORTEST) {
            Some(Self::Describe)
        } else if abbreviates("prompt", PROMPT_SHORTEST) {
            Some(Self::Prompt)
        } else if abbreviates("spool", SPOOL_SHORTEST) {
            Some(Self::Spool)
        } else {
            None
        }
    }
}

pub(super) fn describe_query(name: &str) -> Option<String> {
    let parts = dictionary_names(name)?;
    let literal = |text: &str| format!("'{}'", text.replace('\'', "''"));
    let columns = "SELECT column_name, nullable, data_type, data_length, data_precision, \
        data_scale, char_length";
    match parts.as_slice() {
        [table] => Some(format!(
            "{columns} FROM user_tab_columns WHERE table_name = {} ORDER BY column_id",
            literal(table)
        )),
        [owner, table] => Some(format!(
            "{columns} FROM all_tab_columns WHERE owner = {} AND table_name = {} ORDER BY column_id",
            literal(owner),
            literal(table)
        )),
        _ => None,
    }
}

fn dictionary_names(name: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = name.trim();
    if rest.is_empty() || rest.contains('@') {
        return None;
    }
    loop {
        let (part, after) = if let Some(quoted) = rest.strip_prefix('"') {
            let mut text = String::new();
            let mut chars = quoted.char_indices();
            let mut end = None;
            while let Some((at, character)) = chars.next() {
                if character != '"' {
                    text.push(character);
                    continue;
                }
                if quoted.get(at + 1..).is_some_and(|after| after.starts_with('"')) {
                    text.push('"');
                    chars.next();
                    continue;
                }
                end = Some(at + 1);
                break;
            }
            let end = end?;
            (text, quoted.get(end..).unwrap_or_default())
        } else {
            let end = rest.find('.').unwrap_or(rest.len());
            let word = rest.get(..end).unwrap_or_default();
            if word.is_empty() || word.contains(|character: char| character.is_whitespace() || character == '"') {
                return None;
            }
            (word.to_uppercase(), rest.get(end..).unwrap_or_default())
        };
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        if after.is_empty() {
            return Some(parts);
        }
        rest = after.strip_prefix('.')?;
    }
}

pub(super) fn describe_text(rows: &[Vec<Value>]) -> String {
    let described: Vec<(String, bool, String)> = rows
        .iter()
        .map(|row| {
            let field = |at: usize| row.get(at).unwrap_or(&Value::Null);
            let name = text_of(field(0)).unwrap_or_default();
            let not_null = text_of(field(1)).as_deref() == Some("N");
            let data_type = text_of(field(2)).unwrap_or_default();
            let sql_type = type_text(
                &data_type,
                whole(field(3)),
                whole(field(4)),
                whole(field(5)),
                whole(field(6)),
            );
            (name, not_null, sql_type)
        })
        .collect();
    let name_width = described
        .iter()
        .map(|(name, _, _)| name.chars().count())
        .max()
        .unwrap_or(0)
        .max(NAME_WIDTH);
    let type_width = described
        .iter()
        .map(|(_, _, sql_type)| sql_type.chars().count())
        .max()
        .unwrap_or(0)
        .max(TYPE_WIDTH);
    let mut text = format!(" {:<name_width$} {:<NULL_WIDTH$} Type\n", "Name", "Null?");
    text.push_str(&format!(
        " {} {} {}\n",
        "-".repeat(name_width),
        "-".repeat(NULL_WIDTH),
        "-".repeat(type_width)
    ));
    for (name, not_null, sql_type) in described {
        let null = if not_null { "NOT NULL" } else { "" };
        text.push_str(&format!(" {name:<name_width$} {null:<NULL_WIDTH$} {sql_type}\n"));
    }
    text
}

fn type_text(
    data_type: &str,
    length: Option<i64>,
    precision: Option<i64>,
    scale: Option<i64>,
    characters: Option<i64>,
) -> String {
    match data_type {
        "NUMBER" => match (precision, scale) {
            (Some(precision), Some(0) | None) => format!("NUMBER({precision})"),
            (Some(precision), Some(scale)) => format!("NUMBER({precision},{scale})"),
            (None, Some(0)) => "NUMBER(38)".to_owned(),
            (None, _) => "NUMBER".to_owned(),
        },
        "FLOAT" => precision.map_or_else(|| "FLOAT".to_owned(), |precision| format!("FLOAT({precision})")),
        "VARCHAR2" | "CHAR" => match characters.filter(|characters| *characters > 0) {
            Some(characters) => format!("{data_type}({characters} CHAR)"),
            None => data_type.to_owned(),
        },
        "NVARCHAR2" | "NCHAR" => match characters.filter(|characters| *characters > 0) {
            Some(characters) => format!("{data_type}({characters})"),
            None => data_type.to_owned(),
        },
        "RAW" => length.map_or_else(|| "RAW".to_owned(), |length| format!("RAW({length})")),
        other => other.to_owned(),
    }
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) => Some(text.clone()),
        other => whole(other).map(|number| number.to_string()),
    }
}

fn whole(value: &Value) -> Option<i64> {
    match value {
        Value::SmallInt(number) => Some(i64::from(*number)),
        Value::Integer(number) => Some(i64::from(*number)),
        Value::BigInt(number) => Some(*number),
        Value::Numeric(number) => number.to_string().parse().ok(),
        Value::Text(text) => text.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpoolMode {
    Create,
    Replace,
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SpoolCommand {
    Status,
    Off,
    To { path: String, mode: SpoolMode },
}

pub(super) fn parse_spool(arguments: &str) -> Result<SpoolCommand, String> {
    let words: Vec<&str> = arguments.split_whitespace().collect();
    let Some((target, options)) = words.split_first() else {
        return Ok(SpoolCommand::Status);
    };
    if options.is_empty() && target.eq_ignore_ascii_case("off") {
        return Ok(SpoolCommand::Off);
    }
    if options.is_empty() && target.eq_ignore_ascii_case("out") {
        return Err("SP2-0333: SPOOL OUT sends output to a printer, which eggdb shell has none of; use SPOOL OFF".to_owned());
    }
    let mode = match options {
        [] => SpoolMode::Replace,
        [option] => {
            let option = option.to_ascii_lowercase();
            let names = |command: &str| option.len() >= 3 && command.starts_with(option.as_str());
            if names("create") {
                SpoolMode::Create
            } else if names("replace") {
                SpoolMode::Replace
            } else if names("append") {
                SpoolMode::Append
            } else {
                return Err(format!("SP2-0768: Illegal SPOOL command: {option}"));
            }
        }
        _ => return Err("SP2-0768: Illegal SPOOL command".to_owned()),
    };
    let unquoted = target
        .strip_prefix('"')
        .and_then(|target| target.strip_suffix('"'))
        .unwrap_or(target);
    if unquoted.is_empty() {
        return Err("SP2-0768: Illegal SPOOL command".to_owned());
    }
    let file_name = unquoted.rsplit(['/', '\\']).next().unwrap_or(unquoted);
    let path = if file_name.contains('.') {
        unquoted.to_owned()
    } else {
        format!("{unquoted}{DEFAULT_SPOOL_EXTENSION}")
    };
    Ok(SpoolCommand::To { path, mode })
}

pub(super) struct Spool {
    pub(super) path: String,
    pub(super) writer: BufWriter<File>,
}

impl Spool {
    pub(super) fn open(path: &str, mode: SpoolMode) -> Result<Self, String> {
        let mut options = OpenOptions::new();
        match mode {
            SpoolMode::Create => options.write(true).create_new(true),
            SpoolMode::Replace => options.write(true).create(true).truncate(true),
            SpoolMode::Append => options.append(true).create(true),
        };
        match options.open(path) {
            Ok(file) => Ok(Self {
                path: path.to_owned(),
                writer: BufWriter::new(file),
            }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(format!(
                "SP2-0771: File \"{path}\" already exists. Use another name or \"SPOOL filename[.ext] REPLACE\""
            )),
            Err(error) => Err(format!("SP2-0606: Cannot create SPOOL file \"{path}\": {error}")),
        }
    }
}

pub(super) struct Tee<'a> {
    pub(super) first: &'a mut dyn Write,
    pub(super) second: &'a mut dyn Write,
}

impl Write for Tee<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.first.write_all(bytes)?;
        self.second.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.first.flush()?;
        self.second.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_recognised_by_their_sqlplus_abbreviations() {
        assert_eq!(SqlPlusCommand::named("desc"), Some(SqlPlusCommand::Describe));
        assert_eq!(SqlPlusCommand::named("describe"), Some(SqlPlusCommand::Describe));
        assert_eq!(SqlPlusCommand::named("des"), None);
        assert_eq!(SqlPlusCommand::named("pro"), Some(SqlPlusCommand::Prompt));
        assert_eq!(SqlPlusCommand::named("spool"), Some(SqlPlusCommand::Spool));
        assert_eq!(SqlPlusCommand::named("select"), None);
    }

    #[test]
    fn describe_names_are_folded_to_upper_case_unless_quoted() {
        let query = describe_query("emp").unwrap_or_default();
        assert!(query.contains("user_tab_columns WHERE table_name = 'EMP'"), "{query}");
        let query = describe_query("scott.\"Emp\"\"s\"").unwrap_or_default();
        assert!(
            query.contains("owner = 'SCOTT' AND table_name = 'Emp\"s'"),
            "{query}"
        );
        assert_eq!(describe_query("a.b.c"), None);
        assert_eq!(describe_query("emp@remote"), None);
        assert_eq!(describe_query("\"open"), None);
        assert_eq!(describe_query(""), None);
    }

    #[test]
    fn describe_lays_out_names_nullability_and_types_like_sqlplus() {
        let row = |name: &str, nullable: &str, data_type: &str, precision: Option<i64>, scale: Option<i64>, characters: i64| {
            vec![
                Value::Text(name.to_owned()),
                Value::Text(nullable.to_owned()),
                Value::Text(data_type.to_owned()),
                Value::BigInt(22),
                precision.map_or(Value::Null, Value::BigInt),
                scale.map_or(Value::Null, Value::BigInt),
                Value::BigInt(characters),
            ]
        };
        let text = describe_text(&[
            row("EMPNO", "N", "NUMBER", Some(4), Some(0), 0),
            row("SAL", "Y", "NUMBER", Some(7), Some(2), 0),
            row("ENAME", "Y", "VARCHAR2", None, None, 10),
            row("HIREDATE", "Y", "DATE", None, None, 0),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6);
        assert!(lines[0].starts_with(" Name"), "{text}");
        assert!(lines[0].ends_with("Null?    Type"), "{text}");
        assert!(lines[2].starts_with(" EMPNO ") && lines[2].ends_with("NOT NULL NUMBER(4)"), "{text}");
        assert!(lines[3].ends_with(" NUMBER(7,2)"), "{text}");
        assert!(lines[4].ends_with(" VARCHAR2(10 CHAR)"), "{text}");
        assert!(lines[5].ends_with(" DATE"), "{text}");
    }

    #[test]
    fn spool_takes_a_file_a_mode_or_off() {
        assert_eq!(parse_spool(""), Ok(SpoolCommand::Status));
        assert_eq!(parse_spool("OFF"), Ok(SpoolCommand::Off));
        assert_eq!(
            parse_spool("report"),
            Ok(SpoolCommand::To {
                path: "report.lst".to_owned(),
                mode: SpoolMode::Replace
            })
        );
        assert_eq!(
            parse_spool("out/report.txt app"),
            Ok(SpoolCommand::To {
                path: "out/report.txt".to_owned(),
                mode: SpoolMode::Append
            })
        );
        assert_eq!(
            parse_spool("report cre"),
            Ok(SpoolCommand::To {
                path: "report.lst".to_owned(),
                mode: SpoolMode::Create
            })
        );
        assert!(parse_spool("report sideways").is_err());
        assert!(parse_spool("out").is_err());
    }
}
