use super::Parser;
use crate::ast::{
    FromClause, MergeInsert, MergeStatement, MergeUpdate, Name, Select, SelectItem, Statement,
    TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

/// The options a dialect's `MERGE` has: whether `DELETE WHERE` may follow the update.
pub(crate) struct MergeRules {
    pub(crate) delete_where: bool,
}

impl MergeRules {
    /// Oracle's `MERGE`: the update may end with `DELETE WHERE`.
    pub(crate) fn oracle() -> Self {
        Self { delete_where: true }
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
        let condition = if self.take_word("where") {
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

    fn merge_insert(&mut self) -> SqlResult<MergeInsert> {
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

    /// `MERGE INTO target [alias] USING {table | (subquery)} [alias] ON (condition)` with a
    /// `WHEN MATCHED THEN UPDATE` clause, a `WHEN NOT MATCHED THEN INSERT` clause, or both, in
    /// either order.
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
        self.expect_word("using")?;
        let source = self.table_reference()?;
        let source_label = source
            .alias
            .clone()
            .or_else(|| source.derived.is_none().then(|| source.name.text.clone()));
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
            source: Select {
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
            },
            source_label,
            on,
            update,
            insert,
        })))
    }
}
