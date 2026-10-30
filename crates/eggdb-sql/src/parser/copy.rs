use super::Parser;
use crate::ast::{CopyDirection, CopySource, CopyStatement, Name, Statement};
use crate::copy::{
    CopyColumns, CopyEncoding, CopyFormat, CopyHeader, CopyLogVerbosity, CopyOnError, CopyOptions,
    EncodingName, Layout,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

const TEXT_DELIMITERS_REFUSED: &[u8] = b"\\.abcdefghijklmnopqrstuvwxyz0123456789";
const SUPPORTED_ENCODINGS_HINT: &str =
    "EggDB's COPY reads and writes UTF8, LATIN1, LATIN9, WIN1252 and UTF16 (UTF16LE, UTF16BE).";
const SERVER_FILE_HINT: &str =
    "Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.";

enum Argument {
    Absent,
    Text(String),
    Star,
    Names(Vec<Name>),
}

#[derive(Default)]
struct OptionsBuilder {
    options: CopyOptions,
    seen: Vec<&'static str>,
    freeze: bool,
    on_error: bool,
}

fn invalid_value(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn unsupported(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, message)
}

fn option_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "format" => "format",
        "freeze" => "freeze",
        "delimiter" => "delimiter",
        "null" => "null",
        "default" => "default",
        "header" => "header",
        "quote" => "quote",
        "escape" => "escape",
        "force_quote" => "force_quote",
        "force_not_null" => "force_not_null",
        "force_null" => "force_null",
        "encoding" => "encoding",
        "on_error" => "on_error",
        "log_verbosity" => "log_verbosity",
        _ => return None,
    })
}

fn boolean(key: &str, argument: &Argument) -> SqlResult<bool> {
    match argument {
        Argument::Absent => Ok(true),
        Argument::Text(text) => match text.to_ascii_lowercase().as_str() {
            "true" | "on" | "1" => Ok(true),
            "false" | "off" | "0" => Ok(false),
            _ => Err(invalid_value(format!("{key} requires a Boolean value"))),
        },
        Argument::Star | Argument::Names(_) => {
            Err(invalid_value(format!("{key} requires a Boolean value")))
        }
    }
}

fn text(key: &str, argument: Argument) -> SqlResult<String> {
    match argument {
        Argument::Text(text) => Ok(text),
        _ => Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("{key} requires a parameter"),
        )),
    }
}

fn one_byte(key: &str, argument: Argument, what: &str) -> SqlResult<u8> {
    let text = text(key, argument)?;
    match text.as_bytes() {
        [byte] => Ok(*byte),
        _ => Err(unsupported(format!(
            "COPY {what} must be a single one-byte character"
        ))),
    }
}

fn columns(key: &str, argument: Argument) -> SqlResult<CopyColumns> {
    match argument {
        Argument::Star => Ok(CopyColumns::All),
        Argument::Names(names) => Ok(CopyColumns::Named(names)),
        Argument::Absent | Argument::Text(_) => Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("argument to option \"{key}\" must be a list of column names"),
        )),
    }
}

