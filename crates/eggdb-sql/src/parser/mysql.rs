use super::{Level, Parser};
use crate::ast::{
    AssignedValue, Assignment, BinaryOperator, ColumnDefinition, Expr, ExprKind, IndexSpec,
    Literal, MySqlStatement, Name, OnDuplicate, Statement, UnaryOperator, VariableTarget,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::mysql_variables::MYSQL_COMPATIBLE_VERSION;
use crate::settings::Settings;
use crate::types::{SqlType, Value};

pub(super) fn is_reserved(word: &str) -> bool {
    matches!(
        word,
        "add"
            | "all"
            | "alter"
            | "and"
            | "as"
            | "asc"
            | "between"
            | "both"
            | "by"
            | "case"
            | "character"
            | "check"
            | "collate"
            | "column"
            | "constraint"
            | "create"
            | "cross"
            | "current_date"
            | "current_time"
            | "current_timestamp"
            | "current_user"
            | "database"
            | "databases"
            | "default"
            | "delete"
            | "desc"
            | "describe"
            | "distinct"
            | "div"
            | "drop"
            | "dual"
            | "else"
            | "exists"
            | "explain"
            | "false"
            | "for"
            | "foreign"
            | "from"
            | "group"
            | "having"
            | "if"
            | "ignore"
            | "in"
            | "index"
            | "inner"
            | "insert"
            | "interval"
            | "into"
            | "is"
            | "join"
            | "key"
            | "keys"
            | "leading"
            | "left"
            | "like"
            | "limit"
            | "localtime"
            | "localtimestamp"
            | "lock"
            | "mod"
            | "natural"
            | "not"
            | "null"
            | "on"
            | "or"
            | "order"
            | "outer"
            | "primary"
            | "references"
            | "replace"
            | "right"
            | "schema"
            | "select"
            | "set"
            | "show"
            | "table"
            | "then"
            | "to"
            | "trailing"
            | "true"
            | "union"
            | "unique"
            | "unlock"
            | "unsigned"
            | "update"
            | "use"
            | "using"
            | "values"
            | "when"
            | "where"
            | "with"
            | "xor"
            | "zerofill"
    )
}

pub(super) fn is_clause_word(word: &str) -> bool {
    matches!(
        word,
        "div" | "mod" | "xor" | "regexp" | "rlike" | "duplicate"
    )
}

fn variable_name(name: &str) -> &str {
    name.trim_start_matches('@')
}

impl<'t> Parser<'t> {
    fn settings_or_default<T>(&self, read: impl FnOnce(&Settings) -> T) -> T {
        match self.settings {
            Some(settings) => read(settings),
            None => read(&Settings::default()),
        }
    }

    pub(super) fn check_database(&self, name: &Name) -> SqlResult<()> {
        let database = self.settings_or_default(|settings| settings.database.clone());
        if name.text == database.to_lowercase() || name.text == database {
            return Ok(());
        }
        Err(SqlError::new(
            SqlState::INVALID_CATALOG_NAME,
            format!("database \"{}\" does not exist", name.text),
        )
        .at(name.position))
    }

    fn word_text(&self) -> Option<String> {
        match self.peek_kind() {
            TokenKind::Word(word) => Some(word.as_ref().to_owned()),
            TokenKind::QuotedIdentifier(name) => Some(name.clone()),
            TokenKind::String(text) => Some(text.as_ref().to_owned()),
            _ => None,
        }
    }

    fn like_pattern(&mut self) -> SqlResult<Option<String>> {
        if !self.take_word("like") {
            if self.is_word("where") {
                return Err(self.not_supported("SHOW ... WHERE"));
            }
            return Ok(None);
        }
        match self.peek_kind() {
            TokenKind::String(text) => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                Ok(Some(text))
            }
            _ => Err(self.unexpected()),
        }
    }

    fn skip_database_qualifier(&mut self) -> SqlResult<()> {
        if self.take_word("from") || self.take_word("in") {
            let database = self.name()?;
            self.check_database(&database)?;
        }
        Ok(())
    }

    pub(super) fn mysql_statement(&mut self) -> SqlResult<Option<Statement>> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        let statement = match word.as_ref() {
            "show" => self.mysql_show()?,
            "describe" | "desc" => {
                self.at += 1;
                self.describe()?
            }
            "explain"
                if !self.is_word_at(1, "select")
                    && !self.is_word_at(1, "update")
                    && !self.is_word_at(1, "delete") =>
            {
                self.at += 1;
                self.describe()?
            }
            "use" => {
                self.at += 1;
                Statement::MySql(MySqlStatement::Use(self.name()?))
            }
            "set" => self.mysql_set()?,
            "replace" => self.insert()?,
            "lock" if self.is_word_at(1, "tables") || self.is_word_at(1, "table") => {
                self.at += 2;
                loop {
                    self.table_name()?;
                    let lock_word = ["read", "write", "low_priority"]
                        .iter()
                        .any(|word| self.is_word(word));
                    if self.take_word("as") || !lock_word && self.at_bare_alias() {
                        self.name()?;
                    }
                    if !self.take_word("read") {
                        self.take_word("low_priority");
                        self.expect_word("write")?;
                    } else {
                        self.take_word("local");
                    }
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                Statement::MySql(MySqlStatement::Accepted("LOCK TABLES"))
            }
            "unlock" if self.is_word_at(1, "tables") || self.is_word_at(1, "table") => {
                self.at += 2;
                Statement::MySql(MySqlStatement::Accepted("UNLOCK TABLES"))
            }
            "create" | "drop" if self.is_word_at(1, "database") || self.is_word_at(1, "schema") => {
                return Err(self.not_supported(
                    "CREATE DATABASE and DROP DATABASE (a server serves one database)",
                ));
            }
            _ => return Ok(None),
        };
        Ok(Some(statement))
    }

    fn describe(&mut self) -> SqlResult<Statement> {
        let table = self.table_name()?;
        let like = match self.peek_kind() {
            TokenKind::String(text) => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                Some(text)
            }
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) => Some(self.name()?.text),
            _ => None,
        };
        Ok(Statement::MySql(MySqlStatement::ShowColumns {
            table,
            full: false,
            like,
        }))
    }

    fn mysql_show(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let full = self.take_word("full");
        let statement = if self.take_word("tables") {
            self.skip_database_qualifier()?;
            MySqlStatement::ShowTables {
                full,
                like: self.like_pattern()?,
            }
        } else if self.take_word("databases") || self.take_word("schemas") {
            MySqlStatement::ShowDatabases {
                like: self.like_pattern()?,
            }
        } else if self.take_word("columns") || self.take_word("fields") {
            if !(self.take_word("from") || self.take_word("in")) {
                return Err(self.unexpected());
            }
            let table = self.table_name()?;
            self.skip_database_qualifier()?;
            MySqlStatement::ShowColumns {
                table,
                full,
                like: self.like_pattern()?,
            }
        } else if self.take_word("index") || self.take_word("indexes") || self.take_word("keys") {
            if !(self.take_word("from") || self.take_word("in")) {
                return Err(self.unexpected());
            }
            let table = self.table_name()?;
            self.skip_database_qualifier()?;
            MySqlStatement::ShowIndex(table)
        } else if self.is_word("create") && self.is_word_at(1, "table") {
            self.at += 2;
            MySqlStatement::ShowCreateTable(self.table_name()?)
        } else if self.is_word("create") && self.is_word_at(1, "view") {
            self.at += 2;
            MySqlStatement::ShowCreateView(self.table_name()?)
        } else if self.take_word("warnings") || self.take_word("errors") {
            let errors_only = self.tokens[self.at - 1].kind == TokenKind::Word("errors".into());
            if self.take_word("limit") {
                self.expression()?;
                if self.take(&TokenKind::Comma) {
                    self.expression()?;
                }
            }
            MySqlStatement::ShowWarnings { errors_only }
        } else if self.is_word("count") && self.peek_kind_at(1) == &TokenKind::LeftParen {
            self.at += 2;
            if !self.take_operator("*") {
                return Err(self.unexpected());
            }
            self.expect(&TokenKind::RightParen)?;
            let errors_only = if self.take_word("errors") {
                true
            } else {
                self.expect_word("warnings")?;
                false
            };
            MySqlStatement::ShowCount { errors_only }
        } else {
            let _ = self.take_word("session") || self.take_word("local");
            if self.is_word("global") {
                return Err(self.not_supported("SHOW GLOBAL"));
            }
            if self.take_word("variables") {
                MySqlStatement::ShowVariables {
                    like: self.like_pattern()?,
                }
            } else if self.take_word("status") {
                self.like_pattern()?;
                MySqlStatement::ShowStatus
            } else {
                let what = self.word_text().unwrap_or_default().to_ascii_uppercase();
                return Err(self.not_supported(&format!("SHOW {what}")));
            }
        };
        Ok(Statement::MySql(statement))
    }

    fn mysql_set(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("names") || {
            let character_set = self.is_word("character") && self.is_word_at(1, "set");
            if character_set {
                self.at += 2;
            }
            character_set || self.take_word("charset")
        } {
            let value = if self.take_word("default") {
                AssignedValue::Default
            } else {
                let value = self.word_text().ok_or_else(|| self.unexpected())?;
                self.at += 1;
                AssignedValue::Word(value)
            };
            if self.take_word("collate") {
                self.word_text().ok_or_else(|| self.unexpected())?;
                self.at += 1;
            }
            let assignments = [
                "character_set_client",
                "character_set_connection",
                "character_set_results",
            ]
            .into_iter()
            .map(|name| Assignment {
                target: VariableTarget::System(name.to_owned()),
                value: value.clone(),
            })
            .collect();
            return Ok(Statement::MySql(MySqlStatement::Set(assignments)));
        }
        let mut assignments = Vec::new();
        loop {
            let mut global = self.is_word("global") || self.is_word("persist");
            let _ = self.take_word("session")
                || self.take_word("local")
                || self.take_word("global")
                || self.take_word("persist");
            if self.take_word("transaction") {
                while !matches!(self.peek_kind(), TokenKind::End | TokenKind::Semicolon) {
                    self.at += 1;
                }
                return Ok(Statement::MySql(MySqlStatement::Accepted(
                    "SET TRANSACTION",
                )));
            }
            if self.is_word("password") || self.is_word("role") || self.is_word("default") {
                return Err(self.not_supported(&format!(
                    "SET {}",
                    self.word_text().unwrap_or_default().to_ascii_uppercase()
                )));
            }
            let target = match self.peek_kind() {
                TokenKind::Variable(name) if name.starts_with("@@") => {
                    let mut name = variable_name(name).to_owned();
                    self.at += 1;
                    if matches!(name.as_str(), "session" | "local" | "global" | "persist")
                        && self.take(&TokenKind::Dot)
                    {
                        global |= name == "global" || name == "persist";
                        name = self.word_text().ok_or_else(|| self.unexpected())?;
                        self.at += 1;
                    }
                    VariableTarget::System(name)
                }
                TokenKind::Variable(name) => {
                    let name = variable_name(name).to_owned();
                    self.at += 1;
                    VariableTarget::User(name)
                }
                _ => {
                    let name = self.word_text().ok_or_else(|| self.unexpected())?;
                    self.at += 1;
                    VariableTarget::System(name)
                }
            };
            if global {
                return Err(self.not_supported("SET GLOBAL"));
            }
            if !self.take_operator("=") && !self.take_operator(":=") {
                return Err(self.unexpected());
            }
            let value = self.assigned_value(&target)?;
            assignments.push(Assignment { target, value });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::MySql(MySqlStatement::Set(assignments)))
    }

    fn assigned_value(&mut self, target: &VariableTarget) -> SqlResult<AssignedValue> {
        let ends_here = matches!(
            self.peek_kind_at(1),
            TokenKind::Comma | TokenKind::Semicolon | TokenKind::End
        );
        if matches!(target, VariableTarget::System(_)) && ends_here {
            match self.peek_kind() {
                TokenKind::Word(word) if word == "default" => {
                    self.at += 1;
                    return Ok(AssignedValue::Default);
                }
                TokenKind::Word(word) if !matches!(word.as_ref(), "null" | "true" | "false") => {
                    let word = word.as_ref().to_owned();
                    self.at += 1;
                    return Ok(AssignedValue::Word(word));
                }
                _ => {}
            }
        }
        Ok(AssignedValue::Expr(self.expression()?))
    }

    pub(super) fn mysql_type(&mut self) -> SqlResult<SqlType> {
        let token = self.peek();
        let word: &str = match &token.kind {
            TokenKind::Word(word) => word,
            TokenKind::QuotedIdentifier(name) => name,
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        if ["decimal", "dec", "numeric", "fixed"].contains(&word) {
            let sql_type = self.mysql_decimal(token.start)?;
            return self.integer_attributes(sql_type);
        }
        let width = self.optional_width()?;
        let sql_type = match word {
            "tinyint" if width == Some(1) => SqlType::Boolean,
            "tinyint" | "smallint" => SqlType::SmallInt,
            "mediumint" | "int" | "integer" => SqlType::Integer,
            "bigint" => SqlType::BigInt,
            "bool" | "boolean" => SqlType::Boolean,
            "float" if width.is_some_and(|bits| bits > 24) => SqlType::Double,
            "float" => SqlType::Real,
            "double" => {
                self.take_word("precision");
                self.optional_width()?;
                SqlType::Double
            }
            "real" => SqlType::Double,
            "date" => SqlType::Date,
            "datetime" => SqlType::Timestamp(Some(self.fractional_digits(width, token.start)?)),
            "timestamp" => SqlType::TimestampTz(Some(self.fractional_digits(width, token.start)?)),
            "json" => SqlType::Jsonb,
            "varchar" => SqlType::Varchar(Some(self.required_length(width, token.start)?)),
            "char" | "character" => {
                if self.take_word("varying") {
                    let length = self.optional_width()?;
                    SqlType::Varchar(Some(self.required_length(length, token.start)?))
                } else {
                    SqlType::Varchar(Some(
                        width.map_or(Ok(1), |length| self.length(length, token.start))?,
                    ))
                }
            }
            "text" | "tinytext" | "mediumtext" | "longtext" => SqlType::Text,
            "time" => SqlType::Time(Some(self.fractional_digits(width, token.start)?)),
            "blob" | "tinyblob" | "mediumblob" | "longblob" => SqlType::Bytea,
            "binary" => {
                width.map_or(Ok(1), |length| self.length(length, token.start))?;
                SqlType::Bytea
            }
            "varbinary" => {
                self.required_length(width, token.start)?;
                SqlType::Bytea
            }
            "year" | "bit" | "enum" | "set" | "geometry" | "point" => {
                return Err(SqlError::not_supported(format!(
                    "type {} is not supported yet",
                    word.to_ascii_uppercase()
                ))
                .at(token.start));
            }
            _ => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("type \"{word}\" does not exist"),
                )
                .at(token.start));
            }
        };
        let sql_type = self.integer_attributes(sql_type)?;
        if (self.is_word("character") && self.is_word_at(1, "set")) || self.is_word("charset") {
            self.at += if self.is_word("charset") { 1 } else { 2 };
            self.word_text().ok_or_else(|| self.unexpected())?;
            self.at += 1;
        }
        if self.take_word("collate") {
            self.word_text().ok_or_else(|| self.unexpected())?;
            self.at += 1;
        }
        Ok(sql_type)
    }

    fn mysql_decimal(&mut self, position: usize) -> SqlResult<SqlType> {
        let mut precision = 10;
        let mut scale = 0;
        if self.take(&TokenKind::LeftParen) {
            let TokenKind::Integer(digits) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            precision = digits.parse().unwrap_or(i64::MAX);
            self.at += 1;
            if self.take(&TokenKind::Comma) {
                let TokenKind::Integer(digits) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                scale = digits.parse().unwrap_or(i64::MAX);
                self.at += 1;
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if precision > 65 {
            return Err(SqlError::new(
                SqlState::MYSQL_SYNTAX_OR_ACCESS,
                format!("Too-big precision {precision} specified. Maximum is 65."),
            )
            .with_vendor_code(1426)
            .at(position));
        }
        if scale > 30 || scale > precision {
            return Err(SqlError::new(
                SqlState::MYSQL_SYNTAX_OR_ACCESS,
                format!("Too big scale {scale} specified. Maximum is 30."),
            )
            .with_vendor_code(1425)
            .at(position));
        }
        let precision = precision.max(1);
        Ok(SqlType::Numeric(Some(crate::types::NumericModifier {
            precision: precision as u16,
            scale: scale as i16,
        })))
    }

    fn fractional_digits(&self, width: Option<i64>, position: usize) -> SqlResult<u8> {
        match width {
            None => Ok(0),
            Some(digits @ 0..=6) => Ok(digits as u8),
            Some(digits) => Err(SqlError::new(
                SqlState::MYSQL_SYNTAX_OR_ACCESS,
                format!("Too-big precision {digits} specified. Maximum is 6."),
            )
            .with_vendor_code(1426)
            .at(position)),
        }
    }

    fn integer_attributes(&mut self, mut sql_type: SqlType) -> SqlResult<SqlType> {
        loop {
            if self.take_word("unsigned") {
                sql_type = match sql_type {
                    SqlType::SmallInt => SqlType::Integer,
                    SqlType::Integer => SqlType::BigInt,
                    SqlType::BigInt => {
                        return Err(self.not_supported("BIGINT UNSIGNED"));
                    }
                    other => other,
                };
            } else if !(self.take_word("signed") || self.take_word("zerofill")) {
                return Ok(sql_type);
            }
        }
    }

    fn optional_width(&mut self) -> SqlResult<Option<i64>> {
        if self.peek_kind() != &TokenKind::LeftParen {
            return Ok(None);
        }
        self.at += 1;
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let width = digits.parse().unwrap_or(i64::MAX);
        self.at += 1;
        if self.take(&TokenKind::Comma) {
            let TokenKind::Integer(_) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            self.at += 1;
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(width))
    }

    fn length(&self, length: i64, position: usize) -> SqlResult<u32> {
        match length {
            1..=16_383 => Ok(length as u32),
            0 => Err(self
                .not_supported("a string column of length 0")
                .at(position)),
            _ => Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("Column length too big (max = 16383); use BLOB or TEXT instead: {length}"),
            )
            .at(position)),
        }
    }

    fn required_length(&self, length: Option<i64>, position: usize) -> SqlResult<u32> {
        match length {
            Some(length) => self.length(length, position),
            None => Err(SqlError::syntax(
                "VARCHAR needs a length, such as VARCHAR(255)",
                position,
            )),
        }
    }

    pub(super) fn mysql_column_option(&mut self, column: &mut ColumnDefinition) -> SqlResult<bool> {
        if self.take_word("auto_increment") {
            column.auto_increment = true;
        } else if self.take_word("comment") {
            let TokenKind::String(text) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            column.comment = Some(text.as_ref().to_owned());
            self.at += 1;
        } else if self.is_word("key") {
            self.at += 1;
            column.primary_key = true;
        } else if self.take_word("unique") {
            self.take_word("key");
            column.unique = Some(None);
        } else if self.is_word("on") && self.is_word_at(1, "update") {
            return Err(self.not_supported("ON UPDATE"));
        } else if self.is_word("default") {
            self.at += 1;
            let start = self.position();
            let expr = self.binary_operand()?;
            let source = postgresql_source(&expr).ok_or_else(|| {
                SqlError::not_supported("a DEFAULT other than a constant").at(start)
            })?;
            column.default = Some((expr, source));
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) fn mysql_index_entry(&mut self) -> SqlResult<Option<IndexSpec>> {
        if !(self.is_word("key") || self.is_word("index")) {
            if self.is_word("fulltext") || self.is_word("spatial") {
                return Err(self.not_supported("FULLTEXT and SPATIAL indexes"));
            }
            return Ok(None);
        }
        let position = self.position();
        self.at += 1;
        let name = if self.peek_kind() != &TokenKind::LeftParen && !self.is_word("using") {
            Some(self.name()?)
        } else {
            None
        };
        self.index_options()?;
        let columns = self.index_column_list(false)?;
        self.index_options()?;
        Ok(Some(IndexSpec {
            name,
            unique: false,
            columns,
            position,
        }))
    }

    fn index_options(&mut self) -> SqlResult<()> {
        loop {
            if self.take_word("using") {
                self.word_text().ok_or_else(|| self.unexpected())?;
                self.at += 1;
            } else if self.take_word("comment") {
                self.word_text().ok_or_else(|| self.unexpected())?;
                self.at += 1;
            } else {
                return Ok(());
            }
        }
    }

    pub(super) fn mysql_table_options(&mut self) -> SqlResult<Option<String>> {
        let mut comment = None;
        loop {
            let _ = self.take_word("default");
            let Some(option) = self.word_text() else {
                return Ok(comment);
            };
            if !matches!(self.peek_kind(), TokenKind::Word(_)) {
                return Ok(comment);
            }
            if option == "comment" {
                self.at += 1;
                self.take_operator("=");
                let TokenKind::String(text) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                comment = Some(text.as_ref().to_owned());
                self.at += 1;
                self.take(&TokenKind::Comma);
                continue;
            }
            match option.as_str() {
                "engine" | "charset" | "collate" | "auto_increment" | "comment" | "row_format"
                | "stats_persistent" | "key_block_size" | "avg_row_length" | "checksum"
                | "pack_keys" | "min_rows" | "max_rows" => self.at += 1,
                "character" if self.is_word_at(1, "set") => self.at += 2,
                _ => return Ok(comment),
            }
            self.take_operator("=");
            match self.peek_kind() {
                TokenKind::Word(_)
                | TokenKind::QuotedIdentifier(_)
                | TokenKind::String(_)
                | TokenKind::Integer(_) => self.at += 1,
                _ => return Err(self.unexpected()),
            }
            self.take(&TokenKind::Comma);
        }
    }

    pub(super) fn on_duplicate_key_update(
        &mut self,
        current: OnDuplicate,
    ) -> SqlResult<OnDuplicate> {
        self.expect_word("on")?;
        self.expect_word("duplicate")?;
        self.expect_word("key")?;
        self.expect_word("update")?;
        if current == OnDuplicate::Replace {
            return Err(self.unexpected());
        }
        let mut assignments = Vec::new();
        loop {
            let mut name = self.name()?;
            if self.take(&TokenKind::Dot) {
                name = self.name()?;
            }
            if !self.take_operator("=") {
                return Err(self.unexpected());
            }
            assignments.push((name, self.expression()?));
            if !self.take(&TokenKind::Comma) {
                return Ok(OnDuplicate::Update(assignments));
            }
        }
    }

    pub(super) fn mysql_infix_level(&self) -> Option<Level> {
        match self.peek_kind() {
            TokenKind::Operator("||") => Some(Level::Or),
            TokenKind::Operator("&&") => Some(Level::And),
            TokenKind::Operator("<=>") => Some(Level::Comparison),
            TokenKind::Operator("/" | "%") => Some(Level::Multiplicative),
            TokenKind::Word(word) if matches!(word.as_ref(), "div" | "mod") => {
                Some(Level::Multiplicative)
            }
            TokenKind::Word(word) if word == "xor" || word == "regexp" || word == "rlike" => {
                Some(Level::Other)
            }
            _ => None,
        }
    }

    pub(super) fn mysql_infix(&mut self, left: Expr, level: Level) -> SqlResult<Expr> {
        let position = left.position;
        let symbol = self.peek_kind().clone();
        self.at += 1;
        match (symbol, level) {
            (TokenKind::Operator("||"), _) => {
                let right = self.expression_above(Level::And)?;
                Ok(Self::joined(left, right, Level::Or))
            }
            (TokenKind::Operator("&&"), _) => {
                let right = self.expression_above(Level::Not)?;
                Ok(Self::joined(left, right, Level::And))
            }
            (TokenKind::Operator("<=>"), _) => {
                let right = self.expression_above(Level::Pattern)?;
                Ok(Expr {
                    kind: ExprKind::IsDistinctFrom {
                        left: Box::new(left),
                        right: Box::new(right),
                        negated: true,
                    },
                    position,
                })
            }
            (TokenKind::Operator("/"), _) => {
                let right = self.expression_above(Level::Power)?;
                Ok(Self::binary(
                    as_double(left),
                    BinaryOperator::Divide,
                    as_double(right),
                ))
            }
            (TokenKind::Operator("%"), _) => {
                let right = self.expression_above(Level::Power)?;
                Ok(Self::binary(left, BinaryOperator::Modulo, right))
            }
            (TokenKind::Word(word), _) if word == "div" => {
                let right = self.expression_above(Level::Power)?;
                Ok(Self::binary(left, BinaryOperator::Divide, right))
            }
            (TokenKind::Word(word), _) if word == "mod" => {
                let right = self.expression_above(Level::Power)?;
                Ok(Self::binary(left, BinaryOperator::Modulo, right))
            }
            (TokenKind::Word(word), _) => {
                self.at -= 1;
                Err(self.not_supported(&word.to_ascii_uppercase()))
            }
            _ => {
                self.at -= 1;
                Err(self.unexpected())
            }
        }
    }

    pub(super) fn mysql_variable(&mut self, name: &str, position: usize) -> SqlResult<Expr> {
        self.at += 1;
        if let Some(system) = name.strip_prefix("@@") {
            let mut system = system.to_owned();
            if matches!(system.as_str(), "session" | "local" | "global")
                && self.take(&TokenKind::Dot)
            {
                system = self.word_text().ok_or_else(|| self.unexpected())?;
                self.at += 1;
            }
            let value = self
                .settings_or_default(|settings| settings.mysql.system(&system))
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("unrecognized configuration parameter \"{system}\""),
                    )
                    .at(position)
                })?;
            return Ok(value_expr(value, position));
        }
        let value = self.settings_or_default(|settings| settings.mysql.user(variable_name(name)));
        Ok(value_expr(value, position))
    }

    pub(super) fn mysql_primary(&mut self, word: &str, position: usize) -> SqlResult<Option<Expr>> {
        let called = self.peek_kind_at(1) == &TokenKind::LeftParen;
        let expr = match word {
            "database" | "schema" if called => {
                self.at += 1;
                self.empty_arguments()?;
                let database = self.settings_or_default(|settings| settings.database.clone());
                text_value(database, position)
            }
            "current_user" => {
                self.at += 1;
                if called {
                    self.empty_arguments()?;
                }
                text_value(self.account(), position)
            }
            "if" | "replace" | "left" | "right" | "insert" | "mod" if called => {
                let name = Name {
                    text: word.to_owned(),
                    position,
                };
                self.at += 1;
                return self.function(name).map(Some);
            }
            "values" if called => {
                self.at += 2;
                let column = self.name()?;
                self.expect(&TokenKind::RightParen)?;
                Expr {
                    kind: ExprKind::Function {
                        name: "values".to_owned(),
                        arguments: vec![Expr {
                            kind: ExprKind::Column {
                                qualifier: None,
                                name: column.text,
                            },
                            position: column.position,
                        }],
                    },
                    position,
                }
            }
            "current_timestamp" | "current_date" | "localtime" | "localtimestamp" => {
                self.at += 1;
                if called {
                    self.expect(&TokenKind::LeftParen)?;
                    if !self.take(&TokenKind::RightParen) {
                        let TokenKind::Integer(_) = self.peek_kind() else {
                            return Err(self.unexpected());
                        };
                        self.at += 1;
                        self.expect(&TokenKind::RightParen)?;
                    }
                }
                let name = if word == "current_date" {
                    "current_date"
                } else {
                    "mysql_now"
                };
                clock_call(name, position)
            }
            "current_time" => {
                self.at += 1;
                if called {
                    self.empty_or_precision()?;
                }
                clock_call("mysql_curtime", position)
            }
            _ => return Ok(None),
        };
        Ok(Some(expr))
    }

    fn empty_or_precision(&mut self) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        if !self.take(&TokenKind::RightParen) {
            let TokenKind::Integer(_) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            self.at += 1;
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(())
    }

    fn empty_arguments(&mut self) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        self.expect(&TokenKind::RightParen)
    }

    fn account(&self) -> String {
        let user = self.settings_or_default(|settings| {
            settings
                .get("session_authorization")
                .unwrap_or_else(|_| "eggdb".to_owned())
        });
        format!("{user}@localhost")
    }

    pub(super) fn mysql_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let position = name.position;
        let renamed = match name.text.as_str() {
            "version" | "user" | "session_user" | "system_user" | "connection_id"
            | "last_insert_id" | "database" | "schema" => {
                self.empty_arguments()?;
                let expr = match name.text.as_str() {
                    "version" => text_value(MYSQL_COMPATIBLE_VERSION.to_owned(), position),
                    "connection_id" => {
                        let id = self.settings_or_default(|settings| settings.mysql.connection_id);
                        value_expr(Value::BigInt(i64::from(id)), position)
                    }
                    "last_insert_id" => {
                        let id = self.settings_or_default(|settings| settings.mysql.last_insert_id);
                        value_expr(Value::BigInt(id.min(i64::MAX as u64) as i64), position)
                    }
                    "database" | "schema" => text_value(
                        self.settings_or_default(|settings| settings.database.clone()),
                        position,
                    ),
                    _ => text_value(self.account(), position),
                };
                return Ok(Some(expr));
            }
            "now" | "sysdate" | "curdate" | "utc_timestamp" | "current_timestamp" => {
                self.expect(&TokenKind::LeftParen)?;
                if !self.take(&TokenKind::RightParen) {
                    let TokenKind::Integer(_) = self.peek_kind() else {
                        return Err(self.unexpected());
                    };
                    self.at += 1;
                    self.expect(&TokenKind::RightParen)?;
                }
                let clock = match name.text.as_str() {
                    "curdate" => "current_date",
                    "utc_timestamp" => "mysql_utc_now",
                    _ => "mysql_now",
                };
                return Ok(Some(clock_call(clock, position)));
            }
            "curtime" => {
                self.empty_or_precision()?;
                return Ok(Some(clock_call("mysql_curtime", position)));
            }
            "utc_date" | "unix_timestamp" => {
                return Err(self.not_supported("this date and time function"));
            }
            "ifnull" => "coalesce",
            "length" | "octet_length" => "octet_length",
            "ucase" => "upper",
            "lcase" => "lower",
            "concat" => "concat_or_null",
            "if" => "if",
            "greatest" => "mysql_greatest",
            "least" => "mysql_least",
            _ => return Ok(None),
        };
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        let expected = match renamed {
            "coalesce" => Some(2),
            "if" => Some(3),
            "octet_length" | "upper" | "lower" => Some(1),
            _ => None,
        };
        if expected.is_some_and(|count| count != arguments.len())
            || renamed == "concat_or_null" && arguments.is_empty()
            || renamed.starts_with("mysql_") && arguments.len() < 2
        {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!(
                    "Incorrect parameter count in the call to native function '{}'",
                    name.text
                ),
            )
            .with_detail("wrong parameter count")
            .at(position));
        }
        if renamed == "if" {
            let mut arguments = arguments.into_iter();
            let (Some(condition), Some(then), Some(otherwise)) =
                (arguments.next(), arguments.next(), arguments.next())
            else {
                return Err(self.unexpected());
            };
            return Ok(Some(Expr {
                kind: ExprKind::Case {
                    operand: None,
                    branches: vec![(condition, then)],
                    otherwise: Some(Box::new(otherwise)),
                },
                position,
            }));
        }
        Ok(Some(Expr {
            kind: ExprKind::Function {
                name: renamed.to_owned(),
                arguments,
            },
            position,
        }))
    }
}

