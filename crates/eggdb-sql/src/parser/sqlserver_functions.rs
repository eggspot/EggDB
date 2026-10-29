use super::Parser;
use super::sqlserver::not_supported;
use crate::ast::{
    AggregateFunction, DerivedTable, Expr, ExprKind, FromClause, GeneratedTable, Literal, Name,
    Query, Select, SelectItem, TableFunction, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::sqlserver_functions::{DatePart, Function};
use crate::types::{SqlType, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputStatement {
    Insert,
    Update,
    Delete,
}

fn call(function: Function, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::SqlServerFunction {
            function,
            arguments,
        },
        position,
    }
}

fn wrong_argument_count(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("The {name} function requires the right number of arguments."),
    )
    .with_sqlserver_code(174)
    .at(position)
}

fn unknown_date_part(word: &str, function: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("'{word}' is not a recognized {function} option."),
    )
    .with_sqlserver_code(155)
    .at(position)
}

fn not_bound(qualifier: &str, name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!("The multi-part identifier \"{qualifier}.{name}\" could not be bound."),
    )
    .with_sqlserver_code(4104)
    .at(position)
}

pub(super) fn binary_literal(digits: &str, position: usize) -> SqlResult<Expr> {
    let padded = if digits.len() % 2 == 1 {
        format!("0{digits}")
    } else {
        digits.to_owned()
    };
    let bytes = padded
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
        .ok_or_else(|| {
            SqlError::syntax(format!("Incorrect syntax near '0x{digits}'."), position)
        })?;
    Ok(Expr {
        kind: ExprKind::Constant {
            value: Value::Bytea(bytes),
            sql_type: Some(SqlType::Bytea),
        },
        position,
    })
}

pub(super) fn now_with_offset(position: usize) -> Expr {
    Expr {
        kind: ExprKind::Cast {
            expr: Box::new(Expr {
                kind: ExprKind::Function {
                    name: "now".to_owned(),
                    arguments: Vec::new(),
                },
                position,
            }),
            sql_type: SqlType::TimestampTz(Some(6)),
        },
        position,
    }
}

fn unqualified(expr: &mut Expr, statement: OutputStatement) -> SqlResult<()> {
    if let ExprKind::Column { qualifier, name } = &mut expr.kind {
        match qualifier.as_deref() {
            Some("inserted") if statement != OutputStatement::Delete => *qualifier = None,
            Some("deleted") if statement == OutputStatement::Delete => *qualifier = None,
            Some("deleted") => {
                return Err(not_supported("OUTPUT deleted. in UPDATE").at(expr.position));
            }
            Some(other @ "inserted") => return Err(not_bound(other, name, expr.position)),
            _ => {}
        }
        return Ok(());
    }
    for child in crate::eval::children_mut(expr) {
        unqualified(child, statement)?;
    }
    Ok(())
}

