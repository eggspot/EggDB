use super::{Parser, sqlserver};
use crate::ast::{
    Expr, ExprKind, FromClause, Literal, MergeInsert, MergeStatement, MergeUpdate, Name, Select,
    SelectItem, Statement, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

pub(crate) struct MergeRules {
    pub(crate) delete_where: bool,
    pub(crate) where_clauses: bool,
}

impl MergeRules {
    pub(crate) fn oracle() -> Self {
        Self {
            delete_where: true,
            where_clauses: true,
        }
    }

    fn sqlserver() -> Self {
        Self {
            delete_where: false,
            where_clauses: false,
        }
    }
}

fn missing_clause(position: usize) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, "missing keyword")
        .with_oracle_code(905)
        .at(position)
}

impl Parser<'_> {
    fn merge_column(&mut self) -> SqlResult<Name> {
        let first = self.name()?;
        if self.take(&TokenKind::Dot) {
            return self.name();
        }
        Ok(first)
    }

    fn merge_update(&mut self, rules: &MergeRules) -> SqlResult<MergeUpdate> {
        self.expect_word("update")?;
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            let column = self.merge_column()?;
            if !self.take_operator("=") {
                return Err(self.unexpected());
            }
            assignments.push((column, self.expression()?));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        let condition = if rules.where_clauses && self.take_word("where") {
            Some(self.expression()?)
        } else {
            None
        };
        let delete_condition = if rules.delete_where && self.take_word("delete") {
            self.expect_word("where")?;
            Some(self.expression()?)
        } else {
            None
        };
        Ok(MergeUpdate {
            assignments,
            condition,
            delete_condition,
        })
    }

    fn merge_insert_values(&mut self) -> SqlResult<(Option<Vec<Name>>, Vec<Expr>)> {
        self.expect_word("insert")?;
        let columns = if self.take(&TokenKind::LeftParen) {
            let mut columns = Vec::new();
            loop {
                columns.push(self.merge_column()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
            Some(columns)
        } else {
            None
        };
        self.expect_word("values")?;
        self.expect(&TokenKind::LeftParen)?;
        let mut values = Vec::new();
        loop {
            if self.is_word("default") {
                return Err(self.not_supported("DEFAULT in MERGE ... VALUES"));
            }
            values.push(self.expression()?);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok((columns, values))
    }

    fn merge_insert(&mut self) -> SqlResult<MergeInsert> {
        let (columns, values) = self.merge_insert_values()?;
        let condition = if self.take_word("where") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(MergeInsert {
            columns,
            values,
            condition,
        })
    }

    fn merge_source(&mut self) -> SqlResult<(TableReference, Option<String>)> {
        self.expect_word("using")?;
        let source = self.table_reference()?;
        let source_label = source.alias.clone().or_else(|| {
            source
                .derived
                .is_none()
                .then(|| crate::schemas::base_name(&source.name.text).to_owned())
        });
        Ok((source, source_label))
    }

    pub(super) fn merge(&mut self, rules: &MergeRules) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        self.expect_word("into")?;
        let name = self.table_name()?;
        let alias = if !self.is_word("using") && (self.take_word("as") || self.at_bare_alias()) {
            Some(self.name()?.text)
        } else {
            None
        };
        let target = TableReference::named(name, alias);
        let (source, source_label) = self.merge_source()?;
        self.expect_word("on")?;
        self.expect(&TokenKind::LeftParen)?;
        let on = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        let mut update = None;
        let mut insert = None;
        while self.take_word("when") {
            let matched = !self.take_word("not");
            self.expect_word("matched")?;
            self.expect_word("then")?;
            if matched && update.is_none() {
                update = Some(self.merge_update(rules)?);
            } else if !matched && insert.is_none() {
                insert = Some(self.merge_insert()?);
            } else {
                return Err(self.unexpected());
            }
        }
        if update.is_none() && insert.is_none() {
            return Err(missing_clause(self.position()));
        }
        Ok(Statement::Merge(Box::new(MergeStatement {
            target,
            source: every_column_of(source, position),
            source_label,
            on,
            update,
            insert,
            output: None,
        })))
    }

    pub(super) fn sqlserver_merge(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        if self.is_word("top") {
            return Err(sqlserver::not_supported("TOP in MERGE").at(self.position()));
        }
        self.take_word("into");
        let name = self.table_name()?;
        if self.is_word("with") {
            return Err(sqlserver::not_supported("table hints in MERGE").at(self.position()));
        }
        let alias = if !self.is_word("using") && (self.take_word("as") || self.at_bare_alias()) {
            Some(self.name()?.text)
        } else {
            None
        };
        let target = TableReference::named(name, alias);
        let (source, source_label) = self.merge_source()?;
        self.expect_word("on")?;
        let on = self.expression()?;
        let mut update = None;
        let mut insert = None;
        while self.take_word("when") {
            let matched = !self.take_word("not");
            self.expect_word("matched")?;
            if !matched && self.take_word("by") {
                if self.is_word("source") {
                    return Err(
                        sqlserver::not_supported("WHEN NOT MATCHED BY SOURCE").at(self.position())
                    );
                }
                self.expect_word("target")?;
            }
            let condition = if self.take_word("and") {
                Some(self.expression()?)
            } else {
                None
            };
            self.expect_word("then")?;
            if matched && update.is_none() {
                update = Some(self.sqlserver_merge_matched(condition)?);
            } else if !matched && insert.is_none() {
                let (columns, values) = self.merge_insert_values()?;
                insert = Some(MergeInsert {
                    columns,
                    values,
                    condition,
                });
            } else {
                return Err(sqlserver::not_supported("more than one WHEN MATCHED clause in MERGE")
                    .at(self.position()));
            }
        }
        if update.is_none() && insert.is_none() {
            return Err(SqlError::syntax(
                "A MERGE statement must have at least one WHEN clause.",
                self.position(),
            )
            .with_sqlserver_code(10713));
        }
        let output = if self.take_word("output") {
            let items = self.select_items()?;
            if self.is_word("into") {
                return Err(sqlserver::not_supported("OUTPUT ... INTO").at(self.position()));
            }
            Some(items)
        } else {
            None
        };
        Ok(Statement::Merge(Box::new(MergeStatement {
            target,
            source: every_column_of(source, position),
            source_label,
            on,
            update,
            insert,
            output,
        })))
    }

    fn sqlserver_merge_matched(&mut self, condition: Option<Expr>) -> SqlResult<MergeUpdate> {
        if self.is_word("delete") {
            let position = self.position();
            self.at += 1;
            return Ok(MergeUpdate {
                assignments: Vec::new(),
                condition,
                delete_condition: Some(Expr {
                    kind: ExprKind::Literal(Literal::Boolean(true)),
                    position,
                }),
            });
        }
        let mut update = self.merge_update(&MergeRules::sqlserver())?;
        update.condition = condition;
        Ok(update)
    }
}

fn every_column_of(source: TableReference, position: usize) -> Select {
    Select {
        distinct: false,
        items: vec![SelectItem::Wildcard {
            qualifier: None,
            position,
        }],
        from: Some(FromClause {
            first: source,
            joins: Vec::new(),
        }),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}