fn as_double(expr: Expr) -> Expr {
    let position = expr.position;
    Expr {
        kind: ExprKind::Cast {
            expr: Box::new(expr),
            sql_type: SqlType::Double,
        },
        position,
    }
}

fn text_value(value: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::SessionValue {
            name: "value",
            value,
        },
        position,
    }
}

fn value_expr(value: Value, position: usize) -> Expr {
    let literal = |literal| Expr {
        kind: ExprKind::Literal(literal),
        position,
    };
    match value {
        Value::Null => literal(Literal::Null),
        Value::Boolean(flag) => literal(Literal::Boolean(flag)),
        Value::Text(text) => text_value(text, position),
        Value::Real(number) => literal(Literal::Decimal(
            Value::Real(number).to_text().unwrap_or_default(),
        )),
        Value::Double(number) => literal(Literal::Decimal(
            Value::Double(number).to_text().unwrap_or_default(),
        )),
        other => match other.as_integer() {
            Some(number) => Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(literal(Literal::Integer(number))),
                    sql_type: SqlType::BigInt,
                },
                position,
            },
            None => literal(Literal::Null),
        },
    }
}

fn clock_call(name: &str, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments: Vec::new(),
        },
        position,
    }
}

pub(super) fn postgresql_source(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Function { name, arguments } if arguments.is_empty() => match name.as_str() {
            "mysql_now" => Some("localtimestamp".to_owned()),
            "current_date" => Some("current_date".to_owned()),
            _ => None,
        },
        ExprKind::Cast { expr, .. }
            if matches!(expr.kind, ExprKind::Literal(Literal::Decimal(_))) =>
        {
            postgresql_source(expr)
        }
        ExprKind::Literal(Literal::Null) => Some("NULL".to_owned()),
        ExprKind::Literal(Literal::Boolean(flag)) => Some(flag.to_string()),
        ExprKind::Literal(Literal::Integer(number)) => Some(number.to_string()),
        ExprKind::Literal(Literal::Decimal(text)) => Some(text.clone()),
        ExprKind::Literal(Literal::String(text)) => Some(format!("'{}'", text.replace('\'', "''"))),
        ExprKind::Unary(UnaryOperator::Minus, operand) => {
            postgresql_source(operand).map(|source| format!("-{source}"))
        }
        _ => None,
    }
}
