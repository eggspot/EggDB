use super::script::{ClientCommand, ScriptFailure, ScriptItem, ScriptSource, ScriptStatement};
use super::script_text::DecodedLines;
use super::{IncludeBase, IncludeOpener, IncludeRequest};
use crate::error::{SqlError, SqlResult, SqlState};
use std::collections::HashMap;
use std::io::BufRead;

pub(crate) const MAX_INCLUDE_DEPTH: usize = 20;
pub(crate) const MAX_INCLUDES_OPENED: usize = 10_000;
pub(crate) const MAX_SUBSTITUTION_VARIABLES: usize = 1024;
pub(crate) const MAX_VARIABLE_VALUE_BYTES: usize = 240;
const MAX_STATEMENT_BYTES: usize = 1 << 30;
const UNTERMINATED: &str = "unterminated";

const SQL_WORDS: [&str; 29] = [
    "select",
    "with",
    "insert",
    "update",
    "delete",
    "merge",
    "create",
    "alter",
    "drop",
    "truncate",
    "rename",
    "grant",
    "revoke",
    "comment",
    "commit",
    "rollback",
    "savepoint",
    "lock",
    "analyze",
    "audit",
    "noaudit",
    "call",
    "explain",
    "flashback",
    "purge",
    "associate",
    "disassociate",
    "declare",
    "begin",
];

const COMMANDS: [(&str, usize); 51] = [
    ("remark", 3),
    ("prompt", 3),
    ("define", 3),
    ("undefine", 5),
    ("accept", 3),
    ("column", 3),
    ("spool", 3),
    ("connect", 4),
    ("start", 3),
    ("execute", 4),
    ("whenever", 8),
    ("exit", 4),
    ("quit", 4),
    ("set", 3),
    ("show", 3),
    ("break", 3),
    ("compute", 4),
    ("ttitle", 3),
    ("btitle", 3),
    ("clear", 2),
    ("pause", 3),
    ("timing", 4),
    ("variable", 3),
    ("print", 3),
    ("describe", 4),
    ("password", 5),
    ("disconnect", 4),
    ("host", 2),
    ("get", 3),
    ("save", 3),
    ("store", 5),
    ("edit", 2),
    ("run", 1),
    ("copy", 4),
    ("xquery", 6),
    ("startup", 7),
    ("shutdown", 8),
    ("recover", 7),
    ("archive", 7),
    ("load", 4),
    ("unload", 6),
    ("ddl", 3),
    ("script", 6),
    ("cd", 2),
    ("alias", 5),
    ("info", 4),
    ("soda", 4),
    ("liquibase", 9),
    ("repeat", 6),
    ("history", 7),
    ("format", 6),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatementMode {
    Undecided,
    Sql,
    PlSql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lex {
    Normal,
    String,
    QQuote(u8),
    Quoted,
    Comment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Substituted {
    pub(crate) text: String,
    pub(crate) unresolved: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubstitutionVariables {
    values: HashMap<String, String>,
    define_character: Option<char>,
    escape: Option<char>,
    concat: Option<char>,
}

impl Default for SubstitutionVariables {
    fn default() -> Self {
        let mut values = HashMap::new();
        for name in [
            "_DATE",
            "_USER",
            "_CONNECT_IDENTIFIER",
            "_O_VERSION",
            "_O_RELEASE",
            "_SQLPLUS_RELEASE",
        ] {
            values.insert(name.to_owned(), String::new());
        }
        Self {
            values,
            define_character: Some('&'),
            escape: None,
            concat: Some('.'),
        }
    }
}

fn is_variable_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '$' | '#')
}

impl SubstitutionVariables {
    pub(crate) fn define(&mut self, name: &str, value: &str) -> SqlResult<()> {
        let key = name.to_ascii_uppercase();
        if value.len() > MAX_VARIABLE_VALUE_BYTES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "the value of substitution variable {name} is longer than {MAX_VARIABLE_VALUE_BYTES} bytes, SQL*Plus's DEFINE limit"
                ),
            ));
        }
        if !self.values.contains_key(&key) && self.values.len() >= MAX_SUBSTITUTION_VARIABLES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("the script defines more than {MAX_SUBSTITUTION_VARIABLES} substitution variables"),
            ));
        }
        self.values.insert(key, value.to_owned());
        Ok(())
    }

    pub(crate) fn undefine(&mut self, name: &str) {
        self.values.remove(&name.to_ascii_uppercase());
    }

    pub(crate) fn is_defined(&self, name: &str) -> bool {
        self.values.contains_key(&name.to_ascii_uppercase())
    }

    fn set_define(&mut self, define: Option<char>) {
        self.define_character = define;
    }

    pub(crate) fn substitute(&mut self, text: &str) -> SqlResult<Substituted> {
        let Some(define) = self.define_character else {
            return Ok(Substituted {
                text: text.to_owned(),
                unresolved: None,
            });
        };
        if !text.contains(define) {
            return Ok(Substituted {
                text: text.to_owned(),
                unresolved: None,
            });
        }
        let mut out = String::with_capacity(text.len());
        let mut unresolved = None;
        let mut rest = text;
        while let Some(character) = rest.chars().next() {
            if Some(character) == self.escape && rest[character.len_utf8()..].starts_with(define) {
                out.push(define);
                rest = &rest[character.len_utf8() + define.len_utf8()..];
                continue;
            }
            if character != define {
                out.push(character);
                rest = &rest[character.len_utf8()..];
                continue;
            }
            let mut after = &rest[define.len_utf8()..];
            let twice = after.starts_with(define);
            if twice {
                after = &after[define.len_utf8()..];
            }
            let length = after
                .find(|character: char| !is_variable_character(character))
                .unwrap_or(after.len());
            if length == 0 {
                let taken = rest.len() - after.len();
                out.push_str(&rest[..taken]);
                rest = after;
                continue;
            }
            let name = &after[..length];
            let mut next = &after[length..];
            match self.values.get(&name.to_ascii_uppercase()) {
                Some(value) => {
                    if out.len() + value.len() > MAX_STATEMENT_BYTES {
                        return Err(SqlError::new(
                            SqlState::PROGRAM_LIMIT_EXCEEDED,
                            "a statement grows past 1 GiB when its substitution variables are replaced",
                        ));
                    }
                    out.push_str(value);
                    if let Some(concat) = self.concat
                        && next.starts_with(concat)
                    {
                        next = &next[concat.len_utf8()..];
                    }
                }
                None => {
                    if unresolved.is_none() {
                        unresolved = Some(name.to_owned());
                    }
                    out.push_str(&rest[..rest.len() - next.len()]);
                }
            }
            rest = next;
        }
        Ok(Substituted {
            text: out,
            unresolved,
        })
    }
}

