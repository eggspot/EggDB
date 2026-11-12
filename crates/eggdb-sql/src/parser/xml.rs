use super::Parser;
use crate::ast::{AggregateFunction, Expr, ExprKind, Literal, Name};
use crate::error::{SqlError, SqlResult};
use crate::lexer::TokenKind;
use crate::xml_functions as xml;

fn call(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

fn text(value: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::String(value)),
        position,
    }
}

fn content(expr: Expr) -> Expr {
    if let ExprKind::Function { name, .. } = &expr.kind
        && xml::produces_xml(name)
    {
        return expr;
    }
    let position = expr.position;
    call(xml::ESCAPE, vec![expr], position)
}

impl Parser<'_> {
    fn xml_name(&mut self) -> SqlResult<String> {
        let quoted = matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_));
        let name = self.name()?.text;
        Ok(if self.oracle() && !quoted {
            name.to_ascii_uppercase()
        } else {
            name
        })
    }

    fn named_value(&mut self) -> SqlResult<(String, Expr)> {
        let position = self.position();
        let value = self.expression()?;
        if self.take_word("as") {
            return Ok((self.xml_name()?, value));
        }
        match &value.kind {
            ExprKind::Column { name, .. } => Ok((
                if self.oracle() {
                    name.to_ascii_uppercase()
                } else {
                    name.clone()
                },
                value,
            )),
            _ => Err(SqlError::syntax(
                "unnamed XML attribute value must be a column reference",
                position,
            )),
        }
    }

    /// `XMLELEMENT`, `XMLFOREST`, `XMLCONCAT`, `XMLAGG`, `XMLCOMMENT`, `XMLPI`, `XMLSERIALIZE`,
    /// `XMLPARSE`, `XMLQUERY` and `XMLEXISTS` after their name, as PostgreSQL and Oracle write them;
    /// `None` for another name.
    pub(super) fn xml_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        if !(self.postgresql() || self.oracle()) || self.peek_kind() != &TokenKind::LeftParen {
            return Ok(None);
        }
        let position = name.position;
        let expr = match name.text.as_str() {
            "xmlelement" => {
                self.at += 1;
                if !self.take_word("name") && !self.oracle() {
                    return Err(self.unexpected());
                }
                let tag = self.xml_name()?;
                let mut attributes: Vec<(String, Expr)> = Vec::new();
                let mut contents = Vec::new();
                while self.take(&TokenKind::Comma) {
                    if self.is_word("xmlattributes") && self.peek_kind_at(1) == &TokenKind::LeftParen {
                        self.at += 2;
                        loop {
                            attributes.push(self.named_value()?);
                            if !self.take(&TokenKind::Comma) {
                                break;
                            }
                        }
                        self.expect(&TokenKind::RightParen)?;
                    } else {
                        contents.push(content(self.expression()?));
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                let mut arguments = vec![
                    text(tag, position),
                    Expr {
                        kind: ExprKind::Literal(Literal::Integer(attributes.len() as i64)),
                        position,
                    },
                ];
                for (attribute, value) in attributes {
                    arguments.push(text(attribute, position));
                    arguments.push(value);
                }
                arguments.extend(contents);
                call(xml::ELEMENT, arguments, position)
            }
            "xmlforest" => {
                self.at += 1;
                let mut arguments = Vec::new();
                loop {
                    let (tag, value) = self.named_value()?;
                    arguments.push(text(tag, position));
                    arguments.push(content(value));
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                call(xml::FOREST, arguments, position)
            }
            "xmlconcat" => {
                self.at += 1;
                let mut arguments = Vec::new();
                loop {
                    arguments.push(self.expression()?);
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                call(xml::CONCAT, arguments, position)
            }
            "xmlagg" => {
                self.at += 1;
                let value = self.expression()?;
                let order = if self.is_word("order") && self.is_word_at(1, "by") {
                    self.at += 2;
                    self.order_items()?
                } else {
                    Vec::new()
                };
                self.expect(&TokenKind::RightParen)?;
                let joined = call(
                    "string_agg_arguments",
                    vec![value, text(String::new(), position)],
                    position,
                );
                let argument = if order.is_empty() {
                    joined
                } else {
                    crate::aggregate::ordered_arguments(joined, order)
                };
                call(
                    xml::FRAGMENT,
                    vec![Expr {
                        kind: ExprKind::Aggregate {
                            function: AggregateFunction::StringAgg,
                            argument: Some(Box::new(argument)),
                            distinct: false,
                        },
                        position,
                    }],
                    position,
                )
            }
            "xmlcomment" => {
                self.at += 1;
                let value = self.expression()?;
                self.expect(&TokenKind::RightParen)?;
                call(xml::COMMENT, vec![value], position)
            }
            "xmlpi" => {
                self.at += 1;
                if !self.take_word("name") && !self.oracle() {
                    return Err(self.unexpected());
                }
                let target = self.xml_name()?;
                let mut arguments = vec![text(target, position)];
                if self.take(&TokenKind::Comma) {
                    arguments.push(self.expression()?);
                }
                self.expect(&TokenKind::RightParen)?;
                call(xml::PI, arguments, position)
            }
            "xmlserialize" => {
                self.at += 1;
                if !self.take_word("content") {
                    self.take_word("document");
                }
                let value = self.expression()?;
                let sql_type = if self.take_word("as") {
                    Some(self.cast_type()?)
                } else {
                    None
                };
                loop {
                    if self.take_word("indent") {
                        if self.take_word("size") {
                            let _ = self.take_operator("=");
                            self.at += 1;
                        }
                    } else if self.is_word("no") && self.is_word_at(1, "indent") {
                        self.at += 2;
                    } else {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                match sql_type {
                    Some(sql_type) => Expr {
                        kind: ExprKind::Cast {
                            expr: Box::new(value),
                            sql_type,
                        },
                        position,
                    },
                    None => value,
                }
            }
            "xmlparse" => {
                self.at += 1;
                if !self.take_word("content") {
                    self.take_word("document");
                }
                let value = self.expression()?;
                self.take_word("wellformed");
                self.expect(&TokenKind::RightParen)?;
                call(xml::FRAGMENT, vec![value], position)
            }
            "xmlquery" | "xmlexists" => {
                self.at += 1;
                let TokenKind::String(path) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                let path = text(path.as_ref().to_owned(), position);
                self.at += 1;
                self.expect_word("passing")?;
                self.skip_xml_passing_mechanism();
                let document = self.expression()?;
                self.skip_xml_passing_mechanism();
                let mut null_on_empty = false;
                if name.text == "xmlquery" {
                    self.expect_word("returning")?;
                    if !self.take_word("content") {
                        self.expect_word("sequence")?;
                    }
                    if self.take_word("null") {
                        self.expect_word("on")?;
                        self.expect_word("empty")?;
                        null_on_empty = true;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                if name.text == "xmlexists" {
                    call("xpath_exists", vec![path, document], position)
                } else {
                    let flag = Expr {
                        kind: ExprKind::Literal(Literal::Boolean(null_on_empty)),
                        position,
                    };
                    call(crate::xml_path::QUERY, vec![document, path, flag], position)
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(expr))
    }

    pub(super) fn skip_xml_passing_mechanism(&mut self) {
        if self.is_word("by") && (self.is_word_at(1, "ref") || self.is_word_at(1, "value")) {
            self.at += 2;
        }
    }
}
