use super::{Compiler, not_supported_yet};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::plpgsql::{
    Block, Branch, CursorDeclaration, Declaration, DeclarationKind, DiagnosticsItem,
    ExceptionCondition, FetchDirection, Fragment, Handler, IntoTargets, OpenSource, Raise,
    RaiseCondition, RaiseItem, RaiseLevel, RaiseOption, Step, StepKind, Target,
    VariableDeclaration, VariableType,
};
use crate::types::SqlType;

const NO_DATA_STATE: &str = "02000";
const NO_DATA_MESSAGE: &str = "'No data - zero rows fetched, selected, or processed'";

#[derive(Debug, Clone)]
enum NamedCondition {
    State(String),
    Errno(u16),
}

#[derive(Debug, Clone)]
struct HandlerSpec {
    exit: bool,
    conditions: Vec<ExceptionCondition>,
    steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub(super) struct HandlerScope {
    label: String,
    conditions: Vec<(String, NamedCondition)>,
    handlers: Vec<HandlerSpec>,
}

impl HandlerScope {
    pub(super) fn new(label: String) -> Self {
        Self {
            label,
            conditions: Vec::new(),
            handlers: Vec::new(),
        }
    }
}

fn specificity(condition: &ExceptionCondition) -> u8 {
    match condition {
        ExceptionCondition::MySqlErrno(_) => 0,
        ExceptionCondition::MySqlState(_) => 1,
        _ => 2,
    }
}

fn bad_state(code: &str, position: usize) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, format!("Bad SQLSTATE: '{code}'"))
        .with_vendor_code(1407)
        .at(position)
}

fn valid_state(code: &str) -> bool {
    code.len() == 5
        && code.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && !code.starts_with("00")
}

fn literal(text: &str, position: usize) -> Fragment {
    Fragment {
        text: text.to_owned(),
        position,
    }
}

fn step(kind: StepKind, position: usize) -> Step {
    Step { kind, position }
}

fn raise_no_data(position: usize) -> Step {
    let state = SqlState::from_text(NO_DATA_STATE).unwrap_or(SqlState::NO_DATA_FOUND);
    step(
        StepKind::Raise(Raise {
            level: RaiseLevel::Exception,
            condition: Some(RaiseCondition::State(state)),
            format: None,
            arguments: Vec::new(),
            options: vec![RaiseOption {
                item: RaiseItem::Message,
                value: literal(NO_DATA_MESSAGE, position),
            }],
        }),
        position,
    )
}

enum DiagnosticsTarget {
    Local(Target),
    User(String, usize),
}

enum DiagnosticsSource {
    Item(DiagnosticsItem),
    Constant(&'static str),
}

impl Compiler<'_, '_> {
    pub(super) fn in_handler(&self) -> bool {
        self.in_handler > 0
    }

    pub(super) fn guard(&self, guarded: Step) -> Step {
        let mut handlers = Vec::new();
        for scope in self.scopes.iter().rev() {
            let mut level: Vec<(u8, Handler)> = Vec::new();
            for spec in &scope.handlers {
                let mut steps = spec.steps.clone();
                if spec.exit {
                    steps.push(step(
                        StepKind::Exit {
                            label: Some(scope.label.clone()),
                            condition: None,
                        },
                        guarded.position,
                    ));
                }
                for condition in &spec.conditions {
                    level.push((
                        specificity(condition),
                        Handler {
                            conditions: vec![condition.clone()],
                            steps: steps.clone(),
                        },
                    ));
                }
            }
            level.sort_by_key(|(rank, _)| *rank);
            handlers.extend(level.into_iter().map(|(_, handler)| handler));
        }
        if handlers.is_empty() {
            return guarded;
        }
        let position = guarded.position;
        step(
            StepKind::Block(Block {
                label: None,
                declarations: Vec::new(),
                steps: vec![guarded],
                handlers,
                position,
            }),
            position,
        )
    }

