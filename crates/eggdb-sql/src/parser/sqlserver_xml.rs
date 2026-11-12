use super::Parser;
use crate::ast::{Expr, ExprKind, Literal, Name};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};
use crate::lexer::{TokenKind, tokenize_in};

const XML_METHODS: [&str; 3] = ["value", "query", "exist"];

fn call(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

fn string(text: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::String(text)),
        position,
    }
}

/// Whether `name` is one of the `xml` type's methods.
pub(super) fn is_xml_method(name: &str) -> bool {
    XML_METHODS.iter().any(|method| name.eq_ignore_ascii_case(method))
}

impl Parser<'_> {
    /// Whether the word `ahead` tokens on is one of the `xml` type's methods (`value`, `query`,
    /// `exist`) and is called.
    pub(super) fn at_xml_method(&self, ahead: usize) -> bool {
        XML_METHODS.iter().any(|method| self.is_word_at(ahead, method))
            && self.peek_kind_at(ahead + 1) == &TokenKind::LeftParen
    }

    fn quoted_argument(&mut self) -> SqlResult<String> {
        let TokenKind::String(text) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let text = text.as_ref().to_owned();
        self.at += 1;
        Ok(text)
    }

    /// An `xml` method call on `target` (`column.value('(/a/@id)[1]', 'int')`, `.query(...)`,
    /// `.exist(...)`), with the method name read already and its `(` next.
    pub(super) fn sqlserver_xml_method(&mut self, target: Expr, method: &Name) -> SqlResult<Expr> {
        let position = method.position;
        self.expect(&TokenKind::LeftParen)?;
        let path = string(self.quoted_argument()?, position);
        let expr = match method.text.to_ascii_lowercase().as_str() {
            "value" => {
                self.expect(&TokenKind::Comma)?;
                let type_text = self.quoted_argument()?;
                let tokens = tokenize_in(&type_text, Dialect::SqlServer)?;
                let mut type_parser = Parser::new(&type_text, &tokens).in_dialect(Dialect::SqlServer);
                let sql_type = type_parser.sql_type()?;
                if !type_parser.at_end() {
                    return Err(SqlError::syntax(format!("{type_text} is not a type"), position));
                }
                Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(call(crate::xml_path::VALUE, vec![target, path], position)),
                        sql_type,
                    },
                    position,
                }
            }
            "query" => {
                let empty_is_text = Expr {
                    kind: ExprKind::Literal(Literal::Boolean(false)),
                    position,
                };
                call(crate::xml_path::QUERY, vec![target, path, empty_is_text], position)
            }
            _ => call("existsnode", vec![target, path], position),
        };
        self.expect(&TokenKind::RightParen)?;
        Ok(expr)
    }
}