struct IncludedScript<'r> {
    lines: DecodedLines<'r>,
    script: u32,
}

struct Pending {
    text: String,
    line: u64,
    script: u32,
    mode: StatementMode,
    ended_by_semicolon: bool,
}

pub(crate) struct SqlPlusReader<'r> {
    scripts: Vec<IncludedScript<'r>>,
    opener: Option<&'r dyn IncludeOpener>,
    names: Vec<String>,
    variables: SubstitutionVariables,
    lex: Lex,
    lex_line: u64,
    pending: Option<Pending>,
    comment_between: bool,
    last_ended_by_semicolon: bool,
    queued: Vec<ScriptItem>,
    opened: usize,
    refusing_includes: bool,
    ended: bool,
    line: String,
}

pub(crate) fn unresolved_error(name: &str, line: u64) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_PARAMETER,
        format!(
            "substitution variable &{name} has no value: add DEFINE {name} = ... before line {line}, or SET DEFINE OFF"
        ),
    )
    .with_hint("eggdb restore never prompts; --define name=value gives a value from the command line.")
}

fn first_word(text: &str) -> String {
    let trimmed = text.trim_start();
    let length = trimmed
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(trimmed.len());
    trimmed[..length].to_ascii_lowercase()
}

fn words_of(text: &str, count: usize) -> Vec<String> {
    let mut words = Vec::new();
    for word in super::words::Words::oracle(text) {
        match word {
            super::words::Token::Word(word) => words.push(word),
            super::words::Token::Quoted(name) => words.push(name),
            super::words::Token::Symbol(symbol) => words.push((symbol as char).to_string()),
            super::words::Token::Literal => words.push("'".to_owned()),
        }
        if words.len() >= count {
            break;
        }
    }
    words
}

