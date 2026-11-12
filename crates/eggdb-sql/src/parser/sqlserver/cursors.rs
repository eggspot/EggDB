use super::super::Parser;
use super::not_supported;
use crate::ast::{
    CursorDeclaration, CursorName, Expr, ExprKind, FetchOrientation, Literal, Name,
    SqlServerStatement, Statement,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

const CURSOR_SCOPES: [&str; 2] = ["local", "global"];
const CURSOR_KINDS: [&str; 4] = ["static", "keyset", "dynamic", "fast_forward"];
const CURSOR_CONCURRENCY: [&str; 3] = ["read_only", "scroll_locks", "optimistic"];

fn conflicting_options(first: &str, second: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("Conflicting cursor options {first} and {second}."),
    )
    .with_sqlserver_code(1048)
    .at(position)
}

impl Parser<'_> {
    pub(in crate::parser) fn at_cursor_declaration(&self) -> bool {
        !self.at_variable(1)
            && matches!(
                self.peek_kind_at(1),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            )
            && (self.is_word_at(2, "cursor")
                || self.is_word_at(2, "insensitive")
                || self.is_word_at(2, "scroll"))
    }

    pub(in crate::parser) fn tsql_declare_cursor(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let name = self.name()?;
        self.tsql_cursor_definition(name, false)
    }

    /// `SET @c = CURSOR [options] FOR query`: a cursor variable, kept as a local cursor named by
    /// the variable.
    pub(in crate::parser) fn tsql_set_cursor_variable(&mut self) -> SqlResult<Statement> {
        let name = self.variable_name()?;
        if !self.take_operator("=") {
            return Err(self.unexpected());
        }
        self.tsql_cursor_definition(name, true)
    }

    fn tsql_cursor_definition(&mut self, name: Name, variable: bool) -> SqlResult<Statement> {
        let insensitive = !variable && self.take_word("insensitive");
        let iso_scroll = !variable && self.take_word("scroll");
        self.expect_word("cursor")?;
        let mut global = !variable;
        let mut forward_only = false;
        let mut scroll = iso_scroll;
        let mut kind: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();
        while let TokenKind::Word(word) = self.peek_kind() {
            let word = word.as_ref();
            if word == "for" {
                break;
            }
            let position = self.position();
            let option = word.to_owned();
            let shown = option.to_uppercase();
            if seen.contains(&option) || insensitive || iso_scroll {
                return Err(self.unexpected());
            }
            if CURSOR_SCOPES.contains(&word) && variable {
                return Err(self.unexpected());
            }
            if CURSOR_SCOPES.contains(&word) {
                if let Some(other) = seen
                    .iter()
                    .find(|seen| CURSOR_SCOPES.contains(&seen.as_str()))
                {
                    return Err(conflicting_options(&other.to_uppercase(), &shown, position));
                }
                global = word == "global";
            } else if word == "forward_only" || word == "scroll" {
                if forward_only || scroll {
                    return Err(conflicting_options("FORWARD_ONLY", "SCROLL", position));
                }
                forward_only = word == "forward_only";
                scroll = word == "scroll";
            } else if CURSOR_KINDS.contains(&word) {
                if let Some(other) = &kind {
                    return Err(conflicting_options(&other.to_uppercase(), &shown, position));
                }
                kind = Some(option.clone());
            } else if !CURSOR_CONCURRENCY.contains(&word) && word != "type_warning" {
                return Err(self.unexpected());
            }
            seen.push(option);
            self.at += 1;
        }
        let fast_forward = kind.as_deref() == Some("fast_forward");
        if fast_forward && scroll {
            return Err(conflicting_options(
                "FAST_FORWARD",
                "SCROLL",
                self.position(),
            ));
        }
        let scrolls = scroll
            || insensitive
            || (!forward_only && matches!(kind.as_deref(), Some("static" | "keyset" | "dynamic")));
        self.expect_word("for")?;
        let query_position = self.position();
        let query = self.query_statement()?;
        if !matches!(query, Statement::Select(_) | Statement::Query(_)) {
            return Err(self.unexpected());
        }
        if self.take_word("for") {
            if self.take_word("read") {
                self.expect_word("only")?;
            } else {
                return Err(not_supported("FOR UPDATE in DECLARE CURSOR").at(query_position));
            }
        }
        Ok(Statement::SqlServer(SqlServerStatement::DeclareCursor(
            CursorDeclaration {
                name,
                global,
                scroll: scrolls,
                dynamic: matches!(kind.as_deref(), Some("dynamic" | "fast_forward")),
                query: Box::new(query),
            },
        )))
    }

    fn tsql_cursor_name(&mut self) -> SqlResult<CursorName> {
        let global = self.is_word("global")
            && matches!(
                self.peek_kind_at(1),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            );
        if global {
            self.at += 1;
        }
        if self.at_variable(0) && !global {
            return Ok(CursorName {
                name: self.variable_name()?,
                global: false,
            });
        }
        Ok(CursorName {
            name: self.name()?,
            global,
        })
    }

    pub(in crate::parser) fn tsql_open_cursor(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        Ok(Statement::SqlServer(SqlServerStatement::OpenCursor(
            self.tsql_cursor_name()?,
        )))
    }

    pub(in crate::parser) fn tsql_close_cursor(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        Ok(Statement::SqlServer(SqlServerStatement::CloseCursor(
            self.tsql_cursor_name()?,
        )))
    }

    pub(in crate::parser) fn tsql_deallocate_cursor(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        Ok(Statement::SqlServer(SqlServerStatement::DeallocateCursor(
            self.tsql_cursor_name()?,
        )))
    }

    fn fetch_offset(&mut self) -> SqlResult<Expr> {
        let position = self.position();
        if self.at_variable(0) {
            let variable = self.variable_name()?;
            return self.variable_reference(&variable.text, variable.position);
        }
        let negative = self.take_operator("-");
        if !negative {
            self.take_operator("+");
        }
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let Ok(magnitude) = digits.parse::<i64>() else {
            return Err(self.unexpected());
        };
        self.at += 1;
        Ok(Expr {
            kind: ExprKind::Literal(Literal::Integer(if negative {
                -magnitude
            } else {
                magnitude
            })),
            position,
        })
    }

    pub(in crate::parser) fn tsql_fetch(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let orientation = if self.take_word("next") {
            Some(FetchOrientation::Next)
        } else if self.take_word("prior") {
            Some(FetchOrientation::Prior)
        } else if self.take_word("first") {
            Some(FetchOrientation::First)
        } else if self.take_word("last") {
            Some(FetchOrientation::Last)
        } else if self.take_word("absolute") {
            Some(FetchOrientation::Absolute(self.fetch_offset()?))
        } else if self.take_word("relative") {
            Some(FetchOrientation::Relative(self.fetch_offset()?))
        } else {
            None
        };
        if orientation.is_some() {
            self.expect_word("from")?;
        } else {
            self.take_word("from");
        }
        let cursor = self.tsql_cursor_name()?;
        let mut into: Vec<Name> = Vec::new();
        if self.take_word("into") {
            loop {
                into.push(self.variable_name()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(Statement::SqlServer(SqlServerStatement::FetchCursor {
            cursor,
            orientation: orientation.unwrap_or(FetchOrientation::Next),
            into,
        }))
    }

    pub(in crate::parser) fn tsql_waitfor(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        let until_time = if self.take_word("time") {
            true
        } else if self.take_word("delay") {
            false
        } else {
            return Err(
                not_supported("WAITFOR RECEIVE and WAITFOR GET CONVERSATION GROUP").at(position),
            );
        };
        let time = self.expression()?;
        if self.take(&TokenKind::Comma) || self.is_word("timeout") {
            return Err(not_supported("WAITFOR ... TIMEOUT").at(position));
        }
        Ok(Statement::SqlServer(SqlServerStatement::WaitFor {
            until_time,
            time,
        }))
    }
}
