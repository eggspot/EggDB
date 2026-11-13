use super::Parser;
use crate::ast::{
    DerivedTable, Expr, FromClause, InsertBranch, InsertTarget, InsertValue, MultitableInsert,
    MultitableInsertKind, Name, Query, Select, SelectItem, Statement, TableReference, TargetValue,
};
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::lexer::TokenKind;

const SOURCE_LABEL: &str = "unnamed_subquery";

fn source_select(computed: Vec<Expr>, query: Query, position: usize) -> Select {
    let mut items: Vec<SelectItem> = computed
        .into_iter()
        .map(|expr| SelectItem::Expr { expr, alias: None })
        .collect();
    items.push(SelectItem::Wildcard {
        qualifier: None,
        position,
    });
    Select {
        distinct: false,
        items,
        from: Some(FromClause {
            first: TableReference {
                name: Name {
                    text: SOURCE_LABEL.to_owned(),
                    position,
                },
                alias: None,
                derived: Some(Box::new(DerivedTable {
                    query,
                    column_names: Vec::new(),
                    dialect: Dialect::Oracle,
                    lateral: false,
                    hierarchy: None,
                })),
                generated: None,
                partition_selection: None,
                system_time: None,
            },
            joins: Vec::new(),
        }),
        filter: None,
        group_by: Vec::new(),
        grouping_sets: None,
        distinct_on: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
        locking: None,
    }
}

impl Parser<'_> {
    pub(super) fn multitable_insert(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        let kind = if self.take_word("first") {
            MultitableInsertKind::First
        } else {
            self.expect_word("all")?;
            MultitableInsertKind::All
        };
        let mut computed = Vec::new();
        let mut branches = Vec::new();
        let mut otherwise = Vec::new();
        if self.is_word("when") {
            while self.take_word("when") {
                let condition = self.expression()?;
                computed.push(self.condition(condition));
                let condition = Some(computed.len() - 1);
                self.expect_word("then")?;
                let targets = self.insert_targets(&mut computed)?;
                branches.push(InsertBranch { condition, targets });
            }
            if self.take_word("else") {
                otherwise = self.insert_targets(&mut computed)?;
            }
        } else if kind == MultitableInsertKind::First {
            return Err(self.unexpected());
        } else {
            let targets = self.insert_targets(&mut computed)?;
            branches.push(InsertBranch {
                condition: None,
                targets,
            });
        }
        if !(self.is_word("select")
            || self.is_word("with")
            || self.peek_kind() == &TokenKind::LeftParen)
        {
            return Err(self.unexpected());
        }
        let query = self.insert_query()?;
        let width = computed.len();
        Ok(Statement::MultitableInsert(Box::new(MultitableInsert {
            kind,
            source: Query::of_select(source_select(computed, query, position), position),
            computed: width,
            branches,
            otherwise,
        })))
    }

    fn insert_targets(&mut self, computed: &mut Vec<Expr>) -> SqlResult<Vec<InsertTarget>> {
        let mut targets = Vec::new();
        while self.take_word("into") {
            let name = self.table_name()?;
            let alias = if !self.is_word("when") && self.at_bare_alias() {
                Some(self.name()?.text)
            } else {
                None
            };
            let columns = if self.peek_kind() == &TokenKind::LeftParen
                && !self.is_word_at(1, "select")
                && !self.is_word_at(1, "with")
            {
                Some(self.name_list()?)
            } else {
                None
            };
            let values = if self.take_word("values") {
                self.expect(&TokenKind::LeftParen)?;
                let mut values = Vec::new();
                loop {
                    values.push(match self.insert_value()? {
                        InsertValue::Default => TargetValue::Default,
                        InsertValue::Expr(expr) => {
                            computed.push(expr);
                            TargetValue::Computed(computed.len() - 1)
                        }
                    });
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                Some(values)
            } else {
                None
            };
            targets.push(InsertTarget {
                table: TableReference::named(name, alias),
                columns,
                values,
            });
        }
        if targets.is_empty() {
            return Err(self.unexpected());
        }
        Ok(targets)
    }
}
