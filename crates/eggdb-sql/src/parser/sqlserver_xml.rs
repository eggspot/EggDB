use super::Parser;
use crate::ast::{Expr, ExprKind, GeneratedTable, Literal, Name, TableFunction, TableReference};
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
            "query" => call(crate::xml_path::NODE_QUERY, vec![target, path], position),
            _ => call(crate::xml_path::NODE_EXIST, vec![target, path], position),
        };
        self.expect(&TokenKind::RightParen)?;
        Ok(expr)
    }

    /// Whether a `CROSS APPLY` or `OUTER APPLY` item is an `xml` column's `nodes()` call:
    /// `column.nodes(` or `table.column.nodes(`.
    pub(super) fn at_xml_nodes(&self) -> bool {
        let word = |ahead: usize| matches!(self.peek_kind_at(ahead), TokenKind::Word(_) | TokenKind::QuotedIdentifier(_));
        let nodes_at = |ahead: usize| {
            self.is_word_at(ahead, "nodes") && self.peek_kind_at(ahead + 1) == &TokenKind::LeftParen
        };
        word(0)
            && self.peek_kind_at(1) == &TokenKind::Dot
            && (nodes_at(2) || (word(2) && self.peek_kind_at(3) == &TokenKind::Dot && nodes_at(4)))
    }

    /// `column.nodes('path') AS t(c)`: one row per node the path selects, with the node in
    /// column `c` for its `value()`, `query()` and `exist()`.
    pub(super) fn sqlserver_xml_nodes(&mut self) -> SqlResult<TableReference> {
        let position = self.position();
        let first = self.name()?;
        self.expect(&TokenKind::Dot)?;
        let second = self.name()?;
        let target = if second.text.eq_ignore_ascii_case("nodes") {
            ExprKind::Column { qualifier: None, name: first.text }
        } else {
            self.expect(&TokenKind::Dot)?;
            self.expect_word("nodes")?;
            ExprKind::Column { qualifier: Some(first.text), name: second.text }
        };
        self.expect(&TokenKind::LeftParen)?;
        let path = self.quoted_argument()?;
        self.expect(&TokenKind::RightParen)?;
        self.take_word("as");
        let alias = self.name()?;
        self.expect(&TokenKind::LeftParen)?;
        let column = self.name()?;
        self.expect(&TokenKind::RightParen)?;
        let arguments = vec![
            Expr { kind: target, position },
            string(path, position),
            string("X".to_owned(), position),
            Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(Expr { kind: ExprKind::Literal(Literal::Null), position }),
                    sql_type: crate::types::SqlType::Text,
                },
                position,
            },
            Expr { kind: ExprKind::Literal(Literal::Null), position },
        ];
        Ok(TableReference {
            name: Name { text: alias.text.clone(), position },
            alias: Some(alias.text),
            derived: None,
            generated: Some(Box::new(GeneratedTable::Function(TableFunction {
                name: crate::xml_path::TABLE.to_owned(),
                arguments,
                ordinality: false,
                column_names: vec![column],
                position,
            }))),
            partition_selection: None,
        })
    }
}
