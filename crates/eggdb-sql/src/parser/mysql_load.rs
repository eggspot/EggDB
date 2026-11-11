use super::Parser;
use crate::ast::{LoadData, LoadDuplicates, LoadTarget, MySqlStatement, Statement};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

const DEFAULT_FIELDS_TERMINATED: &[u8] = b"\t";
const DEFAULT_LINES_TERMINATED: &[u8] = b"\n";
const DEFAULT_ESCAPE: u8 = b'\\';

impl Parser<'_> {
    fn load_string(&mut self) -> SqlResult<Vec<u8>> {
        let TokenKind::String(text) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let bytes = text.as_bytes().to_vec();
        self.at += 1;
        Ok(bytes)
    }

    fn load_character(&mut self) -> SqlResult<Option<u8>> {
        let position = self.position();
        let bytes = self.load_string()?;
        match bytes.as_slice() {
            [] => Ok(None),
            [byte] => Ok(Some(*byte)),
            _ => Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                "Field separator argument is not what is expected; check the manual",
            )
            .with_vendor_code(1083)
            .at(position)),
        }
    }

    pub(super) fn mysql_load_data(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("xml") {
            return Err(self.not_supported("LOAD XML"));
        }
        self.expect_word("data")?;
        let _ = self.take_word("low_priority") || self.take_word("concurrent");
        let local = self.take_word("local");
        self.expect_word("infile")?;
        let file = String::from_utf8_lossy(&self.load_string()?).into_owned();
        let mut duplicates = if self.take_word("replace") {
            LoadDuplicates::Replace
        } else if self.take_word("ignore") {
            LoadDuplicates::Ignore
        } else {
            LoadDuplicates::Error
        };
        if local && duplicates == LoadDuplicates::Error {
            duplicates = LoadDuplicates::Ignore;
        }
        self.expect_word("into")?;
        self.expect_word("table")?;
        let table_start = self.peek().start;
        let table = self.table_name()?;
        let table_text = self.slice(table_start, self.previous_end());
        let partitions = if self.is_word("partition") {
            let start = self.peek().start;
            self.at += 1;
            self.expect(&TokenKind::LeftParen)?;
            loop {
                self.name()?;
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
            Some(self.slice(start, self.previous_end()))
        } else {
            None
        };
        if self.is_word("character") && self.is_word_at(1, "set") {
            self.at += 2;
            let charset = self.word_text_or_string();
            if !matches!(charset.as_deref(), Some("utf8mb4" | "utf8" | "utf8mb3" | "binary" | "ascii" | "latin1")) {
                return Err(self.not_supported("LOAD DATA with this CHARACTER SET"));
            }
            self.at += 1;
        }
        let mut load = LoadData {
            local,
            file,
            duplicates,
            table,
            table_text,
            partitions,
            fields_terminated: DEFAULT_FIELDS_TERMINATED.to_vec(),
            enclosed: None,
            optionally_enclosed: false,
            escaped: Some(DEFAULT_ESCAPE),
            lines_starting: Vec::new(),
            lines_terminated: DEFAULT_LINES_TERMINATED.to_vec(),
            ignore_lines: 0,
            targets: None,
            assignments: Vec::new(),
        };
        if self.take_word("fields") || self.take_word("columns") {
            loop {
                if self.is_word("terminated") && self.is_word_at(1, "by") {
                    self.at += 2;
                    load.fields_terminated = self.load_string()?;
                } else if self.is_word("optionally")
                    || (self.is_word("enclosed") && self.is_word_at(1, "by"))
                {
                    load.optionally_enclosed = self.take_word("optionally");
                    self.expect_word("enclosed")?;
                    self.expect_word("by")?;
                    load.enclosed = self.load_character()?;
                } else if self.is_word("escaped") && self.is_word_at(1, "by") {
                    self.at += 2;
                    load.escaped = self.load_character()?;
                } else {
                    break;
                }
            }
        }
        if self.take_word("lines") {
            loop {
                if self.is_word("starting") && self.is_word_at(1, "by") {
                    self.at += 2;
                    load.lines_starting = self.load_string()?;
                } else if self.is_word("terminated") && self.is_word_at(1, "by") {
                    self.at += 2;
                    load.lines_terminated = self.load_string()?;
                } else {
                    break;
                }
            }
        }
        if load.lines_terminated.is_empty() {
            return Err(self.not_supported("LOAD DATA with empty LINES TERMINATED BY"));
        }
        if self.take_word("ignore") {
            let TokenKind::Integer(digits) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            load.ignore_lines = digits.parse().map_err(|_| self.unexpected())?;
            self.at += 1;
            if !self.take_word("lines") {
                self.expect_word("rows")?;
            }
        }
        if self.take(&TokenKind::LeftParen) {
            let mut targets = Vec::new();
            if !self.take(&TokenKind::RightParen) {
                loop {
                    if let TokenKind::Variable(name) = self.peek_kind() {
                        targets.push(LoadTarget::Variable(name.to_string()));
                        self.at += 1;
                    } else {
                        targets.push(LoadTarget::Column(self.name()?.text));
                    }
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
            }
            load.targets = Some(targets);
        }
        if self.take_word("set") {
            loop {
                let column = self.name()?.text;
                if !self.take_operator("=") {
                    return Err(self.unexpected());
                }
                let start = self.peek().start;
                self.expression()?;
                load.assignments
                    .push((column, self.slice(start, self.previous_end())));
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(Statement::MySql(MySqlStatement::LoadData(Box::new(load))))
    }

    fn word_text_or_string(&self) -> Option<String> {
        match self.peek_kind() {
            TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
            TokenKind::String(text) => Some(text.to_ascii_lowercase()),
            TokenKind::QuotedIdentifier(name) => Some(name.to_ascii_lowercase()),
            _ => None,
        }
    }
}
