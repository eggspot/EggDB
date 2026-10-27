use super::Parser;
use crate::ast::{
    AlterAction, AlterSequence, AlterTable, ColumnDefinition, ConstraintKind, ConstraintSpec,
    CreateSequence, ForeignKeySpec, Name, ReferentialAction, SequenceOptions, Statement,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

const MAX_ALTER_ACTIONS: usize = 1024;

impl Parser<'_> {
    pub(super) fn at_table_constraint(&self) -> bool {
        self.is_word("check") || self.is_word("foreign")
    }

    pub(super) fn table_constraint_body(
        &mut self,
        name: Option<Name>,
        position: usize,
    ) -> SqlResult<ConstraintSpec> {
        let kind = if self.take_word("check") {
            self.check_body()?
        } else if self.take_word("foreign") {
            self.expect_word("key")?;
            if self.mysql() && self.peek_kind() != &TokenKind::LeftParen {
                self.name()?;
            }
            let columns = self.name_list()?;
            self.expect_word("references")?;
            ConstraintKind::ForeignKey(self.references_body(columns)?)
        } else if self.take_word("primary") {
            self.expect_word("key")?;
            if self.mysql() && self.take_word("using") {
                self.label()?;
            }
            let columns = self.index_column_list(true)?;
            ConstraintKind::PrimaryKey(columns)
        } else if self.is_word("unique") {
            let spec = self.unique_constraint(None, position)?;
            return Ok(ConstraintSpec {
                name: name.or(spec.name),
                kind: ConstraintKind::Unique(spec.columns),
                position,
                column: None,
            });
        } else {
            return Err(self.unexpected());
        };
        self.constraint_tail()?;
        Ok(ConstraintSpec {
            name,
            kind,
            position,
            column: None,
        })
    }

    fn constraint_tail(&mut self) -> SqlResult<()> {
        loop {
            if self.is_word("not") && self.is_word_at(1, "deferrable") {
                self.at += 2;
            } else if self.take_word("deferrable") {
            } else if self.take_word("initially") {
                if !self.take_word("deferred") {
                    self.expect_word("immediate")?;
                }
            } else if self.is_word("not") && self.is_word_at(1, "valid") {
                return Err(self.not_supported("NOT VALID constraints"));
            } else if self.is_word("no") && self.is_word_at(1, "inherit") {
                self.at += 2;
            } else if self.mysql() && self.take_word("enforced") {
            } else if self.mysql() && self.is_word("not") && self.is_word_at(1, "enforced") {
                return Err(self.not_supported("NOT ENFORCED constraints"));
            } else if self.oracle()
                && (self.take_word("enable")
                    || self.take_word("validate")
                    || self.take_word("novalidate")
                    || self.take_word("rely")
                    || self.take_word("norely"))
            {
            } else if self.oracle() && self.is_word("disable") {
                return Err(self.not_supported("disabled constraints"));
            } else if self.sqlite() && self.is_word("on") && self.is_word_at(1, "conflict") {
                self.sqlite_conflict_clause()?;
            } else {
                return Ok(());
            }
        }
    }

    fn check_body(&mut self) -> SqlResult<ConstraintKind> {
        self.expect(&TokenKind::LeftParen)?;
        let start = self.position();
        let expr = self.expression()?;
        let end = self.tokens[self.at.saturating_sub(1)].end;
        self.expect(&TokenKind::RightParen)?;
        let text = self.text.get(start..end).unwrap_or("").to_owned();
        Ok(ConstraintKind::Check { expr, text })
    }

    pub(super) fn references_body(&mut self, columns: Vec<Name>) -> SqlResult<ForeignKeySpec> {
        let parent = self.table_name()?;
        let parent_columns = if self.peek_kind() == &TokenKind::LeftParen {
            Some(self.name_list()?)
        } else {
            None
        };
        let mut spec = ForeignKeySpec {
            columns,
            parent,
            parent_columns,
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
            deferrable: false,
        };
        loop {
            if self.take_word("match") {
                if self.take_word("full") || self.take_word("partial") {
                    return Err(self.not_supported("MATCH FULL and MATCH PARTIAL"));
                }
                self.expect_word("simple")?;
            } else if self.is_word("on")
                && (self.is_word_at(1, "delete") || self.is_word_at(1, "update"))
            {
                self.at += 1;
                let delete = self.take_word("delete");
                if !delete {
                    self.expect_word("update")?;
                }
                let action = self.referential_action()?;
                if delete {
                    spec.on_delete = action;
                } else {
                    spec.on_update = action;
                }
            } else if self.is_word("not") && self.is_word_at(1, "deferrable") {
                self.at += 2;
                spec.deferrable = false;
            } else if self.take_word("deferrable") {
                spec.deferrable = true;
            } else if self.take_word("initially") {
                if !self.take_word("deferred") {
                    self.expect_word("immediate")?;
                }
            } else {
                return Ok(spec);
            }
        }
    }

    fn referential_action(&mut self) -> SqlResult<ReferentialAction> {
        if self.take_word("cascade") {
            Ok(ReferentialAction::Cascade)
        } else if self.take_word("restrict") {
            Ok(ReferentialAction::Restrict)
        } else if self.take_word("no") {
            self.expect_word("action")?;
            Ok(ReferentialAction::NoAction)
        } else if self.take_word("set") {
            if self.take_word("null") {
                Ok(ReferentialAction::SetNull)
            } else {
                self.expect_word("default")?;
                Ok(ReferentialAction::SetDefault)
            }
        } else {
            Err(self.unexpected())
        }
    }

    pub(super) fn column_constraint(
        &mut self,
        column: &mut ColumnDefinition,
        name: Option<Name>,
    ) -> SqlResult<bool> {
        let position = self.position();
        let kind = if self.take_word("check") {
            self.check_body()?
        } else if self.take_word("references") {
            let key = self.references_body(vec![column.name.clone()])?;
            if self.mysql() {
                return Ok(true);
            }
            ConstraintKind::ForeignKey(key)
        } else {
            return Ok(false);
        };
        self.constraint_tail()?;
        column.constraints.push(ConstraintSpec {
            name,
            kind,
            position,
            column: Some(column.name.clone()),
        });
        Ok(true)
    }

    fn signed_integer(&mut self) -> SqlResult<i64> {
        let position = self.position();
        let negative = self.take_operator("-");
        if !negative {
            self.take_operator("+");
        }
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        self.at += 1;
        let text = if negative {
            format!("-{digits}")
        } else {
            (*digits).to_owned()
        };
        text.parse::<i64>().map_err(|_| {
            SqlError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("value \"{text}\" is out of range for type bigint"),
            )
            .at(position)
        })
    }

    fn oracle_bound(&mut self) -> SqlResult<Option<i64>> {
        match self.signed_integer() {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.state() == SqlState::NUMERIC_VALUE_OUT_OF_RANGE => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn sequence_options(&mut self, alter: bool) -> SqlResult<SequenceOptions> {
        let mut options = SequenceOptions::default();
        loop {
            if !self.oracle() && self.take_word("as") {
                let position = self.position();
                let sql_type = self.sql_type()?;
                options.sql_type = Some((sql_type, position));
            } else if self.take_word("increment") {
                self.take_word("by");
                options.increment = Some(self.signed_integer()?);
            } else if self.take_word("minvalue") {
                options.min = Some(if self.oracle() {
                    self.oracle_bound()?
                } else {
                    Some(self.signed_integer()?)
                });
            } else if self.take_word("maxvalue") {
                options.max = Some(if self.oracle() {
                    self.oracle_bound()?
                } else {
                    Some(self.signed_integer()?)
                });
            } else if self.oracle() && self.take_word("nominvalue") {
                options.min = Some(None);
            } else if self.oracle() && self.take_word("nomaxvalue") {
                options.max = Some(None);
            } else if !self.oracle() && self.is_word("no") && self.is_word_at(1, "minvalue") {
                self.at += 2;
                options.min = Some(None);
            } else if !self.oracle() && self.is_word("no") && self.is_word_at(1, "maxvalue") {
                self.at += 2;
                options.max = Some(None);
            } else if !self.oracle() && self.is_word("no") && self.is_word_at(1, "cycle") {
                self.at += 2;
                options.cycle = Some(false);
            } else if self.take_word("start") {
                self.take_word("with");
                options.start = Some(self.signed_integer()?);
            } else if alter && self.take_word("restart") {
                let with = self.take_word("with");
                options.restart = Some(
                    if with
                        || matches!(self.peek_kind(), TokenKind::Integer(_))
                        || self.is_operator("-")
                    {
                        Some(self.signed_integer()?)
                    } else {
                        None
                    },
                );
            } else if self.take_word("cache") {
                options.cache = Some(self.signed_integer()?);
            } else if self.oracle() && self.take_word("nocache") {
                options.cache = Some(1);
            } else if self.take_word("cycle") {
                options.cycle = Some(true);
            } else if self.oracle() && self.take_word("nocycle") {
                options.cycle = Some(false);
            } else if self.oracle()
                && (self.take_word("order")
                    || self.take_word("noorder")
                    || self.take_word("keep")
                    || self.take_word("nokeep")
                    || self.take_word("noscale")
                    || self.take_word("global")
                    || self.take_word("session"))
            {
            } else if !self.oracle() && self.take_word("owned") {
                self.expect_word("by")?;
                if self.take_word("none") {
                    options.owned_by = Some(None);
                } else {
                    let mut parts = vec![self.name()?];
                    while parts.len() < 3 && self.take(&TokenKind::Dot) {
                        parts.push(self.name()?);
                    }
                    let column = parts.pop().ok_or_else(|| self.unexpected())?;
                    let table = parts.pop().ok_or_else(|| self.unexpected())?;
                    if let Some(schema) = parts.pop()
                        && schema.text != "public"
                    {
                        return Err(SqlError::not_supported(
                            "schemas other than public are not supported yet",
                        )
                        .at(schema.position));
                    }
                    options.owned_by = Some(Some((table, column)));
                }
            } else {
                return Ok(options);
            }
        }
    }

    pub(super) fn create_sequence(&mut self) -> SqlResult<Statement> {
        self.expect_word("sequence")?;
        let if_not_exists = if !self.oracle() && self.take_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = self.table_name()?;
        let options = self.sequence_options(false)?;
        Ok(Statement::CreateSequence(CreateSequence {
            name,
            if_not_exists,
            options,
        }))
    }

    pub(super) fn alter_sequence(&mut self) -> SqlResult<Statement> {
        self.expect_word("sequence")?;
        let if_exists = if !self.oracle() && self.take_word("if") {
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = self.table_name()?;
        if !self.oracle() && self.take_word("owner") {
            self.expect_word("to")?;
            self.label()?;
            return Ok(Statement::AlterSequence(AlterSequence {
                name,
                if_exists,
                options: SequenceOptions::default(),
                rename: None,
            }));
        }
        if !self.oracle() && self.take_word("rename") {
            self.expect_word("to")?;
            let to = self.name()?;
            return Ok(Statement::AlterSequence(AlterSequence {
                name,
                if_exists,
                options: SequenceOptions::default(),
                rename: Some(to),
            }));
        }
        let options = self.sequence_options(true)?;
        Ok(Statement::AlterSequence(AlterSequence {
            name,
            if_exists,
            options,
            rename: None,
        }))
    }

    pub(super) fn drop_sequence(&mut self) -> SqlResult<Statement> {
        self.expect_word("sequence")?;
        let if_exists = if !self.oracle() && self.take_word("if") {
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let mut names = vec![self.table_name()?];
        while !self.oracle() && self.take(&TokenKind::Comma) {
            names.push(self.table_name()?);
        }
        let cascade = !self.oracle() && self.take_word("cascade");
        if !cascade && !self.oracle() {
            self.take_word("restrict");
        }
        Ok(Statement::DropSequence {
            names,
            if_exists,
            cascade,
        })
    }

    fn add_column_definition(&mut self) -> SqlResult<ColumnDefinition> {
        if self.sqlite() {
            let (column, _) = self.sqlite_column_definition()?;
            Ok(column)
        } else {
            self.column_definition()
        }
    }

    fn drop_behaviour(&mut self) -> bool {
        if self.take_word("cascade") {
            return true;
        }
        self.take_word("restrict");
        false
    }

    pub(super) fn alter_table(&mut self) -> SqlResult<Statement> {
        self.expect_word("table")?;
        let postgresql = self.postgresql();
        let if_exists = if postgresql && self.is_word("if") && self.is_word_at(1, "exists") {
            self.at += 2;
            true
        } else {
            false
        };
        if postgresql {
            self.take_word("only");
        }
        let name = self.table_name()?;
        let mut actions = Vec::new();
        loop {
            if actions.len() >= MAX_ALTER_ACTIONS {
                return Err(SqlError::new(
                    SqlState::STATEMENT_TOO_COMPLEX,
                    format!("ALTER TABLE takes at most {MAX_ALTER_ACTIONS} actions"),
                ));
            }
            self.alter_action(&mut actions)?;
            if self.sqlite() || !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::AlterTable(AlterTable {
            name,
            if_exists,
            actions,
        }))
    }

    fn alter_action(&mut self, actions: &mut Vec<AlterAction>) -> SqlResult<()> {
        let position = self.position();
        if self.take_word("rename") {
            return self.rename_action(actions);
        }
        if self.sqlite() && !(self.is_word("add") || self.is_word("drop")) {
            return Err(self.unexpected());
        }
        if self.take_word("add") {
            return self.add_action(actions, position);
        }
        if self.take_word("drop") {
            return self.drop_action(actions, position);
        }
        if (self.postgresql() || self.mysql()) && self.take_word("alter") {
            self.take_word("column");
            let column = self.name()?;
            actions.push(self.alter_column_action(column)?);
            return Ok(());
        }
        if self.postgresql() && self.take_word("owner") {
            self.expect_word("to")?;
            self.label()?;
            actions.push(AlterAction::Accepted);
            return Ok(());
        }
        if self.postgresql() && self.is_word("set") && self.is_word_at(1, "schema") {
            self.at += 2;
            let schema = self.name()?;
            actions.push(AlterAction::SetSchema(crate::schemas::schema_named(
                &schema.text,
                schema.position,
            )?));
            return Ok(());
        }
        if self.mysql() && (self.is_word("modify") || self.is_word("change")) {
            let change = self.take_word("change");
            if !change {
                self.at += 1;
            }
            self.take_word("column");
            let old_name = if change { Some(self.name()?) } else { None };
            let column = self.column_definition()?;
            if self.is_word("first") || self.is_word("after") {
                return Err(self.not_supported("FIRST and AFTER"));
            }
            actions.push(AlterAction::Redefine {
                old_name: old_name.unwrap_or_else(|| column.name.clone()),
                column,
            });
            return Ok(());
        }
        if self.mysql() && self.take_word("auto_increment") {
            self.take_operator("=");
            self.signed_integer()?;
            actions.push(AlterAction::Accepted);
            return Ok(());
        }
        if self.oracle() && self.take_word("modify") {
            let parenthesized = self.take(&TokenKind::LeftParen);
            loop {
                let column = self.name()?;
                self.oracle_modify(column, actions)?;
                if !parenthesized || !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            if parenthesized {
                self.expect(&TokenKind::RightParen)?;
            }
            return Ok(());
        }
        Err(self.unexpected())
    }

    fn oracle_modify(&mut self, column: Name, actions: &mut Vec<AlterAction>) -> SqlResult<()> {
        let mut changed = false;
        if !self.is_word("default")
            && !self.is_word("not")
            && !self.is_word("null")
            && matches!(self.peek_kind(), TokenKind::Word(_))
        {
            let sql_type = self.sql_type()?;
            actions.push(AlterAction::SetType {
                column: column.clone(),
                sql_type,
            });
            changed = true;
        }
        loop {
            if self.take_word("default") {
                let start = self.position();
                let expr = self.binary_operand()?;
                let end = self.tokens[self.at.saturating_sub(1)].end;
                let source = self.text.get(start..end).unwrap_or("").to_owned();
                actions.push(AlterAction::SetDefault {
                    column: column.clone(),
                    default: Some((expr, source)),
                });
            } else if self.take_word("not") {
                self.expect_word("null")?;
                actions.push(AlterAction::SetNotNull {
                    column: column.clone(),
                    not_null: true,
                });
            } else if self.take_word("null") {
                actions.push(AlterAction::SetNotNull {
                    column: column.clone(),
                    not_null: false,
                });
            } else if changed {
                return Ok(());
            } else {
                return Err(self.unexpected());
            }
            changed = true;
        }
    }

    fn rename_action(&mut self, actions: &mut Vec<AlterAction>) -> SqlResult<()> {
        if self.take_word("to") || (self.mysql() && self.take_word("as")) {
            actions.push(AlterAction::RenameTable(self.table_name()?));
            return Ok(());
        }
        if self.is_word("constraint") {
            return Err(self.not_supported("RENAME CONSTRAINT"));
        }
        let column_word = self.take_word("column");
        if !column_word && (self.mysql() || self.oracle()) {
            if self.mysql() && !self.is_word("to") {
                actions.push(AlterAction::RenameTable(self.table_name()?));
                return Ok(());
            }
            return Err(self.unexpected());
        }
        let from = self.name()?;
        self.expect_word("to")?;
        let to = self.name()?;
        actions.push(AlterAction::RenameColumn { from, to });
        Ok(())
    }

    fn add_action(&mut self, actions: &mut Vec<AlterAction>, position: usize) -> SqlResult<()> {
        if self.sqlite() {
            self.take_word("column");
            let column = self.add_column_definition()?;
            actions.push(AlterAction::AddColumn {
                column,
                if_not_exists: false,
            });
            return Ok(());
        }
        let constraint_name = if self.take_word("constraint") {
            if self.mysql()
                && (self.is_word("primary")
                    || self.is_word("unique")
                    || self.is_word("foreign")
                    || self.is_word("check"))
            {
                None
            } else {
                Some(self.name()?)
            }
        } else {
            None
        };
        if constraint_name.is_some()
            || self.is_word("primary")
            || self.is_word("unique")
            || self.is_word("foreign")
            || self.is_word("check")
        {
            let spec = self.table_constraint_body(constraint_name, position)?;
            actions.push(AlterAction::AddConstraint(spec));
            return Ok(());
        }
        if self.mysql() && (self.is_word("index") || self.is_word("key")) {
            return Err(self.not_supported("ADD INDEX in ALTER TABLE (use CREATE INDEX)"));
        }
        if self.oracle() && self.take(&TokenKind::LeftParen) {
            loop {
                let column = self.column_definition()?;
                actions.push(AlterAction::AddColumn {
                    column,
                    if_not_exists: false,
                });
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
            return Ok(());
        }
        if !self.oracle() {
            self.take_word("column");
        }
        let if_not_exists = if self.postgresql() && self.is_word("if") && self.is_word_at(1, "not")
        {
            self.at += 2;
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let column = self.add_column_definition()?;
        if self.mysql() && (self.is_word("first") || self.is_word("after")) {
            return Err(self.not_supported("FIRST and AFTER"));
        }
        actions.push(AlterAction::AddColumn {
            column,
            if_not_exists,
        });
        Ok(())
    }

    fn drop_action(&mut self, actions: &mut Vec<AlterAction>, position: usize) -> SqlResult<()> {
        if self.sqlite() {
            self.take_word("column");
            let name = self.name()?;
            actions.push(AlterAction::DropColumn {
                name,
                if_exists: false,
                cascade: false,
            });
            return Ok(());
        }
        if (self.mysql() || self.oracle()) && self.is_word("primary") {
            self.at += 1;
            self.expect_word("key")?;
            if self.oracle() {
                self.take_word("cascade");
            }
            actions.push(AlterAction::DropPrimaryKey(position));
            return Ok(());
        }
        let mysql_kind = self.mysql()
            && if self.take_word("foreign") {
                self.expect_word("key")?;
                true
            } else {
                self.take_word("check") || self.take_word("index") || self.take_word("key")
            };
        if mysql_kind || self.take_word("constraint") {
            let if_exists =
                if self.postgresql() && self.is_word("if") && self.is_word_at(1, "exists") {
                    self.at += 2;
                    true
                } else {
                    false
                };
            let name = self.name()?;
            let cascade = self.drop_behaviour();
            actions.push(AlterAction::DropConstraint {
                name,
                if_exists,
                cascade,
            });
            return Ok(());
        }
        if self.oracle() && self.take(&TokenKind::LeftParen) {
            loop {
                let name = self.name()?;
                actions.push(AlterAction::DropColumn {
                    name,
                    if_exists: false,
                    cascade: false,
                });
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
            if self.take_word("cascade") {
                self.expect_word("constraints")?;
            }
            return Ok(());
        }
        if self.oracle() {
            self.expect_word("column")?;
        } else {
            self.take_word("column");
        }
        let if_exists = if self.postgresql() && self.is_word("if") && self.is_word_at(1, "exists") {
            self.at += 2;
            true
        } else {
            false
        };
        let name = self.name()?;
        let cascade = if self.oracle() {
            self.take_word("cascade") && self.expect_word("constraints").is_ok()
        } else {
            self.drop_behaviour()
        };
        actions.push(AlterAction::DropColumn {
            name,
            if_exists,
            cascade,
        });
        Ok(())
    }

    fn alter_column_action(&mut self, column: Name) -> SqlResult<AlterAction> {
        if self.take_word("set") {
            if self.take_word("default") {
                let start = self.position();
                let expr = self.binary_operand()?;
                let end = self.tokens[self.at.saturating_sub(1)].end;
                let source = if self.mysql() {
                    super::mysql::postgresql_source(&expr).ok_or_else(|| {
                        SqlError::not_supported("a DEFAULT other than a constant").at(start)
                    })?
                } else {
                    self.text.get(start..end).unwrap_or("").to_owned()
                };
                return Ok(AlterAction::SetDefault {
                    column,
                    default: Some((expr, source)),
                });
            }
            if self.postgresql() && self.take_word("not") {
                self.expect_word("null")?;
                return Ok(AlterAction::SetNotNull {
                    column,
                    not_null: true,
                });
            }
            if self.postgresql() && self.take_word("data") {
                self.expect_word("type")?;
                return self.set_type_action(column);
            }
            return Err(self.unexpected());
        }
        if self.take_word("drop") {
            if self.take_word("default") {
                return Ok(AlterAction::SetDefault {
                    column,
                    default: None,
                });
            }
            if self.postgresql() && self.take_word("not") {
                self.expect_word("null")?;
                return Ok(AlterAction::SetNotNull {
                    column,
                    not_null: false,
                });
            }
            return Err(self.unexpected());
        }
        if self.postgresql() && self.take_word("type") {
            return self.set_type_action(column);
        }
        Err(self.unexpected())
    }

    fn set_type_action(&mut self, column: Name) -> SqlResult<AlterAction> {
        let sql_type = self.sql_type()?;
        if self.is_word("using") || self.is_word("collate") {
            return Err(self.not_supported("ALTER COLUMN TYPE with USING or COLLATE"));
        }
        Ok(AlterAction::SetType { column, sql_type })
    }
}
