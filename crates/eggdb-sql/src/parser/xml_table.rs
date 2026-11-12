use super::Parser;
use crate::ast::{Expr, ExprKind, GeneratedTable, Literal, Name, TableFunction, TableReference};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::types::SqlType;

fn literal(value: Literal, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(value),
        position,
    }
}

fn typed_null(sql_type: SqlType, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Cast {
            expr: Box::new(literal(Literal::Null, position)),
            sql_type,
        },
        position,
    }
}

impl Parser<'_> {
    pub(super) fn at_xml_table(&self) -> bool {
        self.is_word("xmltable") && self.peek_kind_at(1) == &TokenKind::LeftParen
    }

    fn string_literal(&mut self) -> SqlResult<String> {
        let TokenKind::String(text) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let text = text.as_ref().to_owned();
        self.at += 1;
        Ok(text)
    }

    pub(super) fn xml_table(&mut self) -> SqlResult<TableReference> {
        let position = self.position();
        self.at += 2;
        if self.is_word("xmlnamespaces") {
            return Err(self.not_supported("XMLNAMESPACES in XMLTABLE"));
        }
        let row_path = self.string_literal()?;
        self.expect_word("passing")?;
        self.skip_xml_passing_mechanism();
        let document = self.expression()?;
        self.skip_xml_passing_mechanism();
        let mut names = Vec::new();
        let mut types = Vec::new();
        let mut plan = Vec::new();
        let mut defaults = Vec::new();
        if self.take_word("columns") {
            loop {
                let name = self.name()?;
                if self.take_word("for") {
                    self.expect_word("ordinality")?;
                    plan.push("O".to_owned());
                    types.push(SqlType::BigInt);
                    defaults.push(literal(Literal::Null, position));
                } else {
                    let sql_type = self.sql_type()?;
                    let mut path = name.text.clone();
                    let mut default = literal(Literal::Null, position);
                    loop {
                        if self.take_word("path") {
                            path = self.string_literal()?;
                        } else if self.take_word("default") {
                            default = self.expression()?;
                        } else if self.take_word("not") {
                            self.expect_word("null")?;
                        } else if !self.take_word("null") {
                            break;
                        }
                    }
                    plan.push(format!("P{path}"));
                    types.push(sql_type);
                    defaults.push(default);
                }
                names.push(name);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        } else {
            names.push(Name {
                text: "column_value".to_owned(),
                position,
            });
            plan.push("X".to_owned());
            types.push(SqlType::Text);
            defaults.push(literal(Literal::Null, position));
        }
        self.expect(&TokenKind::RightParen)?;
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        let mut arguments = vec![
            document,
            literal(Literal::String(row_path), position),
            literal(Literal::String(plan.join(crate::xml_path::PLAN_SEPARATOR)), position),
        ];
        arguments.extend(types.into_iter().map(|sql_type| typed_null(sql_type, position)));
        arguments.extend(defaults);
        Ok(TableReference {
            name: Name {
                text: alias.clone().unwrap_or_else(|| "xmltable".to_owned()),
                position,
            },
            alias,
            derived: None,
            generated: Some(Box::new(GeneratedTable::Function(TableFunction {
                name: crate::xml_path::TABLE.to_owned(),
                arguments,
                ordinality: false,
                column_names: names,
                position,
            }))),
            partition_selection: None,
        })
    }
}