impl Parser<'_> {
    pub(super) fn sqlserver_cast(&mut self, position: usize) -> SqlResult<Expr> {
        self.at += 1;
        self.expect(&TokenKind::LeftParen)?;
        let expr = self.expression()?;
        self.expect_word("as")?;
        let target = self.sqlserver_declared_type(true)?;
        self.expect(&TokenKind::RightParen)?;
        Ok(call(
            Function::Convert {
                target,
                style: None,
            },
            vec![expr],
            position,
        ))
    }

    pub(super) fn sqlserver_convert(&mut self, position: usize) -> SqlResult<Expr> {
        self.at += 1;
        self.expect(&TokenKind::LeftParen)?;
        let target = self.sqlserver_declared_type(true)?;
        self.expect(&TokenKind::Comma)?;
        let expr = self.expression()?;
        let style = if self.take(&TokenKind::Comma) {
            let style_position = self.position();
            let TokenKind::Integer(digits) = self.peek_kind() else {
                return Err(
                    not_supported("A CONVERT style other than an integer constant")
                        .at(style_position),
                );
            };
            let style = digits.parse::<u16>().map_err(|_| {
                SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("{digits} is not a valid style number."),
                )
                .with_sqlserver_code(281)
                .at(style_position)
            })?;
            self.at += 1;
            Some(style)
        } else {
            None
        };
        self.expect(&TokenKind::RightParen)?;
        Ok(call(
            Function::Convert { target, style },
            vec![expr],
            position,
        ))
    }

    fn sqlserver_arguments(
        &mut self,
        name: &Name,
        count: std::ops::RangeInclusive<usize>,
    ) -> SqlResult<Vec<Expr>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if !count.contains(&arguments.len()) {
            return Err(wrong_argument_count(&name.text, name.position));
        }
        Ok(arguments)
    }

    fn sqlserver_date_part(&mut self, function: &str) -> SqlResult<DatePart> {
        self.expect(&TokenKind::LeftParen)?;
        let position = self.position();
        let word = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(word) => word.to_ascii_lowercase(),
            _ => return Err(self.unexpected()),
        };
        let part =
            DatePart::named(&word).ok_or_else(|| unknown_date_part(&word, function, position))?;
        self.at += 1;
        self.expect(&TokenKind::Comma)?;
        Ok(part)
    }

    fn sqlserver_date_call(&mut self, name: &Name, function: &str) -> SqlResult<Expr> {
        let part = self.sqlserver_date_part(function)?;
        let first = self.expression()?;
        self.expect(&TokenKind::Comma)?;
        let second = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        let function = if function == "dateadd" {
            Function::DateAdd(part)
        } else {
            Function::DateDiff(part)
        };
        Ok(call(function, vec![first, second], name.position))
    }

    fn caught_error_value(&self, function: &str, position: usize) -> Expr {
        let caught = self.batch.and_then(|batch| batch.caught.as_ref());
        let (value, sql_type) = match function {
            "error_message" => (
                caught.map_or(Value::Null, |error| Value::Text(error.message.clone())),
                SqlType::Varchar(Some(4000)),
            ),
            "error_procedure" => (Value::Null, SqlType::Varchar(Some(128))),
            number => (
                caught.map_or(Value::Null, |error| {
                    Value::Integer(match number {
                        "error_number" => i32::try_from(error.number).unwrap_or(i32::MAX),
                        "error_severity" => i32::from(error.severity),
                        "error_state" => i32::from(error.state),
                        _ => i32::try_from(error.line).unwrap_or(i32::MAX),
                    })
                }),
                SqlType::Integer,
            ),
        };
        Expr {
            kind: ExprKind::Constant {
                value,
                sql_type: Some(sql_type),
            },
            position,
        }
    }

    pub(super) fn sqlserver_call(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let position = name.position;
        if let Some(function) = crate::sqlserver_catalog::MetadataFunction::named(&name.text) {
            let mut arguments = self.sqlserver_arguments(name, function.arguments())?;
            if function.takes_the_database() {
                let database = self.sqlserver_settings(|settings| settings.database.clone());
                arguments.push(Expr {
                    kind: ExprKind::Constant {
                        value: Value::Text(database),
                        sql_type: Some(SqlType::Varchar(Some(128))),
                    },
                    position,
                });
            }
            return Ok(Some(call(
                Function::Metadata(function),
                arguments,
                position,
            )));
        }
        let expr = match name.text.as_str() {
            "error_number" | "error_severity" | "error_state" | "error_line" | "error_message"
            | "error_procedure" => {
                self.sqlserver_arguments(name, 0..=0)?;
                self.caught_error_value(&name.text, position)
            }
            "xact_state" => {
                self.sqlserver_arguments(name, 0..=0)?;
                let open = self.sqlserver_settings(|settings| settings.sqlserver.transaction_count) > 0;
                Expr {
                    kind: ExprKind::Constant {
                        value: Value::SmallInt(i16::from(open)),
                        sql_type: Some(SqlType::SmallInt),
                    },
                    position,
                }
            }
            "suser_sname" | "suser_name" | "original_login" => {
                self.sqlserver_arguments(name, 0..=1)?;
                let login = self.sqlserver_login();
                super::sqlserver::login_text("system_user", login, position)
            }
            "user_name" => {
                self.sqlserver_arguments(name, 0..=1)?;
                super::sqlserver::login_text(
                    "current_user",
                    crate::sqlserver_catalog::DBO.to_owned(),
                    position,
                )
            }
            "isnull" => call(
                Function::IsNull,
                self.sqlserver_arguments(name, 2..=2)?,
                position,
            ),
            "len" => call(
                Function::Len,
                self.sqlserver_arguments(name, 1..=1)?,
                position,
            ),
            "charindex" => call(
                Function::CharIndex,
                self.sqlserver_arguments(name, 2..=3)?,
                position,
            ),
            "newid" | "newsequentialid" => call(
                Function::NewId,
                self.sqlserver_arguments(name, 0..=0)?,
                position,
            ),
            "scope_identity" => call(
                Function::ScopeIdentity,
                self.sqlserver_arguments(name, 0..=0)?,
                position,
            ),
            "sysdatetimeoffset" => {
                self.sqlserver_arguments(name, 0..=0)?;
                now_with_offset(position)
            }
            "dateadd" | "datediff" => self.sqlserver_date_call(name, &name.text.clone())?,
            "count" => {
                let aggregate = self.aggregate(name.clone(), AggregateFunction::Count)?;
                call(
                    Function::AggregateResult(AggregateFunction::Count),
                    vec![aggregate],
                    position,
                )
            }
            _ => return Ok(None),
        };
        Ok(Some(expr))
    }

    pub(super) fn sqlserver_output(
        &mut self,
        statement: OutputStatement,
    ) -> SqlResult<Option<Vec<SelectItem>>> {
        if !self.take_word("output") {
            return Ok(None);
        }
        let mut items = self.select_items()?;
        if self.is_word("into") {
            return Err(not_supported("OUTPUT ... INTO").at(self.position()));
        }
        for item in &mut items {
            match item {
                SelectItem::Wildcard {
                    qualifier,
                    position,
                } => match qualifier.as_deref() {
                    Some("inserted") if statement != OutputStatement::Delete => *qualifier = None,
                    Some("deleted") if statement == OutputStatement::Delete => *qualifier = None,
                    Some("deleted") => {
                        return Err(not_supported("OUTPUT deleted. in UPDATE").at(*position));
                    }
                    Some(other) => return Err(not_bound(other, "*", *position)),
                    None => {}
                },
                SelectItem::Expr { expr, .. } => unqualified(expr, statement)?,
            }
        }
        Ok(Some(items))
    }

    pub(super) fn sqlserver_offset_fetch(
        &mut self,
        ordered: bool,
    ) -> SqlResult<(Option<Expr>, Option<Expr>)> {
        if !self.is_word("offset") {
            return Ok((None, None));
        }
        if !ordered {
            return Err(SqlError::syntax(
                "Incorrect syntax near 'OFFSET'.",
                self.position(),
            ));
        }
        self.at += 1;
        let offset = self.expression()?;
        if !self.take_word("rows") {
            self.expect_word("row")?;
        }
        if !self.take_word("fetch") {
            return Ok((Some(offset), None));
        }
        if !self.take_word("next") {
            self.expect_word("first")?;
        }
        let count = self.expression()?;
        if !self.take_word("rows") {
            self.expect_word("row")?;
        }
        self.expect_word("only")?;
        Ok((Some(offset), Some(count)))
    }

    pub(super) fn sqlserver_assigned_alias(&mut self) -> SqlResult<Option<String>> {
        let named = matches!(self.peek_kind(), TokenKind::Word(word) if !self.reserved(word))
            || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_))
            || matches!(self.peek_kind(), TokenKind::String(_));
        if !named || !matches!(self.peek_kind_at(1), TokenKind::Operator("=")) {
            return Ok(None);
        }
        let alias = match self.peek_kind() {
            TokenKind::String(text) => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                text
            }
            _ => self.label()?,
        };
        self.at += 1;
        Ok(Some(alias))
    }
}