pub(crate) fn mode_of(text: &str) -> StatementMode {
    let trimmed = text.trim_start();
    if trimmed.starts_with("<<") {
        return StatementMode::PlSql;
    }
    let words = words_of(text, 12);
    let Some(first) = words.first() else {
        return StatementMode::Undecided;
    };
    match first.as_str() {
        "declare" | "begin" => StatementMode::PlSql,
        "with" => match words.get(1).map(String::as_str) {
            Some("function" | "procedure") => StatementMode::PlSql,
            Some(_) => StatementMode::Sql,
            None => StatementMode::Undecided,
        },
        "create" => {
            for word in &words[1..] {
                match word.as_str() {
                    "or" | "replace" | "editionable" | "noneditionable" | "editioning"
                    | "force" | "noforce" | "and" | "resolve" | "compile" | "no" | "public"
                    | "shared" | "sharing" | "=" | "metadata" | "none" => {}
                    "procedure" | "function" | "package" | "trigger" | "type" | "library"
                    | "java" => return StatementMode::PlSql,
                    _ => return StatementMode::Sql,
                }
            }
            StatementMode::Undecided
        }
        _ => StatementMode::Sql,
    }
}

fn starts_statement(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('(') || trimmed.starts_with("<<") {
        return true;
    }
    let word = first_word(trimmed);
    if word.is_empty() {
        return false;
    }
    if SQL_WORDS.contains(&word.as_str()) {
        return true;
    }
    if word == "set" {
        let rest = trimmed[3..].trim_start();
        let next = first_word(rest);
        return matches!(
            next.as_str(),
            "transaction" | "role" | "constraint" | "constraints"
        );
    }
    false
}

pub(crate) fn command_named(word: &str) -> Option<&'static str> {
    if word.is_empty() {
        return None;
    }
    COMMANDS
        .iter()
        .find(|(name, minimum)| word.len() >= *minimum && name.starts_with(word))
        .map(|(name, _)| *name)
}

fn is_line_terminator(content: &str, terminator: &str) -> bool {
    content.trim() == terminator
}

fn include_name(argument: &str) -> (String, Vec<String>) {
    let argument = argument.trim();
    let (name, rest) = if let Some(quoted) = argument.strip_prefix('"') {
        match quoted.find('"') {
            Some(end) => (quoted[..end].to_owned(), &quoted[end + 1..]),
            None => (quoted.to_owned(), ""),
        }
    } else {
        let end = argument
            .find(char::is_whitespace)
            .unwrap_or(argument.len());
        (argument[..end].to_owned(), &argument[end..])
    };
    let arguments = rest.split_whitespace().map(str::to_owned).collect();
    (name, arguments)
}

fn with_default_suffix(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    if last.contains('.') {
        name.to_owned()
    } else {
        format!("{name}.sql")
    }
}

pub(crate) fn schema_of_user(user: &str) -> String {
    let user = user.trim();
    if let Some(quoted) = user.strip_prefix('"') {
        let inner = quoted.trim_end_matches('"');
        return crate::lexer::swap_uniform_case(inner);
    }
    let lower = user.to_ascii_lowercase();
    if lower == "sys" || lower == "system" || lower.is_empty() {
        return "public".to_owned();
    }
    lower
}

impl<'r> SqlPlusReader<'r> {
    pub(crate) fn new(input: &'r mut dyn BufRead, opener: Option<&'r dyn IncludeOpener>) -> Self {
        Self {
            scripts: vec![IncludedScript {
                lines: DecodedLines::new(input),
                script: 0,
            }],
            opener,
            names: vec![String::new()],
            variables: SubstitutionVariables::default(),
            lex: Lex::Normal,
            lex_line: 0,
            pending: None,
            comment_between: false,
            last_ended_by_semicolon: false,
            queued: Vec::new(),
            opened: 0,
            refusing_includes: false,
            ended: false,
            line: String::new(),
        }
    }

    pub(crate) fn define(&mut self, name: &str, value: &str) -> SqlResult<()> {
        self.variables.define(name, value)
    }

    fn current_script(&self) -> u32 {
        self.scripts.last().map_or(0, |script| script.script)
    }

    fn command_item(&self, name: &str, shown: String, argument: String, line: u64) -> ScriptItem {
        ScriptItem::ClientCommand(ClientCommand {
            name: name.to_owned(),
            shown,
            argument,
            line,
            script: self.current_script(),
        })
    }

    fn failure(&self, line: u64, shown: String, error: SqlError) -> ScriptItem {
        ScriptItem::Failed(ScriptFailure {
            line,
            script: self.current_script(),
            shown,
            error,
        })
    }

    fn read_line(&mut self) -> SqlResult<Option<u64>> {
        let Some(script) = self.scripts.last_mut() else {
            return Ok(None);
        };
        let mut line = std::mem::take(&mut self.line);
        let read = script.lines.next_line(&mut line).map_err(oracle_hint);
        let number = script.lines.line_number();
        self.line = line;
        match read? {
            true => Ok(Some(number)),
            false => Ok(None),
        }
    }