impl OptionsBuilder {
    fn set(&mut self, name: &str, argument: Argument, position: usize) -> SqlResult<()> {
        let Some(key) = option_key(name) else {
            return Err(SqlError::syntax(
                format!("option \"{name}\" not recognized"),
                position,
            ));
        };
        if self.seen.contains(&key) {
            return Err(SqlError::syntax(
                "conflicting or redundant options",
                position,
            ));
        }
        self.seen.push(key);
        let options = &mut self.options;
        match key {
            "format" => {
                let format = text(key, argument)?;
                options.format = match format.to_ascii_lowercase().as_str() {
                    "text" => CopyFormat::Text,
                    "csv" => CopyFormat::Csv,
                    "binary" => CopyFormat::Binary,
                    _ => {
                        return Err(invalid_value(format!(
                            "COPY format \"{format}\" not recognized"
                        ))
                        .at(position));
                    }
                };
            }
            "freeze" => self.freeze = boolean(key, &argument)?,
            "delimiter" => options.delimiter = Some(one_byte(key, argument, "delimiter")?),
            "null" => options.null = Some(text(key, argument)?),
            "default" => options.default = Some(text(key, argument)?),
            "header" => {
                options.header = match &argument {
                    Argument::Text(text) if text.eq_ignore_ascii_case("match") => CopyHeader::Match,
                    _ => match boolean(key, &argument) {
                        Ok(true) => CopyHeader::Present,
                        Ok(false) => CopyHeader::Absent,
                        Err(_) => {
                            return Err(invalid_value(
                                "header requires a Boolean value or \"match\"",
                            ));
                        }
                    },
                };
            }
            "quote" => options.quote = Some(one_byte(key, argument, "quote")?),
            "escape" => options.escape = Some(one_byte(key, argument, "escape")?),
            "force_quote" => options.force_quote = columns(key, argument)?,
            "force_not_null" => options.force_not_null = columns(key, argument)?,
            "force_null" => options.force_null = columns(key, argument)?,
            "encoding" => {
                let encoding = text(key, argument)?;
                options.encoding = match CopyEncoding::named(&encoding) {
                    EncodingName::Supported(encoding) => encoding,
                    EncodingName::NotSupported => {
                        return Err(unsupported(format!(
                            "COPY with ENCODING \"{encoding}\" is not supported yet"
                        ))
                        .with_hint(SUPPORTED_ENCODINGS_HINT)
                        .at(position));
                    }
                    EncodingName::Unknown => {
                        return Err(invalid_value(
                            "argument to option \"encoding\" must be a valid encoding name",
                        )
                        .at(position));
                    }
                };
            }
            "log_verbosity" => {
                let verbosity = text(key, argument)?;
                options.log_verbosity = match verbosity.to_ascii_lowercase().as_str() {
                    "default" => CopyLogVerbosity::Default,
                    "verbose" => CopyLogVerbosity::Verbose,
                    "silent" => CopyLogVerbosity::Silent,
                    _ => {
                        return Err(invalid_value(format!(
                            "COPY LOG_VERBOSITY \"{verbosity}\" not recognized"
                        ))
                        .at(position));
                    }
                };
            }
            _ => {
                let action = text(key, argument)?;
                self.on_error = true;
                options.on_error = match action.to_ascii_lowercase().as_str() {
                    "stop" => CopyOnError::Stop,
                    "ignore" => CopyOnError::Ignore,
                    _ => {
                        return Err(invalid_value(format!(
                            "COPY ON_ERROR \"{action}\" not recognized"
                        ))
                        .at(position));
                    }
                };
            }
        }
        Ok(())
    }

