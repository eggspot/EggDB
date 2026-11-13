use super::{Parameters, Parser};
use crate::ast::{
    CreateTable, CreateTableAs, CreateTableAsSpelling, CreateTableLike, ExecutePrepared, Name,
    OnDuplicate, PrepareStatement, PreparedCommand, Statement,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn at_create_table_as(&self) -> bool {
        if self.sqlserver() {
            return false;
        }
        if self.is_word("as") || self.at_postgresql_table_as_option(0) {
            return true;
        }
        if self.mysql() {
            return self.at_mysql_table_query();
        }
        (self.postgresql() || self.oracle()) && self.at_column_names_then_as()
    }

    pub(super) fn at_mysql_table_query(&self) -> bool {
        self.is_word("as")
            || ((self.is_word("ignore") || self.is_word("replace"))
                && (self.is_word_at(1, "as")
                    || self.is_word_at(1, "select")
                    || self.is_word_at(1, "with")
                    || self.peek_kind_at(1) == &TokenKind::LeftParen))
            || self.starts_query()
            || self.starts_query_in_parentheses()
    }

    fn at_column_names_then_as(&self) -> bool {
        if self.peek_kind() != &TokenKind::LeftParen {
            return false;
        }
        let mut ahead = 1;
        loop {
            if !matches!(
                self.peek_kind_at(ahead),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            ) {
                return false;
            }
            match self.peek_kind_at(ahead + 1) {
                TokenKind::Comma => ahead += 2,
                TokenKind::RightParen => {
                    return self.is_word_at(ahead + 2, "as")
                        || self.at_postgresql_table_as_option(ahead + 2);
                }
                _ => return false,
            }
            if ahead > crate::encoding::MAX_COLUMNS * 2 {
                return false;
            }
        }
    }

    fn at_postgresql_table_as_option(&self, ahead: usize) -> bool {
        self.postgresql()
            && (self.is_word_at(ahead, "using")
                || self.is_word_at(ahead, "tablespace")
                || (self.is_word_at(ahead, "on") && self.is_word_at(ahead + 1, "commit"))
                || (self.is_word_at(ahead, "without") && self.is_word_at(ahead + 1, "oids"))
                || (self.is_word_at(ahead, "with")
                    && self.peek_kind_at(ahead + 1) == &TokenKind::LeftParen))
    }

    pub(super) fn create_table_as(
        &mut self,
        name: Name,
        temporary: bool,
        if_not_exists: bool,
        declared: Option<CreateTable>,
    ) -> SqlResult<Statement> {
        let column_names =
            if declared.is_none() && !self.mysql() && self.peek_kind() == &TokenKind::LeftParen {
                self.name_list()?
            } else {
                Vec::new()
            };
        if self.oracle() {
            self.skip_physical_properties()?;
        }
        let mut on_duplicate = OnDuplicate::Fail;
        let mut on_commit = None;
        if self.mysql() {
            if self.take_word("ignore") {
                on_duplicate = OnDuplicate::Ignore;
            } else if self.take_word("replace") {
                on_duplicate = OnDuplicate::Replace;
            }
            self.take_word("as");
        } else {
            if self.postgresql() {
                self.table_access_method()?;
                if self.storage_parameters()? {
                    return Err(self.not_supported("CREATE TABLE ... AS with WITH (storage = lsm)"));
                }
            }
            on_commit = self.on_commit_clause(temporary)?;
            if self.postgresql() {
                self.table_space()?;
            }
            self.expect_word("as")?;
        }
        let query = if self.postgresql() && self.is_word("execute") {
            Statement::Prepared(PreparedCommand::Execute(self.execute_prepared()?))
        } else {
            self.table_query_statement(column_names.len())?
        };
        let mut with_data = true;
        if self.postgresql()
            && self.is_word("with")
            && (self.is_word_at(1, "data")
                || (self.is_word_at(1, "no") && self.is_word_at(2, "data")))
        {
            self.at += 1;
            with_data = !self.take_word("no");
            self.expect_word("data")?;
        }
        Ok(Statement::CreateTableAs(Box::new(CreateTableAs {
            name,
            temporary,
            query,
            spelling: CreateTableAsSpelling::CreateTableAs,
            if_not_exists,
            column_names,
            declared: declared.map(Box::new),
            with_data,
            on_duplicate,
            on_commit,
        })))
    }

    fn table_query_statement(&mut self, names_given: usize) -> SqlResult<Statement> {
        if !self.starts_query() && !self.starts_query_in_parentheses() {
            return Err(self.unexpected());
        }
        let position = self.position();
        self.oracle_unaliased_expressions.clear();
        let query = self.query_statement()?;
        if self.oracle()
            && let Some(unnamed) = crate::create_table_as::unnamed_oracle_expression(
                &query,
                names_given,
                &self.oracle_unaliased_expressions,
            )
        {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                "must name this expression with a column alias",
            )
            .with_oracle_code(998)
            .at(unnamed));
        }
        if !matches!(query, Statement::Select(_) | Statement::Query(_)) {
            return Err(self
                .not_supported("a data-modifying statement in CREATE TABLE AS")
                .at(position));
        }
        Ok(query)
    }

    pub(super) fn at_create_table_like(&self) -> bool {
        self.mysql()
            && (self.is_word("like")
                || (self.peek_kind() == &TokenKind::LeftParen && self.is_word_at(1, "like")))
    }

    pub(super) fn create_table_like(
        &mut self,
        name: Name,
        temporary: bool,
        if_not_exists: bool,
    ) -> SqlResult<Statement> {
        let parenthesized = self.take(&TokenKind::LeftParen);
        self.expect_word("like")?;
        let source = self.table_name()?;
        if parenthesized {
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(Statement::CreateTableLike(Box::new(CreateTableLike {
            name,
            temporary,
            if_not_exists,
            source,
        })))
    }

    pub(super) fn prepare_statement(&mut self) -> SqlResult<Statement> {
        self.expect_word("prepare")?;
        let name = self.name()?;
        let mut parameter_types = Vec::new();
        if self.take(&TokenKind::LeftParen) {
            loop {
                parameter_types.push(self.sql_type()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        self.expect_word("as")?;
        let preparable = [
            "select", "with", "insert", "update", "delete", "merge", "values",
        ]
        .iter()
        .any(|word| self.is_word(word))
            || self.at_table_query_at(0)
            || self.starts_query_in_parentheses();
        if !preparable {
            return Err(self.unexpected());
        }
        let start = self.position();
        let outer = self.parameters;
        self.parameters = Parameters::Types(&[]);
        let parsed = self.statement_body();
        self.parameters = outer;
        parsed?;
        Ok(Statement::Prepared(PreparedCommand::Prepare(
            PrepareStatement {
                name,
                parameter_types,
                text: self.slice(start, self.previous_end()),
            },
        )))
    }

    pub(super) fn execute_prepared(&mut self) -> SqlResult<ExecutePrepared> {
        self.expect_word("execute")?;
        let name = self.name()?;
        let mut arguments = Vec::new();
        if self.take(&TokenKind::LeftParen) {
            loop {
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(ExecutePrepared { name, arguments })
    }

    pub(super) fn table_access_method(&mut self) -> SqlResult<()> {
        if !self.take_word("using") {
            return Ok(());
        }
        let method = self.name()?;
        if method.text == "heap" {
            return Ok(());
        }
        let error = if INDEX_ACCESS_METHODS.contains(&method.text.as_str()) {
            SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("access method \"{}\" is not of type TABLE", method.text),
            )
        } else {
            SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("access method \"{}\" does not exist", method.text),
            )
        };
        Err(error.at(method.position))
    }

    pub(super) fn table_space(&mut self) -> SqlResult<()> {
        if !self.take_word("tablespace") {
            return Ok(());
        }
        let space = self.name()?;
        let error = match space.text.as_str() {
            "pg_default" => return Ok(()),
            "pg_global" => SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "only shared relations can be placed in pg_global tablespace",
            ),
            other => SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("tablespace \"{other}\" does not exist"),
            ),
        };
        Err(error.at(space.position))
    }

    pub(super) fn storage_parameters(&mut self) -> SqlResult<bool> {
        if self.is_word("without") && self.is_word_at(1, "oids") {
            self.at += 2;
            return Ok(false);
        }
        if !(self.is_word("with") && self.peek_kind_at(1) == &TokenKind::LeftParen) {
            return Ok(false);
        }
        self.at += 2;
        let mut seen: Vec<String> = Vec::new();
        let mut lsm_storage = false;
        loop {
            let position = self.position();
            let first = self.name()?.text;
            let (namespace, name) = if self.take(&TokenKind::Dot) {
                (Some(first), self.name()?.text)
            } else {
                (None, first)
            };
            let value = if self.take_operator("=") {
                Some(self.storage_parameter_value()?)
            } else {
                None
            };
            if namespace.is_none() && name == "storage" {
                lsm_storage = table_storage(value.as_deref(), &mut seen).map_err(|error| error.at(position))?;
            } else {
                check_storage_parameter(namespace.as_deref(), &name, value.as_deref(), &mut seen)
                    .map_err(|error| error.at(position))?;
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(lsm_storage)
    }

    fn storage_parameter_value(&mut self) -> SqlResult<String> {
        let sign = if self.take_operator("-") {
            "-"
        } else {
            self.take_operator("+");
            ""
        };
        let text = match self.peek_kind() {
            TokenKind::Integer(text) | TokenKind::Decimal(text) => (*text).to_owned(),
            TokenKind::String(text) if sign.is_empty() => text.to_string(),
            TokenKind::Word(word) if sign.is_empty() => word.to_string(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(format!("{sign}{text}"))
    }
}

const INDEX_ACCESS_METHODS: [&str; 6] = ["btree", "hash", "gist", "gin", "spgist", "brin"];
const MAX_INT_OPTION: i64 = i32::MAX as i64;

#[derive(Clone, Copy)]
enum StorageOption {
    Boolean,
    Integer(i64, i64),
    Real(f64, f64),
    IndexCleanup,
}

fn heap_storage_option(name: &str, toast: bool) -> Option<StorageOption> {
    let option = match name {
        "autovacuum_enabled" | "vacuum_truncate" => StorageOption::Boolean,
        "vacuum_index_cleanup" => StorageOption::IndexCleanup,
        "autovacuum_vacuum_threshold" => StorageOption::Integer(0, MAX_INT_OPTION),
        "autovacuum_vacuum_insert_threshold" | "log_autovacuum_min_duration" => {
            StorageOption::Integer(-1, MAX_INT_OPTION)
        }
        "autovacuum_vacuum_cost_limit" => StorageOption::Integer(1, 10_000),
        "autovacuum_freeze_min_age" | "autovacuum_multixact_freeze_min_age" => {
            StorageOption::Integer(0, 1_000_000_000)
        }
        "autovacuum_freeze_max_age" => StorageOption::Integer(100_000, 2_000_000_000),
        "autovacuum_multixact_freeze_max_age" => StorageOption::Integer(10_000, 2_000_000_000),
        "autovacuum_freeze_table_age" | "autovacuum_multixact_freeze_table_age" => {
            StorageOption::Integer(0, 2_000_000_000)
        }
        "autovacuum_vacuum_cost_delay"
        | "autovacuum_vacuum_scale_factor"
        | "autovacuum_vacuum_insert_scale_factor" => StorageOption::Real(0.0, 100.0),
        _ if toast => return None,
        "fillfactor" => StorageOption::Integer(10, 100),
        "autovacuum_analyze_threshold" => StorageOption::Integer(0, MAX_INT_OPTION),
        "autovacuum_analyze_scale_factor" => StorageOption::Real(0.0, 100.0),
        "toast_tuple_target" => StorageOption::Integer(128, 8160),
        "parallel_workers" => StorageOption::Integer(0, 1024),
        "user_catalog_table" => StorageOption::Boolean,
        _ => return None,
    };
    Some(option)
}

fn option_boolean(text: &str) -> Option<bool> {
    let text = text.trim().to_ascii_lowercase();
    match text.as_str() {
        "" => None,
        "1" => Some(true),
        "0" => Some(false),
        "on" => Some(true),
        "of" | "off" => Some(false),
        _ if "true".starts_with(text.as_str()) || "yes".starts_with(text.as_str()) => Some(true),
        _ if "false".starts_with(text.as_str()) || "no".starts_with(text.as_str()) => Some(false),
        _ => None,
    }
}

fn invalid_option(message: String) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn table_storage(value: Option<&str>, seen: &mut Vec<String>) -> SqlResult<bool> {
    if seen.iter().any(|name| name == "storage") {
        return Err(invalid_option("parameter \"storage\" specified more than once".to_owned()));
    }
    seen.push("storage".to_owned());
    match value.map(str::to_ascii_lowercase).as_deref() {
        Some("lsm") => Ok(true),
        Some("btree" | "heap") => Ok(false),
        _ => Err(invalid_option(format!(
            "invalid value for parameter \"storage\": \"{}\"; EggDB stores a table in a B-tree (btree, the default) or an LSM tree (lsm)",
            value.unwrap_or_default()
        ))),
    }
}

fn check_storage_parameter(
    namespace: Option<&str>,
    name: &str,
    value: Option<&str>,
    seen: &mut Vec<String>,
) -> SqlResult<()> {
    let toast = match namespace {
        None => false,
        Some("toast") => true,
        Some(other) => {
            return Err(invalid_option(format!(
                "unrecognized parameter namespace \"{other}\""
            )));
        }
    };
    let value = value.unwrap_or("true");
    if !toast && name == "oids" {
        return match option_boolean(value) {
            Some(false) => Ok(()),
            Some(true) => Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "tables declared WITH OIDS are not supported",
            )),
            None => Err(invalid_option(format!(
                "invalid value for boolean option \"oids\": {value}"
            ))),
        };
    }
    let Some(option) = heap_storage_option(name, toast) else {
        return Err(invalid_option(format!("unrecognized parameter \"{name}\"")));
    };
    let key = if toast {
        format!("toast.{name}")
    } else {
        name.to_owned()
    };
    if seen.contains(&key) {
        return Err(invalid_option(format!(
            "parameter \"{name}\" specified more than once"
        )));
    }
    seen.push(key);
    match option {
        StorageOption::Boolean => option_boolean(value).map(drop).ok_or_else(|| {
            invalid_option(format!(
                "invalid value for boolean option \"{name}\": {value}"
            ))
        }),
        StorageOption::IndexCleanup => {
            if value.trim().eq_ignore_ascii_case("auto") || option_boolean(value).is_some() {
                return Ok(());
            }
            Err(
                invalid_option(format!("invalid value for enum option \"{name}\": {value}"))
                    .with_detail("Valid values are \"on\", \"off\", and \"auto\"."),
            )
        }
        StorageOption::Integer(lowest, highest) => {
            let number = value
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|number| (i64::from(i32::MIN)..=MAX_INT_OPTION).contains(number))
                .ok_or_else(|| {
                    invalid_option(format!(
                        "invalid value for integer option \"{name}\": {value}"
                    ))
                })?;
            if (lowest..=highest).contains(&number) {
                return Ok(());
            }
            Err(
                invalid_option(format!("value {value} out of bounds for option \"{name}\""))
                    .with_detail(format!(
                        "Valid values are between \"{lowest}\" and \"{highest}\"."
                    )),
            )
        }
        StorageOption::Real(lowest, highest) => {
            let number = value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|number| number.is_finite())
                .ok_or_else(|| {
                    invalid_option(format!(
                        "invalid value for floating point option \"{name}\": {value}"
                    ))
                })?;
            if (lowest..=highest).contains(&number) {
                return Ok(());
            }
            Err(
                invalid_option(format!("value {value} out of bounds for option \"{name}\""))
                    .with_detail(format!(
                        "Valid values are between \"{lowest:.6}\" and \"{highest:.6}\"."
                    )),
            )
        }
    }
}