    fn end_of_script(&mut self) -> Option<ScriptItem> {
        let number = self
            .scripts
            .last()
            .map_or(0, |script| script.lines.line_number());
        let open = if self.comment_between {
            Some("a comment")
        } else {
            match self.lex {
                Lex::Normal => None,
                Lex::String | Lex::QQuote(_) => Some("a string"),
                Lex::Quoted => Some("a quoted name"),
                Lex::Comment => Some("a comment"),
            }
        };
        let pending = self.pending.take();
        let item = if let Some(what) = open {
            let error = SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "the script ends inside {what} that starts at line {}",
                    self.lex_line
                ),
            )
            .with_hint("The script is cut short or damaged; export it again.");
            Some(self.failure(number, String::new(), error))
        } else {
            pending
                .filter(|pending| !pending.text.trim().is_empty())
                .map(|pending| {
                    ScriptItem::ClientCommand(ClientCommand {
                        name: UNTERMINATED.to_owned(),
                        shown: super::shortened(&pending.text),
                        argument: String::new(),
                        line: pending.line,
                        script: pending.script,
                    })
                })
        };
        self.lex = Lex::Normal;
        self.comment_between = false;
        self.last_ended_by_semicolon = false;
        self.scripts.pop();
        item
    }

    fn handle_line(&mut self, line: &str, number: u64) -> SqlResult<Option<ScriptItem>> {
        let content = line.trim_end_matches(['\n', '\r']);
        if self.pending.is_some() {
            return self.continue_statement(content, number);
        }
        if self.comment_between {
            match content.find("*/") {
                Some(end) => {
                    self.comment_between = false;
                    let rest = content[end + 2..].to_owned();
                    if rest.trim().is_empty() {
                        return Ok(None);
                    }
                    return self.handle_line(&rest, number);
                }
                None => return Ok(None),
            }
        }
        let trimmed = content.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            return Ok(None);
        }
        if let Some(after) = trimmed.strip_prefix("/*") {
            match after.find("*/") {
                Some(end) => {
                    let rest = after[end + 2..].to_owned();
                    if rest.trim().is_empty() {
                        return Ok(None);
                    }
                    return self.handle_line(&rest, number);
                }
                None => {
                    self.comment_between = true;
                    self.lex_line = number;
                    return Ok(None);
                }
            }
        }
        if trimmed == "/" {
            if self.last_ended_by_semicolon {
                self.last_ended_by_semicolon = false;
                return Ok(Some(self.command_item(
                    "/",
                    "/".to_owned(),
                    String::new(),
                    number,
                )));
            }
            return Ok(None);
        }
        if trimmed == "." {
            return Ok(None);
        }
        self.last_ended_by_semicolon = false;
        if starts_statement(trimmed) {
            self.pending = Some(Pending {
                text: String::new(),
                line: number,
                script: self.current_script(),
                mode: StatementMode::Undecided,
                ended_by_semicolon: false,
            });
            return self.continue_statement(content.trim_start(), number);
        }
        self.command(trimmed, number)
    }

    fn continue_statement(&mut self, content: &str, number: u64) -> SqlResult<Option<ScriptItem>> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(None);
        };
        if self.lex == Lex::Normal && !pending.text.trim().is_empty() {
            if is_line_terminator(content, "/") {
                return self.finish_statement(number);
            }
            if is_line_terminator(content, ".") {
                let pending = self.pending.take();
                return Ok(pending.map(|pending| {
                    ScriptItem::ClientCommand(ClientCommand {
                        name: ".".to_owned(),
                        shown: super::shortened(&pending.text),
                        argument: String::new(),
                        line: pending.line,
                        script: pending.script,
                    })
                }));
            }
        }
        if pending.text.len() + content.len() + 1 > MAX_STATEMENT_BYTES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "the statement at line {} of the script is longer than 1 GiB",
                    pending.line
                ),
            ));
        }
        if !pending.text.is_empty() {
            pending.text.push('\n');
        }
        let offset = pending.text.len();
        pending.text.push_str(content);
        if pending.mode == StatementMode::Undecided {
            pending.mode = mode_of(&pending.text);
        }
        let plsql = pending.mode == StatementMode::PlSql;
        let end = self.scan(offset, number, !plsql);
        if let Some(at) = end {
            let Some(pending) = self.pending.as_mut() else {
                return Ok(None);
            };
            let rest = pending.text[at + 1..].to_owned();
            pending.text.truncate(at);
            pending.ended_by_semicolon = true;
            let item = self.finish_statement(number)?;
            if !rest.trim().is_empty() {
                self.queue_rest(rest, number)?;
            }
            return Ok(item);
        }
        Ok(None)
    }

    fn queue_rest(&mut self, rest: String, number: u64) -> SqlResult<()> {
        if let Some(item) = self.handle_line(&rest, number)? {
            self.queued.push(item);
        }
        Ok(())
    }

    fn scan(&mut self, from: usize, number: u64, semicolon_ends: bool) -> Option<usize> {
        let pending = self.pending.as_ref()?;
        let bytes = pending.text.as_bytes();
        let mut at = from;
        while at < bytes.len() {
            let byte = bytes[at];
            let next = bytes.get(at + 1).copied();
            match self.lex {
                Lex::Normal => match byte {
                    b'-' if next == Some(b'-') => {
                        while at < bytes.len() && bytes[at] != b'\n' {
                            at += 1;
                        }
                        continue;
                    }
                    b'/' if next == Some(b'*') => {
                        self.lex = Lex::Comment;
                        self.lex_line = number;
                        at += 1;
                    }
                    b'\'' => {
                        self.lex = Lex::String;
                        self.lex_line = number;
                    }
                    b'"' => {
                        self.lex = Lex::Quoted;
                        self.lex_line = number;
                    }
                    b'q' | b'Q' | b'n' | b'N' => {
                        let before = at.checked_sub(1).map(|index| bytes[index]);
                        let starts_word = before.is_none_or(|byte| {
                            !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#'))
                        });
                        let quote_at = if matches!(byte, b'n' | b'N')
                            && matches!(next, Some(b'q' | b'Q'))
                        {
                            at + 2
                        } else if matches!(byte, b'q' | b'Q') {
                            at + 1
                        } else {
                            usize::MAX
                        };
                        if starts_word
                            && bytes.get(quote_at) == Some(&b'\'')
                            && let Some(&open) = bytes.get(quote_at + 1)
                            && !open.is_ascii_whitespace()
                        {
                            let close = match open {
                                b'[' => b']',
                                b'{' => b'}',
                                b'<' => b'>',
                                b'(' => b')',
                                other => other,
                            };
                            self.lex = Lex::QQuote(close);
                            self.lex_line = number;
                            at = quote_at + 1;
                        }
                    }
                    b';' if semicolon_ends => return Some(at),
                    _ => {}
                },
                Lex::String => {
                    if byte == b'\'' {
                        if next == Some(b'\'') {
                            at += 1;
                        } else {
                            self.lex = Lex::Normal;
                        }
                    }
                }
                Lex::QQuote(close) => {
                    if byte == close && next == Some(b'\'') {
                        self.lex = Lex::Normal;
                        at += 1;
                    }
                }
                Lex::Quoted => {
                    if byte == b'"' {
                        self.lex = Lex::Normal;
                    }
                }
                Lex::Comment => {
                    if byte == b'*' && next == Some(b'/') {
                        self.lex = Lex::Normal;
                        at += 1;
                    }
                }
            }
            at += 1;
        }
        None
    }

    fn finish_statement(&mut self, number: u64) -> SqlResult<Option<ScriptItem>> {
        let Some(pending) = self.pending.take() else {
            return Ok(None);
        };
        self.lex = Lex::Normal;
        self.last_ended_by_semicolon = pending.ended_by_semicolon;
        let text = pending.text.trim_end();
        if text.trim().is_empty() {
            return Ok(None);
        }
        let substituted = self.variables.substitute(text)?;
        let mut statement = ScriptStatement::plain(substituted.text, pending.line, number);
        statement.script = pending.script;
        if let Some(name) = substituted.unresolved {
            statement.unresolved = Some(unresolved_error(&name, pending.line));
        }
        Ok(Some(ScriptItem::Statement(statement)))
    }

    fn command_text(&mut self, first: &str, number: u64) -> SqlResult<String> {
        let mut text = first.trim_end().to_owned();
        while text.ends_with('-') {
            text.pop();
            if self.read_line()?.is_none() {
                break;
            }
            let line = self.line.trim_end_matches(['\n', '\r']).trim().to_owned();
            if text.len() + line.len() > MAX_STATEMENT_BYTES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("the SQL*Plus command at line {number} is longer than 1 GiB"),
                ));
            }
            text.push(' ');
            text.push_str(&line);
        }
        let trimmed = text.trim_end();
        let trimmed = trimmed.strip_suffix(';').unwrap_or(trimmed);
        Ok(trimmed.trim().to_owned())
    }

    fn command(&mut self, line: &str, number: u64) -> SqlResult<Option<ScriptItem>> {
        let raw = self.command_text(line, number)?;
        if let Some(rest) = raw.strip_prefix("@@") {
            return self.include(rest, IncludeBase::IncludingScript(self.current_script()), "@@", number);
        }
        if let Some(rest) = raw.strip_prefix('@') {
            return self.include(rest, IncludeBase::WorkingDirectory, "@", number);
        }
        if raw.starts_with('!') || raw.starts_with('$') {
            return Ok(Some(self.command_item(
                "host",
                raw[..1].to_owned(),
                String::new(),
                number,
            )));
        }
        let word = first_word(&raw);
        let Some(command) = command_named(&word) else {
            let shown = if word.is_empty() {
                super::shortened(&raw)
            } else {
                word.to_ascii_uppercase()
            };
            return Ok(Some(self.command_item(
                "unknown",
                shown,
                String::new(),
                number,
            )));
        };
        if command == "remark" {
            return Ok(None);
        }
        let rest = raw[word.len()..].trim().to_owned();
        let substituted = self.variables.substitute(&rest)?;
        let argument = substituted.text;
        match command {
            "set" => self.set(&argument, number),
            "define" => self.define_command(&argument, number),
            "undefine" => {
                for name in argument.split_whitespace() {
                    self.variables.undefine(name);
                }
                Ok(Some(self.command_item(
                    "undefine",
                    "UNDEFINE".to_owned(),
                    String::new(),
                    number,
                )))
            }
            "accept" => self.accept(&argument, number),
            "connect" => self.connect(&argument, substituted.unresolved, number),
            "start" => self.include(&argument, IncludeBase::WorkingDirectory, "START ", number),
            "execute" => {
                let mut statement =
                    ScriptStatement::plain(format!("EXECUTE {argument}"), number, number);
                statement.script = self.current_script();
                if let Some(name) = substituted.unresolved {
                    statement.unresolved = Some(unresolved_error(&name, number));
                }
                Ok(Some(ScriptItem::Statement(statement)))
            }
            "exit" | "quit" => {
                self.ended = true;
                Ok(Some(self.command_item(
                    "exit",
                    command.to_ascii_uppercase(),
                    String::new(),
                    number,
                )))
            }
            "whenever" => {
                let what = first_word(&argument).to_ascii_uppercase();
                Ok(Some(self.command_item(
                    "whenever",
                    format!("WHENEVER {what}"),
                    String::new(),
                    number,
                )))
            }
            other => Ok(Some(self.command_item(
                other,
                other.to_ascii_uppercase(),
                String::new(),
                number,
            ))),
        }
    }

    fn set(&mut self, argument: &str, number: u64) -> SqlResult<Option<ScriptItem>> {
        let option = first_word(argument);
        let value = argument[option.len().min(argument.len())..].trim();
        let value_word = value.to_ascii_lowercase();
        let named = |full: &str, minimum: usize| {
            option.len() >= minimum && full.starts_with(option.as_str())
        };
        let one_character = |value: &str| {
            let mut characters = value.chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) if !character.is_alphanumeric() => Some(character),
                _ => None,
            }
        };
        let shown = format!("SET {}", option.to_ascii_uppercase());
        if named("define", 3) || named("scan", 3) {
            match value_word.as_str() {
                "on" => self.variables.set_define(Some('&')),
                "off" => self.variables.set_define(None),
                _ => match one_character(value) {
                    Some(character) if named("define", 3) => {
                        self.variables.set_define(Some(character));
                    }
                    _ => return Ok(Some(self.bad_setting(&shown, number))),
                },
            }
            return Ok(Some(self.command_item("set define", shown, String::new(), number)));
        }
        if named("escape", 3) {
            match value_word.as_str() {
                "on" => self.variables.escape = Some('\\'),
                "off" => self.variables.escape = None,
                _ => match one_character(value) {
                    Some(character) => self.variables.escape = Some(character),
                    None => return Ok(Some(self.bad_setting(&shown, number))),
                },
            }
            return Ok(Some(self.command_item("set define", shown, String::new(), number)));
        }
        if named("concat", 3) {
            match value_word.as_str() {
                "on" => self.variables.concat = Some('.'),
                "off" => self.variables.concat = None,
                _ => match one_character(value) {
                    Some(character) => self.variables.concat = Some(character),
                    None => return Ok(Some(self.bad_setting(&shown, number))),
                },
            }
            return Ok(Some(self.command_item("set define", shown, String::new(), number)));
        }
        let changes_reading = (named("sqlterminator", 4) && !matches!(value_word.as_str(), "on" | ";"))
            || (named("blockterminator", 3) && !matches!(value_word.as_str(), "on" | "."))
            || (named("sqlprefix", 6) && value_word != "#")
            || (named("sqlblanklines", 5) && value_word == "off");
        if changes_reading {
            return Ok(Some(self.failure(
                number,
                shown.clone(),
                SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!(
                        "line {number}: {shown} changes how the script is read, which eggdb restore does not support"
                    ),
                )
                .with_hint("Remove the SET line; eggdb restore reads ; and / as SQL*Plus does by default."),
            )));
        }
        Ok(Some(self.command_item("set", shown, String::new(), number)))
    }

    fn bad_setting(&self, shown: &str, number: u64) -> ScriptItem {
        self.failure(
            number,
            shown.to_owned(),
            SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("line {number}: {shown} takes ON, OFF or one character"),
            ),
        )
    }

    fn define_command(&mut self, argument: &str, number: u64) -> SqlResult<Option<ScriptItem>> {
        let name_length = argument
            .find(|character: char| !is_variable_character(character))
            .unwrap_or(argument.len());
        let name = &argument[..name_length];
        let rest = argument[name_length..].trim_start();
        let Some(value) = rest.strip_prefix('=') else {
            let shown = if name.is_empty() {
                "DEFINE".to_owned()
            } else {
                format!("DEFINE {}", name.to_ascii_uppercase())
            };
            return Ok(Some(self.command_item("define", shown, String::new(), number)));
        };
        let value = value.trim();
        let value = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            inner.to_owned()
        } else if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            inner.to_owned()
        } else {
            value.to_owned()
        };
        let shown = format!("DEFINE {}", name.to_ascii_uppercase());
        if name.is_empty() {
            return Ok(Some(self.failure(
                number,
                "DEFINE".to_owned(),
                SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("line {number}: DEFINE needs a variable name"),
                ),
            )));
        }
        if let Err(error) = self.variables.define(name, &value) {
            return Ok(Some(self.failure(number, shown, error)));
        }
        Ok(Some(self.command_item("define", shown, String::new(), number)))
    }

    fn accept(&mut self, argument: &str, number: u64) -> SqlResult<Option<ScriptItem>> {
        let name = first_word(argument);
        let shown = format!("ACCEPT {}", name.to_ascii_uppercase());
        let lower = argument.to_ascii_lowercase();
        if !name.is_empty()
            && !self.variables.is_defined(&name)
            && let Some(at) = find_word(&lower, "default")
        {
            let after = argument[at + "default".len()..].trim_start();
            let value = if let Some(quoted) = after.strip_prefix('\'') {
                quoted.split('\'').next().unwrap_or("").to_owned()
            } else if let Some(quoted) = after.strip_prefix('"') {
                quoted.split('"').next().unwrap_or("").to_owned()
            } else {
                after.split_whitespace().next().unwrap_or("").to_owned()
            };
            let resolved = !self.variables.define_character.is_some_and(|define| value.contains(define));
            if resolved && let Err(error) = self.variables.define(&name, &value) {
                return Ok(Some(self.failure(number, shown, error)));
            }
        }
        Ok(Some(self.command_item("accept", shown, String::new(), number)))
    }

    fn connect(
        &mut self,
        argument: &str,
        unresolved: Option<String>,
        number: u64,
    ) -> SqlResult<Option<ScriptItem>> {
        let user = argument
            .split(|character: char| character == '/' || character == '@' || character.is_whitespace())
            .next()
            .unwrap_or("");
        if let Some(name) = unresolved
            && user.contains('&')
        {
            return Ok(Some(self.failure(
                number,
                "CONNECT".to_owned(),
                unresolved_error(&name, number),
            )));
        }
        let schema = schema_of_user(user);
        let _ = self
            .variables
            .define("_USER", &schema.to_ascii_uppercase());
        let shown = if user.is_empty() {
            "CONNECT /".to_owned()
        } else {
            format!("CONNECT {}", user.to_ascii_uppercase())
        };
        Ok(Some(self.command_item("connect", shown, schema, number)))
    }

    fn include(
        &mut self,
        argument: &str,
        base: IncludeBase,
        prefix: &str,
        number: u64,
    ) -> SqlResult<Option<ScriptItem>> {
        let substituted = self.variables.substitute(argument)?;
        let (name, arguments) = include_name(&substituted.text);
        let shown = format!("{prefix}{name}");
        if let Some(variable) = substituted.unresolved
            && name.contains('&')
        {
            return Ok(Some(self.failure(number, shown, unresolved_error(&variable, number))));
        }
        if name.is_empty() {
            return Ok(Some(self.failure(
                number,
                shown,
                SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("line {number}: {prefix} needs a script name"),
                ),
            )));
        }
        let lower = name.to_ascii_lowercase();
        if ["http://", "https://", "ftp://"]
            .iter()
            .any(|scheme| lower.starts_with(scheme))
        {
            return Ok(Some(self.failure(
                number,
                shown,
                SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("line {number}: eggdb restore never fetches a script from a URL"),
                )
                .with_hint("Download the script and include it by its file name."),
            )));
        }
        for (index, value) in arguments.iter().enumerate() {
            if let Err(error) = self.variables.define(&(index + 1).to_string(), value) {
                return Ok(Some(self.failure(number, shown, error)));
            }
        }
        if self.scripts.len() > MAX_INCLUDE_DEPTH {
            return Ok(Some(self.failure(
                number,
                shown,
                SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "line {number}: scripts are nested more than {MAX_INCLUDE_DEPTH} deep (SQL*Plus's SP2-0309 limit)"
                    ),
                ),
            )));
        }
        if self.refusing_includes || self.opened >= MAX_INCLUDES_OPENED {
            self.refusing_includes = true;
            return Ok(Some(self.failure(
                number,
                shown,
                SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "line {number}: the restore has opened {MAX_INCLUDES_OPENED} included scripts, its limit"
                    ),
                ),
            )));
        }
        let Some(opener) = self.opener else {
            return Ok(Some(self.failure(
                number,
                shown,
                SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("line {number}: includes need the script as a file"),
                )
                .with_hint("Restore the script from its file, not from stdin, or paste the included script in its place."),
            )));
        };
        let file = with_default_suffix(&name);
        let script = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        let request = IncludeRequest {
            name: &file,
            relative_to: base,
            script,
            line: number,
        };
        self.opened += 1;
        match opener.open(&request) {
            Ok(input) => {
                self.names.push(file.clone());
                let command = self.command_item("include", shown, file, number);
                self.scripts.push(IncludedScript {
                    lines: DecodedLines::owning(input),
                    script,
                });
                Ok(Some(command))
            }
            Err(error) => Ok(Some(self.failure(number, shown, error))),
        }
    }
}

