use super::{Parser, sqlserver, sqlserver_functions};
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
        let mut matched = Vec::new();
        let mut insert = None;
        while self.take_word("when") {
            let when_matched = !self.take_word("not");
            self.expect_word("matched")?;
            self.expect_word("then")?;
            if when_matched && matched.is_empty() {
                matched.push(self.merge_update(rules)?);
            } else if !when_matched && insert.is_none() {
                insert = Some(self.merge_insert()?);
            } else {
                return Err(self.unexpected());
            }
        }
        if matched.is_empty() && insert.is_none() {
            return Err(missing_clause(self.position()));
        }
        Ok(Statement::Merge(Box::new(MergeStatement {
            target,
            source: every_column_of(source, position),
            source_label,
            on,
            matched,
            insert,
            by_source: Vec::new(),
            output: None,
            top: None,
        })))
    }

    pub(super) fn sqlserver_merge(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        let top = self.sqlserver_merge_top()?;
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
        let mut matched: Vec<MergeUpdate> = Vec::new();
        let mut insert = None;
        let mut by_source: Vec<MergeUpdate> = Vec::new();
        while self.take_word("when") {
            let clause_position = self.position();
            let when_matched = !self.take_word("not");
            self.expect_word("matched")?;
            let mut source_side = false;
            if !when_matched && self.take_word("by") {
                source_side = self.take_word("source");
                if !source_side {
                    self.expect_word("target")?;
                }
            }
            let condition = if self.take_word("and") {
                Some(self.expression()?)
            } else {
                None
            };
            self.expect_word("then")?;
            if source_side {
                let clause = self.sqlserver_merge_matched(condition)?;
                check_second_clause(
                    "WHEN NOT MATCHED BY SOURCE",
                    &by_source,
                    &clause,
                    clause_position,
                )?;
                by_source.push(clause);
            } else if when_matched {
                let clause = self.sqlserver_merge_matched(condition)?;
                check_second_clause("WHEN MATCHED", &matched, &clause, clause_position)?;
                matched.push(clause);
            } else if insert.is_none() {
                let (columns, values) = self.merge_insert_values()?;
                insert = Some(MergeInsert {
                    columns,
                    values,
                    condition,
                });
            } else {
                return Err(second_insert_clause(clause_position));
            }
        }
        if matched.is_empty() && insert.is_none() && by_source.is_empty() {
            return Err(SqlError::syntax(
                "A MERGE statement must have at least one WHEN clause.",
                self.position(),
            )
            .with_sqlserver_code(10713));
        }
        let (output, into) = match self.sqlserver_output_items()? {
            Some(clause) => (Some(clause.items), clause.into),
            None => (None, None),
        };
        let statement = Statement::Merge(Box::new(MergeStatement {
            target,
            source: every_column_of(source, position),
            source_label,
            on,
            matched,
            insert,
            by_source,
            output,
            top,
        }));
        Ok(sqlserver_functions::with_output_into(statement, into))
    }

    pub(super) fn postgresql_merge(&mut self) -> SqlResult<Statement> {
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
        let on = self.expression()?;
        let mut matched: Vec<MergeUpdate> = Vec::new();
        let mut insert = None;
        let mut by_source: Vec<MergeUpdate> = Vec::new();
        let mut nothing_after = [false; 3];
        while self.take_word("when") {
            let clause_position = self.position();
            let when_matched = !self.take_word("not");
            self.expect_word("matched")?;
            let mut source_side = false;
            if !when_matched && self.take_word("by") {
                source_side = self.take_word("source");
                if !source_side {
                    self.expect_word("target")?;
                }
            }
            let condition = if self.take_word("and") { Some(self.expression()?) } else { None };
            self.expect_word("then")?;
            let group = if source_side { 2 } else if when_matched { 0 } else { 1 };
            if nothing_after[group] {
                return Err(self.not_supported("a MERGE clause after a DO NOTHING clause of the same kind"));
            }
            if self.is_word("do") && self.is_word_at(1, "nothing") {
                self.at += 2;
                nothing_after[group] = true;
                continue;
            }
            if group == 1 {
                if insert.is_some() {
                    return Err(self.not_supported("more than one WHEN NOT MATCHED ... INSERT clause"));
                }
                if self.is_word("insert") && self.is_word_at(1, "default") {
                    return Err(self.not_supported("INSERT DEFAULT VALUES in MERGE"));
                }
                let (columns, values) = self.merge_insert_values()?;
                insert = Some(MergeInsert { columns, values, condition });
                continue;
            }
            let clause = self.sqlserver_merge_matched(condition)?;
            let clauses = if group == 0 { &mut matched } else { &mut by_source };
            if clauses.len() == 2 || clauses.first().is_some_and(|first| first.condition.is_none()) {
                return Err(SqlError::not_supported(
                    "more than two WHEN MATCHED clauses of one kind, or one after a clause without a condition, in MERGE",
                )
                .at(clause_position));
            }
            clauses.push(clause);
        }
        if self.is_word("returning") {
            return Err(self.not_supported("MERGE ... RETURNING"));
        }
        if matched.is_empty() && insert.is_none() && by_source.is_empty() {
            return Err(SqlError::syntax("MERGE needs at least one WHEN clause that acts", self.position()));
        }
        Ok(Statement::Merge(Box::new(MergeStatement {
            target,
            source: every_column_of(source, position),
            source_label,
            on,
            matched,
            insert,
            by_source,
            output: None,
            top: None,
        })))
    }

    fn sqlserver_merge_top(&mut self) -> SqlResult<Option<Expr>> {
        if !self.take_word("top") {
            return Ok(None);
        }
        let position = self.position();
        self.expect(&TokenKind::LeftParen)?;
        let count = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        let percent = self.take_word("percent");
        Ok(Some(sqlserver::top_limit(count, percent, false, position)))
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

fn deletes(clause: &MergeUpdate) -> bool {
    clause.delete_condition.is_some()
}

fn second_insert_clause(position: usize) -> SqlError {
    SqlError::syntax(
        "An action of type 'WHEN NOT MATCHED' cannot appear more than once in a 'INSERT' clause of a MERGE statement.",
        position,
    )
    .with_sqlserver_code(10714)
}

fn check_second_clause(
    kind: &str,
    earlier: &[MergeUpdate],
    clause: &MergeUpdate,
    position: usize,
) -> SqlResult<()> {
    let Some(first) = earlier.first() else {
        return Ok(());
    };
    let refused = |message: String| -> SqlResult<()> {
        Err(SqlError::syntax(message, position).with_sqlserver_code(10714))
    };
    if earlier.len() > 1 {
        return refused(format!(
            "A MERGE statement cannot have more than two {kind} clauses."
        ));
    }
    if first.condition.is_none() {
        return refused(format!(
            "The first of two {kind} clauses in a MERGE statement must have an AND search condition."
        ));
    }
    if deletes(first) == deletes(clause) {
        return refused(format!(
            "Two {kind} clauses in a MERGE statement must be one UPDATE and one DELETE."
        ));
    }
    Ok(())
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
        locking: None,
        grouping_sets: None,
        distinct_on: Vec::new(),
    }
}