impl Parser<'_> {
    pub(super) fn at_openjson(&self) -> bool {
        self.is_word("openjson") && self.peek_kind_at(1) == &TokenKind::LeftParen
    }

    fn string_constant(&mut self) -> Option<String> {
        let TokenKind::String(text) = self.peek_kind() else {
            return None;
        };
        let text = text.as_ref().to_owned();
        self.at += 1;
        Some(text)
    }

    fn openjson_column_key(&self) -> String {
        let token = self.peek();
        let written = self.text.get(token.start..token.end).unwrap_or("");
        if let Some(inner) = written.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            return inner.replace("]]", "]");
        }
        if let Some(inner) = written.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
            return inner.replace("\"\"", "\"");
        }
        written.to_owned()
    }

    pub(super) fn sqlserver_openjson(&mut self) -> SqlResult<TableReference> {
        let position = self.position();
        self.at += 1;
        self.expect(&TokenKind::LeftParen)?;
        let mut document = self.expression()?;
        if self.take(&TokenKind::Comma) {
            let path_position = self.position();
            let Some(path) = self.string_constant() else {
                return Err(not_supported("OPENJSON with a path that is not a string constant")
                    .at(path_position));
            };
            let steps = crate::json_functions::sqlserver_path_steps(&path)
                .map_err(|error| error.at_if_unplaced(path_position))?;
            document = json_path_call("json_extract_path", document, &steps, path_position);
        }
        self.expect(&TokenKind::RightParen)?;
        if !self.take_word("with") {
            return Err(not_supported("OPENJSON without a WITH clause").at(position));
        }
        self.expect(&TokenKind::LeftParen)?;
        let element = Expr {
            kind: ExprKind::Column {
                qualifier: None,
                name: crate::set_returning::OPENJSON_FUNCTION.to_owned(),
            },
            position,
        };
        let mut items = Vec::new();
        loop {
            let column_position = self.position();
            let key = self.openjson_column_key();
            let name = self.name()?;
            let target = self.sqlserver_declared_type(true)?;
            let path_position = self.position();
            let path = self.string_constant();
            let as_json = self.is_word("as") && self.is_word_at(1, "json");
            if as_json {
                self.at += 2;
            }
            let steps = match &path {
                Some(path) => crate::json_functions::sqlserver_path_steps(path)
                    .map_err(|error| error.at_if_unplaced(path_position))?,
                None => vec![key],
            };
            let extracted = json_path_call(
                if as_json {
                    crate::json_functions::OPENJSON_JSON
                } else {
                    crate::json_functions::OPENJSON_VALUE
                },
                element.clone(),
                &steps,
                column_position,
            );
            items.push(SelectItem::Expr {
                expr: call(
                    Function::Convert {
                        target,
                        style: None,
                    },
                    vec![extracted],
                    column_position,
                ),
                alias: Some(name.text),
            });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        let rows = TableReference {
            name: Name {
                text: crate::set_returning::OPENJSON_FUNCTION.to_owned(),
                position,
            },
            alias: None,
            derived: None,
            generated: Some(Box::new(GeneratedTable::Function(TableFunction {
                name: crate::set_returning::OPENJSON_FUNCTION.to_owned(),
                arguments: vec![document],
                ordinality: false,
                column_names: Vec::new(),
                position,
            }))),
        };
        let select = Select {
            distinct: false,
            items,
            from: Some(FromClause {
                first: rows,
                joins: Vec::new(),
            }),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            source_row_limit: None,
        };
        Ok(TableReference {
            name: Name {
                text: alias.clone().unwrap_or_else(|| "openjson".to_owned()),
                position,
            },
            alias,
            derived: Some(Box::new(DerivedTable {
                query: Query::of_select(select, position),
                column_names: Vec::new(),
                dialect: self.dialect,
            })),
            generated: None,
        })
    }
}

fn json_path_call(function: &str, document: Expr, steps: &[String], position: usize) -> Expr {
    let mut arguments = Vec::with_capacity(steps.len() + 1);
    arguments.push(document);
    arguments.extend(steps.iter().map(|step| Expr {
        kind: ExprKind::Literal(Literal::String(step.clone())),
        position,
    }));
    Expr {
        kind: ExprKind::Function {
            name: function.to_owned(),
            arguments,
        },
        position,
    }
}