fn find_word(text: &str, word: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text.get(from..)?.find(word) {
        let at = from + offset;
        let before = at.checked_sub(1).map(|index| bytes[index]);
        let after = bytes.get(at + word.len()).copied();
        let boundary =
            |byte: Option<u8>| byte.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'));
        if boundary(before) && boundary(after) {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

fn oracle_hint(error: SqlError) -> SqlError {
    if error.state() == SqlState::CHARACTER_NOT_IN_REPERTOIRE {
        return error.with_hint(
            "Export the script as UTF-8, or convert it, for example iconv -f CP1252 -t UTF-8 hr.sql > hr-utf8.sql (Data Pump writes SQLFILE= output in the database character set).",
        );
    }
    error
}

impl ScriptSource for SqlPlusReader<'_> {
    fn next_item(&mut self) -> SqlResult<ScriptItem> {
        loop {
            if !self.queued.is_empty() {
                return Ok(self.queued.remove(0));
            }
            if self.ended {
                return Ok(ScriptItem::End);
            }
            if self.scripts.is_empty() {
                self.ended = true;
                return Ok(ScriptItem::End);
            }
            let Some(number) = self.read_line()? else {
                if let Some(item) = self.end_of_script() {
                    return Ok(item);
                }
                continue;
            };
            let line = std::mem::take(&mut self.line);
            let item = self.handle_line(&line, number);
            self.line = line;
            if let Some(item) = item? {
                if self.ended {
                    self.pending = None;
                    self.queued.clear();
                }
                return Ok(item);
            }
        }
    }

    fn line_number(&self) -> u64 {
        self.scripts
            .last()
            .map_or(0, |script| script.lines.line_number())
    }

    fn skip_data(&mut self) -> SqlResult<u64> {
        Ok(0)
    }

    fn copy_data(&mut self, _out: &mut Vec<u8>) -> SqlResult<bool> {
        Ok(false)
    }

    fn copy_lines(&self) -> u64 {
        0
    }

    fn script_index(&self) -> u32 {
        self.current_script()
    }

    fn script_names(&self) -> Vec<String> {
        self.names.clone()
    }
}