    fn check_binary(&self) -> SqlResult<()> {
        let options = &self.options;
        if options.format != CopyFormat::Binary {
            return Ok(());
        }
        let text_only = [
            (options.delimiter.is_some(), "DELIMITER"),
            (options.null.is_some(), "NULL"),
            (options.default.is_some(), "DEFAULT"),
            (options.header != CopyHeader::Absent, "HEADER"),
        ];
        if let Some((_, option)) = text_only.iter().find(|(given, _)| *given) {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("cannot specify {option} in BINARY mode"),
            ));
        }
        if options.on_error == CopyOnError::Ignore {
            return Err(unsupported("only ON_ERROR STOP is allowed in BINARY mode"));
        }
        Ok(())
    }

    fn check_modes(&self, direction: CopyDirection) -> SqlResult<()> {
        self.check_binary()?;
        let options = &self.options;
        if options.format != CopyFormat::Csv {
            let csv_only = [
                (options.quote.is_some(), "quote"),
                (options.escape.is_some(), "escape"),
                (options.force_quote != CopyColumns::None, "force quote"),
                (
                    options.force_not_null != CopyColumns::None,
                    "force not null",
                ),
                (options.force_null != CopyColumns::None, "force null"),
            ];
            if let Some((_, what)) = csv_only.iter().find(|(given, _)| *given) {
                return Err(unsupported(format!(
                    "COPY {what} available only in CSV mode"
                )));
            }
        }
        let refused = match direction {
            CopyDirection::FromClient if options.force_quote != CopyColumns::None => {
                Some("COPY force quote only available using COPY TO")
            }
            CopyDirection::ToClient if options.force_not_null != CopyColumns::None => {
                Some("COPY force not null only available using COPY FROM")
            }
            CopyDirection::ToClient if options.force_null != CopyColumns::None => {
                Some("COPY force null only available using COPY FROM")
            }
            CopyDirection::ToClient if options.default.is_some() => {
                Some("COPY DEFAULT only available using COPY FROM")
            }
            CopyDirection::ToClient if options.header == CopyHeader::Match => {
                Some("cannot use \"match\" with HEADER in COPY TO")
            }
            CopyDirection::ToClient if self.freeze => {
                Some("COPY FREEZE cannot be used with COPY TO")
            }
            CopyDirection::ToClient if self.on_error => {
                Some("COPY ON_ERROR cannot be used with COPY TO")
            }
            _ => None,
        };
        match refused {
            Some(message) => Err(unsupported(message)),
            None => Ok(()),
        }
    }

    fn check_characters(&self) -> SqlResult<()> {
        let layout = Layout::of(&self.options);
        let line_break = |byte: &u8| *byte == b'\n' || *byte == b'\r';
        if line_break(&layout.delimiter) {
            return Err(invalid_value(
                "COPY delimiter cannot be newline or carriage return",
            ));
        }
        if layout.csv && (line_break(&layout.quote) || line_break(&layout.escape)) {
            return Err(invalid_value(
                "COPY quote and escape cannot be newline or carriage return",
            ));
        }
        if layout.null.iter().any(line_break) {
            return Err(invalid_value(
                "COPY null representation cannot use newline or carriage return",
            ));
        }
        if !layout.csv && TEXT_DELIMITERS_REFUSED.contains(&layout.delimiter) {
            return Err(invalid_value(format!(
                "COPY delimiter cannot be \"{}\"",
                char::from(layout.delimiter)
            )));
        }
        if layout.csv && layout.delimiter == layout.quote {
            return Err(invalid_value("COPY delimiter and quote must be different"));
        }
        if layout.null.contains(&layout.delimiter) {
            return Err(invalid_value(
                "COPY delimiter must not appear in the NULL specification",
            ));
        }
        if layout.csv && layout.null.contains(&layout.quote) {
            return Err(invalid_value(
                "CSV quote character must not appear in the NULL specification",
            ));
        }
        if let Some(default) = &layout.default {
            if default.iter().any(line_break) {
                return Err(invalid_value(
                    "COPY default representation cannot use newline or carriage return",
                ));
            }
            if default.contains(&layout.delimiter) {
                return Err(invalid_value(
                    "COPY delimiter must not appear in the DEFAULT specification",
                ));
            }
            if layout.csv && default.contains(&layout.quote) {
                return Err(invalid_value(
                    "CSV quote character must not appear in the DEFAULT specification",
                ));
            }
            if *default == layout.null {
                return Err(invalid_value(
                    "NULL specification and DEFAULT specification cannot be the same",
                ));
            }
        }
        Ok(())
    }

    fn finish(self, direction: CopyDirection) -> SqlResult<CopyOptions> {
        self.check_modes(direction)?;
        self.check_characters()?;
        Ok(self.options)
    }
}