    fn named_condition(&self, name: &str) -> Option<NamedCondition> {
        self.scopes.iter().rev().find_map(|scope| {
            scope
                .conditions
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, condition)| condition.clone())
        })
    }

    fn state_literal(&mut self) -> SqlResult<String> {
        let position = self.position();
        self.take_word("value");
        let TokenKind::String(code) = self.parser.peek_kind() else {
            return Err(self.parser.unexpected());
        };
        let code = code.to_string();
        if !valid_state(&code) {
            return Err(bad_state(&code, position));
        }
        self.parser.at += 1;
        Ok(code)
    }

    fn error_number(&mut self) -> SqlResult<u16> {
        let position = self.position();
        let TokenKind::Integer(digits) = self.parser.peek_kind() else {
            return Err(self.parser.unexpected());
        };
        let number = digits
            .parse::<u16>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("Incorrect CONDITION value: '{digits}'"),
                )
                .with_vendor_code(1064)
                .at(position)
            })?;
        self.parser.at += 1;
        Ok(number)
    }

    pub(super) fn condition_declaration(&mut self) -> SqlResult<()> {
        let position = self.position();
        let name = self.parser.name()?.text;
        self.expect_word("condition")?;
        self.expect_word("for")?;
        let condition = if self.take_word("sqlstate") {
            NamedCondition::State(self.state_literal()?)
        } else {
            NamedCondition::Errno(self.error_number()?)
        };
        let Some(scope) = self.scopes.last_mut() else {
            return Err(self.parser.unexpected());
        };
        if scope.conditions.iter().any(|(known, _)| *known == name) {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("Duplicate condition: {name}"),
            )
            .with_vendor_code(1332)
            .at(position));
        }
        scope.conditions.push((name, condition));
        Ok(())
    }

    pub(super) fn cursor_declaration(&mut self, position: usize) -> SqlResult<Declaration> {
        let name = self.parser.name()?.text;
        self.expect_word("cursor")?;
        self.expect_word("for")?;
        let query = self.fragment(&[])?;
        Ok(Declaration {
            name,
            position,
            kind: DeclarationKind::Cursor(CursorDeclaration {
                scroll: None,
                arguments: Vec::new(),
                query,
            }),
        })
    }

    fn handler_condition(&mut self) -> SqlResult<ExceptionCondition> {
        let position = self.position();
        if self.take_word("sqlstate") {
            return Ok(ExceptionCondition::MySqlState(self.state_literal()?));
        }
        if matches!(self.parser.peek_kind(), TokenKind::Integer(_)) {
            return Ok(ExceptionCondition::MySqlErrno(self.error_number()?));
        }
        if self.take_word("sqlwarning") {
            return Ok(ExceptionCondition::MySqlWarning);
        }
        if self.take_word("sqlexception") {
            return Ok(ExceptionCondition::MySqlException);
        }
        if self.is_word("not") && self.parser.is_word_at(1, "found") {
            self.parser.at += 2;
            return Ok(ExceptionCondition::MySqlNotFound);
        }
        let name = self.parser.name()?.text;
        match self.named_condition(&name) {
            Some(NamedCondition::State(code)) => Ok(ExceptionCondition::MySqlState(code)),
            Some(NamedCondition::Errno(number)) => Ok(ExceptionCondition::MySqlErrno(number)),
            None => Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("Undefined CONDITION: {name}"),
            )
            .with_vendor_code(1319)
            .at(position)),
        }
    }

    pub(super) fn handler_declaration(&mut self, depth: usize, position: usize) -> SqlResult<()> {
        let exit = if self.take_word("exit") {
            true
        } else if self.take_word("continue") {
            false
        } else {
            return Err(not_supported_yet("UNDO handlers", position));
        };
        self.expect_word("handler")?;
        self.expect_word("for")?;
        let mut conditions = Vec::new();
        loop {
            let condition = self.handler_condition()?;
            if !conditions.contains(&condition) {
                conditions.push(condition);
            }
            if !self.parser.take(&TokenKind::Comma) {
                break;
            }
        }
        let duplicate = self.scopes.last().is_some_and(|scope| {
            scope
                .handlers
                .iter()
                .any(|spec| spec.conditions.iter().any(|known| conditions.contains(known)))
        });
        if duplicate {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                "Duplicate handler declared in the same block",
            )
            .with_vendor_code(1413)
            .at(position));
        }
        let saved = self
            .scopes
            .last_mut()
            .map(|scope| std::mem::take(&mut scope.handlers))
            .unwrap_or_default();
        self.in_handler += 1;
        let body = self.step(depth + 1).map(|body| self.guard(body));
        self.in_handler -= 1;
        if let Some(scope) = self.scopes.last_mut() {
            scope.handlers = saved;
        }
        let body = body?;
        if let Some(scope) = self.scopes.last_mut() {
            scope.handlers.push(HandlerSpec {
                exit,
                conditions,
                steps: vec![body],
            });
        }
        Ok(())
    }

    pub(super) fn open_step(&mut self) -> SqlResult<StepKind> {
        self.expect_word("open")?;
        let cursor = self.parser.name()?.text;
        Ok(StepKind::Open {
            cursor,
            scroll: None,
            source: OpenSource::Bound(Vec::new()),
        })
    }

    pub(super) fn close_step(&mut self) -> SqlResult<StepKind> {
        self.expect_word("close")?;
        Ok(StepKind::Close(self.parser.name()?.text))
    }

    pub(super) fn fetch_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        self.expect_word("fetch")?;
        if self.take_word("next") {
            self.expect_word("from")?;
        } else {
            self.take_word("from");
        }
        let cursor = self.parser.name()?.text;
        self.expect_word("into")?;
        let mut targets = Vec::new();
        loop {
            let at = self.position();
            targets.push(Target {
                name: self.parser.name()?.text,
                path: Vec::new(),
                position: at,
            });
            if !self.parser.take(&TokenKind::Comma) {
                break;
            }
        }
        let fetch = step(
            StepKind::Fetch {
                cursor,
                direction: FetchDirection::Next,
                into: IntoTargets {
                    strict: false,
                    bulk: false,
                    targets,
                },
                limit: None,
            },
            position,
        );
        let no_row = step(
            StepKind::If {
                branches: vec![Branch {
                    condition: literal("NOT found", position),
                    steps: vec![raise_no_data(position)],
                }],
                otherwise: None,
            },
            position,
        );
        Ok(StepKind::Block(Block {
            label: None,
            declarations: Vec::new(),
            steps: vec![fetch, no_row],
            handlers: Vec::new(),
            position,
        }))
    }

    pub(super) fn condition_state(&mut self, name: &str, position: usize) -> SqlResult<String> {
        match self.named_condition(name) {
            Some(NamedCondition::State(code)) => Ok(code),
            Some(NamedCondition::Errno(_)) => Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                "SIGNAL/RESIGNAL can only use a CONDITION defined with SQLSTATE",
            )
            .with_vendor_code(1645)
            .at(position)),
            None => Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("Undefined CONDITION: {name}"),
            )
            .with_vendor_code(1319)
            .at(position)),
        }
    }

    fn diagnostics_target(&mut self) -> SqlResult<DiagnosticsTarget> {
        let position = self.position();
        if let TokenKind::Variable(name) = self.parser.peek_kind() {
            let name = name.to_string();
            self.parser.at += 1;
            return Ok(DiagnosticsTarget::User(name, position));
        }
        Ok(DiagnosticsTarget::Local(Target {
            name: self.parser.name()?.text,
            path: Vec::new(),
            position,
        }))
    }

    fn condition_item(&mut self) -> SqlResult<DiagnosticsSource> {
        let position = self.position();
        let item = self
            .word_text()
            .ok_or_else(|| self.parser.unexpected())?
            .to_ascii_lowercase();
        self.parser.at += 1;
        Ok(match item.as_str() {
            "message_text" => DiagnosticsSource::Item(DiagnosticsItem::MessageText),
            "returned_sqlstate" => DiagnosticsSource::Item(DiagnosticsItem::MySqlReturnedSqlstate),
            "mysql_errno" => DiagnosticsSource::Item(DiagnosticsItem::MySqlErrno),
            "schema_name" => DiagnosticsSource::Item(DiagnosticsItem::SchemaName),
            "table_name" => DiagnosticsSource::Item(DiagnosticsItem::TableName),
            "column_name" => DiagnosticsSource::Item(DiagnosticsItem::ColumnName),
            "constraint_name" => DiagnosticsSource::Item(DiagnosticsItem::ConstraintName),
            "class_origin" | "subclass_origin" => DiagnosticsSource::Constant("'ISO 9075'"),
            "constraint_catalog" | "catalog_name" => DiagnosticsSource::Constant("'def'"),
            "constraint_schema" | "cursor_name" => DiagnosticsSource::Constant("''"),
            _ => {
                return Err(SqlError::syntax(
                    format!("unknown GET DIAGNOSTICS item {item}"),
                    position,
                ));
            }
        })
    }

    pub(super) fn diagnostics_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        self.expect_word("get")?;
        if !self.take_word("stacked") {
            self.take_word("current");
        }
        self.expect_word("diagnostics")?;
        let per_condition = self.take_word("condition");
        if per_condition {
            let number_position = self.position();
            let first = matches!(self.parser.peek_kind(), TokenKind::Integer(digits) if *digits == "1");
            if !first {
                return Err(not_supported_yet(
                    "GET DIAGNOSTICS of a condition other than 1",
                    number_position,
                ));
            }
            self.parser.at += 1;
        }
        let mut pairs = Vec::new();
        loop {
            let target = self.diagnostics_target()?;
            if !self.parser.take_operator("=") {
                return Err(self.parser.unexpected());
            }
            let source = if per_condition {
                self.condition_item()?
            } else {
                let item_position = self.position();
                match self.word_text().map(|word| word.to_ascii_lowercase()).as_deref() {
                    Some("row_count") => {
                        self.parser.at += 1;
                        DiagnosticsSource::Item(DiagnosticsItem::RowCount)
                    }
                    Some("number") => {
                        self.parser.at += 1;
                        DiagnosticsSource::Constant(if self.in_handler() { "1" } else { "0" })
                    }
                    _ => {
                        return Err(SqlError::syntax(
                            "GET DIAGNOSTICS takes NUMBER or ROW_COUNT",
                            item_position,
                        ));
                    }
                }
            };
            pairs.push((target, source));
            if !self.parser.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(StepKind::Block(self.diagnostics_block(pairs, per_condition, position)))
    }

    fn diagnostics_block(
        &mut self,
        pairs: Vec<(DiagnosticsTarget, DiagnosticsSource)>,
        stacked: bool,
        position: usize,
    ) -> Block {
        let mut declarations = Vec::new();
        let mut items = Vec::new();
        let mut after = Vec::new();
        for (target, source) in pairs {
            let local = match target {
                DiagnosticsTarget::Local(target) => target,
                DiagnosticsTarget::User(name, at) => {
                    self.generated_labels += 1;
                    let hidden = format!("mysql_diagnostics_{}", self.generated_labels);
                    declarations.push(Declaration {
                        name: hidden.clone(),
                        position: at,
                        kind: DeclarationKind::Variable(VariableDeclaration {
                            variable_type: VariableType::Sql(SqlType::Text),
                            constant: false,
                            not_null: false,
                            collation: None,
                            default: None,
                        }),
                    });
                    after.push(step(
                        StepKind::Sql {
                            statement: literal(&format!("SET {name} = {hidden}"), at),
                            into: None,
                        },
                        at,
                    ));
                    Target {
                        name: hidden,
                        path: Vec::new(),
                        position: at,
                    }
                }
            };
            match source {
                DiagnosticsSource::Item(item) => items.push((local, item)),
                DiagnosticsSource::Constant(value) => {
                    let at = local.position;
                    after.insert(
                        0,
                        step(
                            StepKind::Assign {
                                target: local,
                                value: literal(value, at),
                            },
                            at,
                        ),
                    );
                }
            }
        }
        let mut steps = Vec::new();
        if !items.is_empty() {
            steps.push(step(StepKind::GetDiagnostics { stacked, items }, position));
        }
        steps.extend(after);
        Block {
            label: None,
            declarations,
            steps,
            handlers: Vec::new(),
            position,
        }
    }
}
