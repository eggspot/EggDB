use super::Parser;
use crate::ast::{
    CreateTrigger, DropTrigger, EnableTrigger, Expr, ExprKind, GeneratedTable, Literal, Name,
    Statement, TableReference, TriggerName, TriggerPlacement,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::triggers::{
    BodyStep, ConditionalBranch, RAISE_FUNCTION, SignalStep, StepTarget, TriggerAction,
    TriggerDefinition, TriggerEvent, TriggerEvents, TriggerFrame, TriggerLevel, TriggerTiming,
};
use crate::types::Value;

const MAX_BODY_NESTING: usize = 64;
const MAX_BODY_STEPS: usize = 10_000;
const MAX_DEFINER_TOKENS: usize = 12;

fn nested_too_deeply(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        "the trigger's body is nested too deeply",
    )
    .with_hint(format!(
        "A trigger's body nests IF, WHILE and BEGIN ... END at most {MAX_BODY_NESTING} deep."
    ))
    .at(position)
}

fn too_many_steps(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a trigger's body has at most {MAX_BODY_STEPS} statements"),
    )
    .at(position)
}

fn inconsistent(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_OBJECT_DEFINITION,
        "this combination of timing, events and FOR EACH is not a valid trigger",
    )
    .with_hint(
        "INSTEAD OF triggers are FOR EACH ROW, TRUNCATE triggers FOR EACH STATEMENT, REFERENCING needs AFTER, and UPDATE OF needs an UPDATE event.",
    )
    .at(position)
}

fn transact_sql_first_in_batch(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "'CREATE TRIGGER' must be the first statement in a query batch.",
    )
    .with_sqlserver_code(111)
    .at(position)
}

fn runs_in_trigger(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
            | Statement::Select(_)
            | Statement::Query(_)
            | Statement::With(_)
    )
}

impl<'t> Parser<'t> {
    pub(crate) fn with_trigger_frame(mut self, frame: &'t TriggerFrame) -> Self {
        self.trigger_frame = Some(frame);
        self
    }