impl Parser<'_> {
    pub(super) fn copy(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let binary_position = self.position();
        let binary = self.take_word("binary");
        let source = if self.peek_kind() == &TokenKind::LeftParen {
            self.at += 1;
            let query = self.nested_query()?;
            self.expect(&TokenKind::RightParen)?;
            CopySource::Query(Box::new(query))
        } else {
            let name = self.table_name()?;
            let columns = if self.peek_kind() == &TokenKind::LeftParen {
                self.name_list()?
            } else {
                Vec::new()
            };
            CopySource::Table { name, columns }
        };
        let direction = if self.is_word("from") && matches!(source, CopySource::Table { .. }) {
            CopyDirection::FromClient
        } else if self.is_word("to") {
            CopyDirection::ToClient
        } else {
            return Err(self.unexpected());
        };
        self.at += 1;
        self.copy_target(direction)?;
        let mut builder = OptionsBuilder::default();
        if binary {
            builder.set("format", Argument::Text("binary".into()), binary_position)?;
        }
        let _ = self.take_word("with");
        if self.peek_kind() == &TokenKind::LeftParen {
            self.copy_option_list(&mut builder)?;
        } else {
            self.legacy_copy_options(&mut builder)?;
        }
        let condition = if self.is_word("where") {
            if direction == CopyDirection::ToClient {
                return Err(unsupported("WHERE clause not allowed with COPY TO").at(self.position()));
            }
            self.at += 1;
            Some(self.expression()?)
        } else {
            None
        };
        let options = builder.finish(direction)?;
        Ok(Statement::Copy(Box::new(CopyStatement {
            source,
            direction,
            options,
            condition,
        })))
    }

    fn copy_target(&mut self, direction: CopyDirection) -> SqlResult<()> {
        let position = self.position();
        let (preposition, verb) = match direction {
            CopyDirection::FromClient => ("from", "reads"),
            CopyDirection::ToClient => ("to", "writes"),
        };
        match self.peek_kind() {
            TokenKind::Word(word) if word == "stdin" || word == "stdout" => {
                self.at += 1;
                Ok(())
            }
            TokenKind::String(_) => Err(SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "COPY {preposition} a file is not allowed: EggDB never {verb} files on the server from SQL"
                ),
            )
            .with_hint(SERVER_FILE_HINT)
            .at(position)),
            TokenKind::Word(word) if word == "program" => Err(SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "COPY {preposition} a program is not allowed: EggDB never runs programs on the server from SQL"
                ),
            )
            .with_hint(SERVER_FILE_HINT)
            .at(position)),
            _ => Err(self.unexpected()),
        }
    }

    fn copy_option_list(&mut self, builder: &mut OptionsBuilder) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        loop {
            let position = self.position();
            let name = match self.peek_kind() {
                TokenKind::Word(word) => word.as_ref().to_owned(),
                TokenKind::QuotedIdentifier(name) => name.clone(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            let argument = self.copy_option_argument()?;
            builder.set(&name, argument, position)?;
            if !self.take(&TokenKind::Comma) {
                return self.expect(&TokenKind::RightParen);
            }
        }
    }

    fn copy_option_argument(&mut self) -> SqlResult<Argument> {
        let argument = match self.peek_kind() {
            TokenKind::Comma | TokenKind::RightParen => return Ok(Argument::Absent),
            TokenKind::LeftParen => return self.name_list().map(Argument::Names),
            TokenKind::Operator("*") => Argument::Star,
            TokenKind::Word(word) => Argument::Text(word.as_ref().to_owned()),
            TokenKind::QuotedIdentifier(name) => Argument::Text(name.clone()),
            TokenKind::String(text) => Argument::Text(text.as_ref().to_owned()),
            TokenKind::Integer(digits) | TokenKind::Decimal(digits) => {
                Argument::Text((*digits).to_owned())
            }
            TokenKind::Operator(sign @ ("-" | "+")) => {
                let sign = *sign;
                let (TokenKind::Integer(digits) | TokenKind::Decimal(digits)) =
                    self.peek_kind_at(1)
                else {
                    return Err(self.unexpected());
                };
                self.at += 1;
                Argument::Text(format!("{}{digits}", sign.trim_start_matches('+')))
            }
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(argument)
    }

    fn copy_string(&mut self) -> SqlResult<Argument> {
        let _ = self.take_word("as");
        match self.peek_kind() {
            TokenKind::String(text) => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                Ok(Argument::Text(text))
            }
            _ => Err(self.unexpected()),
        }
    }

    fn bare_column_names(&mut self) -> SqlResult<Vec<Name>> {
        let mut names = vec![self.name()?];
        while self.take(&TokenKind::Comma) {
            names.push(self.name()?);
        }
        Ok(names)
    }

    fn legacy_copy_options(&mut self, builder: &mut OptionsBuilder) -> SqlResult<()> {
        loop {
            let position = self.position();
            let TokenKind::Word(word) = self.peek_kind() else {
                return Ok(());
            };
            let word = word.as_ref().to_owned();
            match word.as_str() {
                "binary" | "csv" => {
                    self.at += 1;
                    builder.set("format", Argument::Text(word), position)?;
                }
                "header" | "freeze" => {
                    self.at += 1;
                    builder.set(&word, Argument::Absent, position)?;
                }
                "delimiter" | "null" | "quote" | "escape" | "encoding" => {
                    self.at += 1;
                    let argument = self.copy_string()?;
                    builder.set(&word, argument, position)?;
                }
                "force" => {
                    self.at += 1;
                    let (key, argument) = if self.take_word("quote") {
                        if self.take_operator("*") {
                            ("force_quote", Argument::Star)
                        } else {
                            ("force_quote", Argument::Names(self.bare_column_names()?))
                        }
                    } else if self.take_word("not") {
                        self.expect_word("null")?;
                        ("force_not_null", Argument::Names(self.bare_column_names()?))
                    } else if self.take_word("null") {
                        ("force_null", Argument::Names(self.bare_column_names()?))
                    } else {
                        return Err(self.unexpected());
                    };
                    builder.set(key, argument, position)?;
                }
                _ => return Ok(()),
            }
        }
    }
}