    pub(super) fn previous_end(&self) -> usize {
        self.at
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index))
            .map_or(0, |token| token.end)
    }

    pub(super) fn slice(&self, start: usize, end: usize) -> String {
        self.text.get(start..end).unwrap_or_default().to_owned()
    }

    fn creates_trigger(&self) -> bool {
        let mut ahead = 1;
        if self.is_word_at(ahead, "or")
            && (self.is_word_at(ahead + 1, "replace") || self.is_word_at(ahead + 1, "alter"))
        {
            ahead += 2;
        }
        if self.is_word_at(ahead, "temp") || self.is_word_at(ahead, "temporary") {
            ahead += 1;
        }
        if self.is_word_at(ahead, "constraint") {
            ahead += 1;
        }
        if self.is_word_at(ahead, "definer") {
            for offset in 1..MAX_DEFINER_TOKENS {
                if self.is_word_at(ahead + offset, "trigger") {
                    return true;
                }
                if self.peek_kind_at(ahead + offset) == &TokenKind::End {
                    return false;
                }
            }
            return false;
        }
        self.is_word_at(ahead, "trigger")
    }

    fn alters_table_triggers(&self) -> bool {
        if !self.is_word_at(1, "table") {
            return false;
        }
        let mut ahead = 2;
        if self.is_word_at(ahead, "only") {
            ahead += 1;
        }
        if self.is_word_at(ahead, "if") && self.is_word_at(ahead + 1, "exists") {
            ahead += 2;
        }
        let mut parts = 0;
        loop {
            match self.peek_kind_at(ahead) {
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) => {
                    ahead += 1;
                    parts += 1;
                }
                _ => return false,
            }
            if parts < 3 && self.peek_kind_at(ahead) == &TokenKind::Dot {
                ahead += 1;
                continue;
            }
            break;
        }
        if !(self.is_word_at(ahead, "enable") || self.is_word_at(ahead, "disable")) {
            return false;
        }
        ahead += 1;
        if self.is_word_at(ahead, "replica") || self.is_word_at(ahead, "always") {
            ahead += 1;
        }
        self.is_word_at(ahead, "trigger")
    }

    pub(super) fn trigger_statement(&mut self) -> SqlResult<Option<Statement>> {
        if self.oracle() {
            return Ok(None);
        }
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        match word.as_ref() {
            "create" if self.creates_trigger() => self.create_trigger(false).map(Some),
            "alter" if self.sqlserver() && self.is_word_at(1, "trigger") => {
                self.create_trigger(true).map(Some)
            }
            "drop" if self.is_word_at(1, "trigger") => self.drop_trigger().map(Some),
            "enable" | "disable" if self.sqlserver() && self.is_word_at(1, "trigger") => {
                self.sqlserver_enable_trigger().map(Some)
            }
            "alter"
                if (self.postgresql() || self.sqlserver()) && self.alters_table_triggers() =>
            {
                self.alter_table_triggers().map(Some)
            }
            _ => Ok(None),
        }
    }

    fn trigger_name(&mut self) -> SqlResult<(Option<String>, Name)> {
        let first = self.name()?;
        if self.take(&TokenKind::Dot) {
            let name = self.name()?;
            return Ok((Some(first.text), name));
        }
        Ok((None, first))
    }

    fn take_if_exists(&mut self) -> SqlResult<bool> {
        if self.take_word("if") {
            self.expect_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn create_trigger(&mut self, alter: bool) -> SqlResult<Statement> {
        let position = self.position();
        let start = self.peek().start;
        if self.sqlserver() && !self.first_in_batch() {
            return Err(transact_sql_first_in_batch(position));
        }
        self.at += 1;
        let mut replace = alter;
        if self.take_word("or") {
            if !(self.take_word("replace") || self.take_word("alter")) {
                return Err(self.unexpected());
            }
            replace = true;
        }
        if self.is_word("temp") || self.is_word("temporary") {
            return Err(self.not_supported("CREATE TEMP TRIGGER"));
        }
        if self.is_word("constraint") {
            return Err(self.not_supported("CREATE CONSTRAINT TRIGGER"));
        }
        if self.take_word("definer") {
            while !self.is_word("trigger") && self.peek_kind() != &TokenKind::End {
                self.at += 1;
            }
        }
        self.expect_word("trigger")?;
        let if_not_exists = if (self.sqlite() || self.mysql()) && self.take_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let (_, name) = self.trigger_name()?;
        let parts = match self.dialect {
            Dialect::SqlServer => self.sqlserver_trigger()?,
            Dialect::PostgreSql => self.postgresql_trigger()?,
            Dialect::Sqlite | Dialect::MySql => self.row_trigger()?,
            Dialect::Oracle => return Err(self.not_supported("CREATE TRIGGER")),
        };
        let definition = TriggerDefinition {
            name: name.text.clone(),
            dialect: self.dialect,
            timing: parts.timing,
            events: parts.events,
            columns: parts.columns,
            level: parts.level,
            condition: parts.condition,
            old_table: parts.old_table,
            new_table: parts.new_table,
            action: parts.action,
            enabled: true,
            text: self.slice(start, self.previous_end()),
        };
        if !definition.is_consistent() {
            return Err(inconsistent(position));
        }
        Ok(Statement::CreateTrigger(Box::new(CreateTrigger {
            name,
            table: parts.table,
            replace,
            if_not_exists,
            placement: parts.placement,
            definition,
        })))
    }

    fn trigger_timing(&mut self) -> SqlResult<Option<TriggerTiming>> {
        if self.take_word("before") {
            return Ok(Some(TriggerTiming::Before));
        }
        if self.take_word("after") {
            return Ok(Some(TriggerTiming::After));
        }
        if self.take_word("instead") {
            self.expect_word("of")?;
            return Ok(Some(TriggerTiming::InsteadOf));
        }
        Ok(None)
    }

    fn trigger_event(&mut self, columns: &mut Vec<String>) -> SqlResult<TriggerEvent> {
        let event = if self.take_word("insert") {
            TriggerEvent::Insert
        } else if self.take_word("update") {
            TriggerEvent::Update
        } else if self.take_word("delete") {
            TriggerEvent::Delete
        } else if self.postgresql() && self.take_word("truncate") {
            TriggerEvent::Truncate
        } else {
            return Err(self.unexpected());
        };
        if event == TriggerEvent::Update && !self.sqlserver() && self.take_word("of") {
            loop {
                columns.push(self.name()?.text);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(event)
    }

    fn row_trigger(&mut self) -> SqlResult<TriggerParts> {
        let timing = match self.trigger_timing()? {
            Some(timing) => timing,
            None if self.sqlite() => TriggerTiming::Before,
            None => return Err(self.unexpected()),
        };
        let mut columns = Vec::new();
        let event = self.trigger_event(&mut columns)?;
        self.expect_word("on")?;
        let table = self.table_name()?;
        if self.mysql() {
            self.expect_word("for")?;
            self.expect_word("each")?;
            self.expect_word("row")?;
        } else if self.take_word("for") {
            self.expect_word("each")?;
            self.expect_word("row")?;
        }
        let placement = if self.mysql() && (self.is_word("follows") || self.is_word("precedes")) {
            let follows = self.take_word("follows");
            if !follows {
                self.at += 1;
            }
            Some(TriggerPlacement {
                follows,
                other: self.name()?,
            })
        } else {
            None
        };
        let condition = if self.sqlite() && self.take_word("when") {
            let condition_start = self.peek().start;
            self.expression()?;
            Some(self.slice(condition_start, self.previous_end()))
        } else {
            None
        };
        if self.sqlite() && !self.is_word("begin") {
            return Err(self.unexpected());
        }
        let body_start = self.peek().start;
        self.trigger_body()?;
        Ok(TriggerParts {
            table,
            timing,
            events: TriggerEvents::default().with(event),
            columns,
            level: TriggerLevel::Row,
            condition,
            old_table: None,
            new_table: None,
            action: TriggerAction::Statements(self.slice(body_start, self.previous_end())),
            placement,
        })
    }

    fn postgresql_trigger(&mut self) -> SqlResult<TriggerParts> {
        let Some(timing) = self.trigger_timing()? else {
            return Err(self.unexpected());
        };
        let mut columns = Vec::new();
        let mut events = TriggerEvents::default().with(self.trigger_event(&mut columns)?);
        while self.take_word("or") {
            events = events.with(self.trigger_event(&mut columns)?);
        }
        self.expect_word("on")?;
        let table = self.table_name()?;
        if self.is_word("from") {
            return Err(self.not_supported("CREATE TRIGGER ... FROM"));
        }
        if self.take_word("not") {
            self.expect_word("deferrable")?;
        }
        if self.is_word("deferrable") || self.is_word("initially") {
            return Err(self.not_supported("deferrable triggers"));
        }
        let mut old_table = None;
        let mut new_table = None;
        if self.take_word("referencing") {
            loop {
                let old = if self.take_word("old") {
                    true
                } else if self.take_word("new") {
                    false
                } else {
                    break;
                };
                self.expect_word("table")?;
                self.take_word("as");
                let name = self.name()?.text;
                if old {
                    old_table = Some(name);
                } else {
                    new_table = Some(name);
                }
            }
            if old_table.is_none() && new_table.is_none() {
                return Err(self.unexpected());
            }
        }
        let mut level = TriggerLevel::Statement;
        if self.take_word("for") {
            self.take_word("each");
            if self.take_word("row") {
                level = TriggerLevel::Row;
            } else {
                self.expect_word("statement")?;
            }
        }
        let condition = if self.take_word("when") {
            self.expect(&TokenKind::LeftParen)?;
            let condition_start = self.peek().start;
            self.expression()?;
            let condition = self.slice(condition_start, self.previous_end());
            self.expect(&TokenKind::RightParen)?;
            Some(condition)
        } else {
            None
        };
        self.expect_word("execute")?;
        if !(self.take_word("function") || self.take_word("procedure")) {
            return Err(self.unexpected());
        }
        let (schema, function) = self.trigger_name()?;
        let name = match schema {
            Some(schema) => format!("{schema}.{}", function.text),
            None => function.text,
        };
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        while !self.take(&TokenKind::RightParen) {
            let argument = match self.peek_kind() {
                TokenKind::String(text) => text.as_ref().to_owned(),
                TokenKind::Integer(digits) | TokenKind::Decimal(digits) => (*digits).to_owned(),
                TokenKind::Word(word) => word.as_ref().to_owned(),
                TokenKind::QuotedIdentifier(name) => name.clone(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            arguments.push(argument);
            if !self.take(&TokenKind::Comma) {
                self.expect(&TokenKind::RightParen)?;
                break;
            }
        }
        Ok(TriggerParts {
            table,
            timing,
            events,
            columns,
            level,
            condition,
            old_table,
            new_table,
            action: TriggerAction::Function { name, arguments },
            placement: None,
        })
    }

    fn sqlserver_trigger(&mut self) -> SqlResult<TriggerParts> {
        self.expect_word("on")?;
        if self.is_word("database") || self.is_word("all") {
            return Err(self.not_supported("DDL triggers (ON DATABASE, ON ALL SERVER)"));
        }
        let table = self.table_name()?;
        if self.take_word("with") {
            while !(self.is_word("for") || self.is_word("after") || self.is_word("instead"))
                && self.peek_kind() != &TokenKind::End
            {
                self.at += 1;
            }
        }
        let timing = if self.take_word("for") || self.take_word("after") {
            TriggerTiming::After
        } else if self.take_word("instead") {
            self.expect_word("of")?;
            TriggerTiming::InsteadOf
        } else {
            return Err(self.unexpected());
        };
        let mut columns = Vec::new();
        let mut events = TriggerEvents::default().with(self.trigger_event(&mut columns)?);
        while self.take(&TokenKind::Comma) {
            events = events.with(self.trigger_event(&mut columns)?);
        }
        if self.take_word("with") {
            self.expect_word("append")?;
        }
        if self.take_word("not") {
            self.expect_word("for")?;
            self.expect_word("replication")?;
        }
        self.expect_word("as")?;
        let body_start = self.peek().start;
        if self.at_end() {
            return Err(self.unexpected());
        }
        self.compile_batch(true)?;
        Ok(TriggerParts {
            table,
            timing,
            events,
            columns,
            level: TriggerLevel::Statement,
            condition: None,
            old_table: None,
            new_table: None,
            action: TriggerAction::Batch(self.slice(body_start, self.previous_end())),
            placement: None,
        })
    }

    fn drop_trigger(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let if_exists = self.take_if_exists()?;
        let mut names = Vec::new();
        loop {
            let (schema, name) = self.trigger_name()?;
            names.push(TriggerName { schema, name });
            if !(self.sqlserver() && self.take(&TokenKind::Comma)) {
                break;
            }
        }
        let table = if self.take_word("on") {
            if self.is_word("database") || self.is_word("all") {
                return Err(self.not_supported("DDL triggers (ON DATABASE, ON ALL SERVER)"));
            }
            Some(self.table_name()?)
        } else if self.postgresql() {
            return Err(self.unexpected());
        } else {
            None
        };
        if self.postgresql() && !self.take_word("cascade") {
            self.take_word("restrict");
        }
        Ok(Statement::DropTrigger(DropTrigger {
            names,
            table,
            if_exists,
        }))
    }

    fn trigger_list(&mut self) -> SqlResult<Option<Vec<Name>>> {
        if self.take_word("all") || self.take_word("user") {
            return Ok(None);
        }
        let mut names = Vec::new();
        loop {
            let (_, name) = self.trigger_name()?;
            names.push(name);
            if !self.take(&TokenKind::Comma) {
                return Ok(Some(names));
            }
        }
    }

    fn sqlserver_enable_trigger(&mut self) -> SqlResult<Statement> {
        let enable = self.is_word("enable");
        self.at += 2;
        let names = self.trigger_list()?;
        self.expect_word("on")?;
        if self.is_word("database") || self.is_word("all") {
            return Err(self.not_supported("DDL triggers (ON DATABASE, ON ALL SERVER)"));
        }
        let table = self.table_name()?;
        Ok(Statement::EnableTrigger(EnableTrigger {
            names,
            table,
            enable,
        }))
    }

    fn alter_table_triggers(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        self.take_word("only");
        if self.take_word("if") {
            self.expect_word("exists")?;
        }
        let table = self.table_name()?;
        let enable = self.take_word("enable");
        if !enable {
            self.expect_word("disable")?;
        }
        if !self.take_word("replica") {
            self.take_word("always");
        }
        self.expect_word("trigger")?;
        let names = self.trigger_list()?;
        Ok(Statement::EnableTrigger(EnableTrigger {
            names,
            table,
            enable,
        }))
    }

    pub(crate) fn trigger_body(&mut self) -> SqlResult<Vec<BodyStep>> {
        if self.take_word("begin") {
            let steps = self.trigger_items(1, &["end"])?;
            self.expect_word("end")?;
            return Ok(steps);
        }
        if self.sqlite() {
            return Err(self.unexpected());
        }
        Ok(vec![self.trigger_item(1)?])
    }

    fn trigger_items(&mut self, depth: usize, stops: &[&str]) -> SqlResult<Vec<BodyStep>> {
        let mut steps = Vec::new();
        loop {
            while self.take(&TokenKind::Semicolon) {}
            if stops.iter().any(|stop| self.is_word(stop)) {
                return Ok(steps);
            }
            if self.at_end() {
                return Err(self.unexpected());
            }
            if steps.len() >= MAX_BODY_STEPS {
                return Err(too_many_steps(self.position()));
            }
            steps.push(self.trigger_item(depth)?);
        }
    }

    fn trigger_item(&mut self, depth: usize) -> SqlResult<BodyStep> {
        if depth > MAX_BODY_NESTING {
            return Err(nested_too_deeply(self.position()));
        }
        if self.mysql()
            && let TokenKind::Word(word) = self.peek_kind()
        {
            match word.as_ref() {
                "begin" => {
                    self.at += 1;
                    let steps = self.trigger_items(depth + 1, &["end"])?;
                    self.expect_word("end")?;
                    return Ok(BodyStep::Block(steps));
                }
                "declare" => return self.trigger_declare(),
                "set" => return self.trigger_assignment(),
                "if" => return self.trigger_if(depth),
                "while" => return self.trigger_while(depth),
                "signal" => return self.trigger_signal(),
                _ => {}
            }
        }
        let at = self.at;
        let position = self.position();
        let statement = self.statement()?;
        if !runs_in_trigger(&statement) {
            return Err(SqlError::not_supported(
                "only INSERT, UPDATE, DELETE and SELECT statements run in a trigger's body",
            )
            .at(position));
        }
        Ok(BodyStep::Statement(at))
    }

    fn expect_assignment_operator(&mut self) -> SqlResult<()> {
        if self.take_operator("=") || self.take_operator(":=") {
            return Ok(());
        }
        if self.peek_kind() == &TokenKind::Colon && matches!(self.peek_kind_at(1), TokenKind::Operator(symbol) if *symbol == "=")
        {
            self.at += 2;
            return Ok(());
        }
        Err(self.unexpected())
    }

    fn skipped_expression(&mut self) -> SqlResult<usize> {
        let at = self.at;
        self.expression()?;
        Ok(at)
    }

    fn row_column(&mut self) -> SqlResult<String> {
        let text = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(text)
    }

    fn trigger_declare(&mut self) -> SqlResult<BodyStep> {
        self.at += 1;
        if ["continue", "exit", "undo"]
            .iter()
            .any(|word| self.is_word(word))
        {
            return Err(self.not_supported("DECLARE ... HANDLER in a trigger"));
        }
        let mut names = vec![self.name()?.text];
        while self.take(&TokenKind::Comma) {
            names.push(self.name()?.text);
        }
        if self.is_word("cursor") || self.is_word("condition") {
            return Err(self.not_supported("DECLARE ... CURSOR and CONDITION in a trigger"));
        }
        let sql_type = self.sql_type()?;
        let default = if self.take_word("default") {
            Some(self.skipped_expression()?)
        } else {
            None
        };
        Ok(BodyStep::Declare {
            names,
            sql_type,
            default,
        })
    }

    fn trigger_assignment(&mut self) -> SqlResult<BodyStep> {
        self.at += 1;
        let mut targets = Vec::new();
        loop {
            let target = if (self.is_word("new") || self.is_word("old"))
                && self.peek_kind_at(1) == &TokenKind::Dot
            {
                let old = self.is_word("old");
                self.at += 2;
                let column = self.row_column()?;
                if old {
                    StepTarget::Old(column)
                } else {
                    StepTarget::New(column)
                }
            } else if matches!(self.peek_kind(), TokenKind::Variable(_)) {
                return Err(self.not_supported("user variables (@name) in a trigger"));
            } else {
                StepTarget::Local(self.name()?.text)
            };
            self.expect_assignment_operator()?;
            targets.push((target, self.skipped_expression()?));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(BodyStep::Assign(targets))
    }

    fn trigger_if(&mut self, depth: usize) -> SqlResult<BodyStep> {
        self.at += 1;
        let mut branches = Vec::new();
        loop {
            let condition = self.skipped_expression()?;
            self.expect_word("then")?;
            let steps = self.trigger_items(depth + 1, &["elseif", "else", "end"])?;
            branches.push(ConditionalBranch { condition, steps });
            if !self.take_word("elseif") {
                break;
            }
        }
        let otherwise = if self.take_word("else") {
            self.trigger_items(depth + 1, &["end"])?
        } else {
            Vec::new()
        };
        self.expect_word("end")?;
        self.expect_word("if")?;
        Ok(BodyStep::If {
            branches,
            otherwise,
        })
    }

    fn trigger_while(&mut self, depth: usize) -> SqlResult<BodyStep> {
        self.at += 1;
        let condition = self.skipped_expression()?;
        self.expect_word("do")?;
        let body = self.trigger_items(depth + 1, &["end"])?;
        self.expect_word("end")?;
        self.expect_word("while")?;
        Ok(BodyStep::While { condition, body })
    }

    fn trigger_signal(&mut self) -> SqlResult<BodyStep> {
        self.at += 1;
        self.expect_word("sqlstate")?;
        self.take_word("value");
        let TokenKind::String(state) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let state = state.as_ref().to_owned();
        self.at += 1;
        let mut message = None;
        let mut code = None;
        if self.take_word("set") {
            loop {
                if self.take_word("message_text") {
                    self.expect_assignment_operator()?;
                    message = Some(self.skipped_expression()?);
                } else if self.take_word("mysql_errno") {
                    self.expect_assignment_operator()?;
                    let TokenKind::Integer(digits) = self.peek_kind() else {
                        return Err(self.unexpected());
                    };
                    code = digits.parse::<u16>().ok();
                    self.at += 1;
                } else {
                    self.name()?;
                    self.expect_assignment_operator()?;
                    self.expression()?;
                }
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(BodyStep::Signal(SignalStep {
            state,
            message,
            code,
        }))
    }

    pub(super) fn trigger_primary(&mut self) -> SqlResult<Option<Expr>> {
        let sqlserver_trigger = self.sqlserver()
            && self
                .batch
                .is_some_and(|batch| batch.updated_columns.is_some());
        if self.trigger_frame.is_none() && !self.sqlite() && !sqlserver_trigger {
            return Ok(None);
        }
        let position = self.position();
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        let word = word.as_ref();
        let call = self.peek_kind_at(1) == &TokenKind::LeftParen;
        if self.sqlite() && word == "raise" && call {
            return self.sqlite_raise(position).map(Some);
        }
        if sqlserver_trigger && word == "update" && call {
            self.at += 2;
            let column = self.name()?;
            self.expect(&TokenKind::RightParen)?;
            let updated = self
                .batch
                .and_then(|batch| batch.updated_columns.as_ref())
                .is_some_and(|columns| {
                    columns
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(&column.text))
                });
            return Ok(Some(Expr {
                kind: ExprKind::Constant {
                    value: Value::Boolean(updated),
                    sql_type: Some(crate::types::SqlType::Boolean),
                },
                position,
            }));
        }
        let Some(frame) = self.trigger_frame else {
            return Ok(None);
        };
        if (word == "new" || word == "old") && self.peek_kind_at(1) == &TokenKind::Dot {
            let qualifier = word.to_owned();
            self.at += 2;
            let column = self.row_column()?;
            return frame.reference(&qualifier, &column, position).map(Some);
        }
        if !call
            && self.peek_kind_at(1) != &TokenKind::Dot
            && let Some(local) = frame.local(word, position)
        {
            self.at += 1;
            return Ok(Some(local));
        }
        Ok(None)
    }

    fn sqlite_raise(&mut self, position: usize) -> SqlResult<Expr> {
        self.at += 2;
        let kind = match self.peek_kind() {
            TokenKind::Word(word)
                if matches!(word.as_ref(), "ignore" | "rollback" | "abort" | "fail") =>
            {
                word.as_ref().to_owned()
            }
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        let mut arguments = vec![Expr {
            kind: ExprKind::Literal(Literal::String(kind.clone())),
            position,
        }];
        if kind != "ignore" {
            self.expect(&TokenKind::Comma)?;
            arguments.push(self.expression()?);
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(Expr {
            kind: ExprKind::Function {
                name: RAISE_FUNCTION.to_owned(),
                arguments,
            },
            position,
        })
    }

    pub(super) fn transition_table_reference(&mut self) -> SqlResult<Option<TableReference>> {
        let Some(batch) = self.batch else {
            return Ok(None);
        };
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        if !batch
            .transition_tables
            .iter()
            .any(|table| table == word.as_ref())
            || self.peek_kind_at(1) == &TokenKind::Dot
            || self.peek_kind_at(1) == &TokenKind::LeftParen
        {
            return Ok(None);
        }
        let name = Name {
            text: word.as_ref().to_owned(),
            position: self.position(),
        };
        self.at += 1;
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        Ok(Some(TableReference {
            alias: Some(alias.unwrap_or_else(|| name.text.clone())),
            generated: Some(Box::new(GeneratedTable::Variable(name.text.clone()))),
            name,
            derived: None,
        }))
    }
}

struct TriggerParts {
    table: Name,
    timing: TriggerTiming,
    events: TriggerEvents,
    columns: Vec<String>,
    level: TriggerLevel,
    condition: Option<String>,
    old_table: Option<String>,
    new_table: Option<String>,
    action: TriggerAction,
    placement: Option<TriggerPlacement>,
}
