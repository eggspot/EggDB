use crate::types::CqlType;

const MAX_TOKENS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Quoted(String),
    Text(String),
    Number(String),
    Uuid(String),
    Blob(String),
    Marker,
    Symbol(&'static str),
}

const SYMBOLS: [&str; 18] = ["<=", ">=", "!=", "(", ")", ",", ";", ".", "=", "<", ">", "*", "{", "}", "[", "]", ":", "+"];

fn is_uuid(bytes: &[u8]) -> bool {
    bytes.len() >= 36
        && bytes[..36].iter().enumerate().all(|(at, byte)| if matches!(at, 8 | 13 | 18 | 23) { *byte == b'-' } else { byte.is_ascii_hexdigit() })
        && bytes.get(36).is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if tokens.len() > MAX_TOKENS {
            return Err("the statement is too long".to_owned());
        }
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
        } else if text[at..].starts_with("--") || text[at..].starts_with("//") {
            at = text[at..].find('\n').map_or(bytes.len(), |offset| at + offset);
        } else if text[at..].starts_with("/*") {
            at = text[at + 2..].find("*/").map(|offset| at + 2 + offset + 2).ok_or("a comment is not closed")?;
        } else if is_uuid(&bytes[at..]) {
            tokens.push(Token::Uuid(text[at..at + 36].to_ascii_lowercase()));
            at += 36;
        } else if (byte == b'0') && matches!(bytes.get(at + 1), Some(b'x' | b'X')) {
            let end = at + 2 + bytes[at + 2..].iter().take_while(|byte| byte.is_ascii_hexdigit()).count();
            tokens.push(Token::Blob(text[at + 2..end].to_ascii_lowercase()));
            at = end;
        } else if byte.is_ascii_digit() || (byte == b'-' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit) && !matches!(tokens.last(), Some(Token::Word(_) | Token::Number(_) | Token::Quoted(_) | Token::Symbol(")")))) {
            let mut end = at + 1;
            while let Some(&next) = bytes.get(end) {
                let exponent_sign = matches!(next, b'+' | b'-') && matches!(bytes.get(end - 1), Some(b'e' | b'E'));
                if next.is_ascii_digit() || next == b'.' || matches!(next, b'e' | b'E') || exponent_sign {
                    end += 1;
                } else {
                    break;
                }
            }
            tokens.push(Token::Number(text[at..end].to_owned()));
            at = end;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let end = at + bytes[at..].iter().take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_').count();
            tokens.push(Token::Word(text[at..end].to_ascii_lowercase()));
            at = end;
        } else if byte == b'\'' || byte == b'"' {
            let mut value = String::new();
            let mut end = at + 1;
            loop {
                let next = text[end..].find(char::from(byte)).map(|offset| end + offset).ok_or("a quoted string or name is not closed")?;
                value.push_str(&text[end..next]);
                if bytes.get(next + 1) == Some(&byte) {
                    value.push(char::from(byte));
                    end = next + 2;
                } else {
                    end = next + 1;
                    break;
                }
            }
            tokens.push(if byte == b'\'' { Token::Text(value) } else { Token::Quoted(value) });
            at = end;
        } else if text[at..].starts_with("$$") {
            let end = text[at + 2..].find("$$").map(|offset| at + 2 + offset).ok_or("a $$ string is not closed")?;
            tokens.push(Token::Text(text[at + 2..end].to_owned()));
            at = end + 2;
        } else if byte == b'?' {
            tokens.push(Token::Marker);
            at += 1;
        } else if byte == b':' && bytes.get(at + 1).is_some_and(|next| next.is_ascii_alphabetic() || *next == b'_') {
            let end = at + 1 + bytes[at + 1..].iter().take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_').count();
            tokens.push(Token::Marker);
            at = end;
        } else if let Some(symbol) = SYMBOLS.iter().find(|symbol| text[at..].starts_with(**symbol)) {
            tokens.push(Token::Symbol(symbol));
            at += symbol.len();
        } else {
            return Err(format!("line 1:{at} no viable alternative at character '{}'", text[at..].chars().next().unwrap_or(' ')));
        }
    }
    Ok(tokens)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Term {
    Text(String),
    Number(String),
    Boolean(bool),
    Uuid(String),
    Blob(String),
    Null,
    Marker(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operator {
    Equal,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    NotEqual,
    In,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Relation {
    pub(crate) column: String,
    pub(crate) operator: Operator,
    pub(crate) terms: Vec<Term>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableName {
    pub(crate) keyspace: Option<String>,
    pub(crate) table: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    All,
    Count,
    Columns(Vec<(String, Option<String>)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Assignment {
    Set(Term),
    Add(Term),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDefinition {
    pub(crate) name: String,
    pub(crate) cql: CqlType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Statement {
    Use(String),
    CreateKeyspace { name: String, if_not_exists: bool },
    DropKeyspace { name: String, if_exists: bool },
    CreateTable { table: TableName, if_not_exists: bool, columns: Vec<ColumnDefinition>, partition: Vec<String>, clustering: Vec<String>, descending: Vec<String> },
    DropTable { table: TableName, if_exists: bool },
    Truncate(TableName),
    Insert { table: TableName, columns: Vec<String>, values: Vec<Term>, if_not_exists: bool },
    Update { table: TableName, assignments: Vec<(String, Assignment)>, relations: Vec<Relation> },
    Delete { table: TableName, columns: Vec<String>, relations: Vec<Relation> },
    Select { table: TableName, selection: Selection, relations: Vec<Relation>, order: Vec<(String, bool)>, limit: Option<Term> },
    Batch(Vec<Statement>),
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    markers: usize,
}

fn unsupported(what: &str) -> String {
    format!("{what} is not supported yet")
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn word_is(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(found)) if found == word)
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = self.word_is(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_word(&mut self, word: &str) -> Result<(), String> {
        if self.eat_word(word) { Ok(()) } else { Err(format!("line 1: mismatched input, expecting {}", word.to_ascii_uppercase())) }
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_symbol(&mut self, symbol: &str) -> Result<(), String> {
        if self.eat_symbol(symbol) { Ok(()) } else { Err(format!("line 1: mismatched input, expecting '{symbol}'")) }
    }

    fn identifier(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Word(word)) => Ok(word),
            Some(Token::Quoted(name)) => Ok(name),
            _ => Err("line 1: mismatched input, expecting an identifier".to_owned()),
        }
    }

    fn table_name(&mut self) -> Result<TableName, String> {
        let first = self.identifier()?;
        if self.eat_symbol(".") {
            return Ok(TableName { keyspace: Some(first), table: self.identifier()? });
        }
        Ok(TableName { keyspace: None, table: first })
    }

    fn if_not_exists(&mut self) -> Result<bool, String> {
        if self.eat_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn if_exists(&mut self) -> Result<bool, String> {
        if self.eat_word("if") {
            self.expect_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn term(&mut self) -> Result<Term, String> {
        match self.next() {
            Some(Token::Text(text)) => Ok(Term::Text(text)),
            Some(Token::Number(number)) => Ok(Term::Number(number)),
            Some(Token::Uuid(uuid)) => Ok(Term::Uuid(uuid)),
            Some(Token::Blob(blob)) => Ok(Term::Blob(blob)),
            Some(Token::Marker) => {
                self.markers += 1;
                Ok(Term::Marker(self.markers - 1))
            }
            Some(Token::Word(word)) => match word.as_str() {
                "true" => Ok(Term::Boolean(true)),
                "false" => Ok(Term::Boolean(false)),
                "null" => Ok(Term::Null),
                "nan" => Ok(Term::Number("NaN".to_owned())),
                "infinity" => Ok(Term::Number("Infinity".to_owned())),
                _ => Err(format!("{word} is not a value: functions in values are not supported yet")),
            },
            Some(Token::Symbol("[" | "{")) => Err(unsupported("collection values")),
            _ => Err("line 1: mismatched input, expecting a value".to_owned()),
        }
    }

    fn cql_type(&mut self) -> Result<CqlType, String> {
        let name = self.identifier()?;
        if matches!(name.as_str(), "list" | "set" | "map" | "frozen" | "tuple") {
            return Err(unsupported("collection, tuple and frozen types"));
        }
        CqlType::from_name(&name).ok_or_else(|| format!("unknown type {name}: user-defined types are not supported yet"))
    }

    fn skip_options(&mut self) {
        let mut depth = 0usize;
        while let Some(token) = self.peek() {
            match token {
                Token::Symbol(";") if depth == 0 => break,
                Token::Symbol("{" | "(" | "[") => depth += 1,
                Token::Symbol("}" | ")" | "]") => depth = depth.saturating_sub(1),
                _ => {}
            }
            self.at += 1;
        }
    }

    fn relations(&mut self) -> Result<Vec<Relation>, String> {
        let mut relations = Vec::new();
        loop {
            if matches!(self.peek(), Some(Token::Symbol("("))) || self.word_is("token") {
                return Err(unsupported("tuple and token relations"));
            }
            let column = self.identifier()?;
            let operator = match self.next() {
                Some(Token::Symbol("=")) => Operator::Equal,
                Some(Token::Symbol("<")) => Operator::Less,
                Some(Token::Symbol("<=")) => Operator::LessOrEqual,
                Some(Token::Symbol(">")) => Operator::Greater,
                Some(Token::Symbol(">=")) => Operator::GreaterOrEqual,
                Some(Token::Symbol("!=")) => Operator::NotEqual,
                Some(Token::Word(word)) if word == "in" => Operator::In,
                Some(Token::Word(word)) if word == "contains" || word == "like" => return Err(unsupported("CONTAINS and LIKE relations")),
                _ => return Err("line 1: mismatched input, expecting a relation operator".to_owned()),
            };
            let terms = if operator == Operator::In {
                if !self.eat_symbol("(") {
                    return Err(unsupported("IN with one bound list"));
                }
                let mut terms = Vec::new();
                if !self.eat_symbol(")") {
                    loop {
                        terms.push(self.term()?);
                        if self.eat_symbol(")") {
                            break;
                        }
                        self.expect_symbol(",")?;
                    }
                }
                terms
            } else {
                vec![self.term()?]
            };
            relations.push(Relation { column, operator, terms });
            if !self.eat_word("and") {
                return Ok(relations);
            }
        }
    }

    fn using(&mut self) -> Result<(), String> {
        if !self.eat_word("using") {
            return Ok(());
        }
        loop {
            if self.eat_word("ttl") {
                if !matches!(self.term()?, Term::Number(number) if number == "0") {
                    return Err(unsupported("USING TTL"));
                }
            } else if self.eat_word("timestamp") {
                return Err(unsupported("USING TIMESTAMP"));
            } else {
                return Err("line 1: mismatched input, expecting TTL or TIMESTAMP".to_owned());
            }
            if !self.eat_word("and") {
                return Ok(());
            }
        }
    }

    fn create(&mut self) -> Result<Statement, String> {
        if self.eat_word("keyspace") || self.eat_word("schema") {
            let if_not_exists = self.if_not_exists()?;
            let name = self.identifier()?;
            self.skip_options();
            return Ok(Statement::CreateKeyspace { name, if_not_exists });
        }
        if !(self.eat_word("table") || self.eat_word("columnfamily")) {
            return Err(unsupported("this CREATE statement (only keyspaces and tables are)"));
        }
        let if_not_exists = self.if_not_exists()?;
        let table = self.table_name()?;
        self.expect_symbol("(")?;
        let mut columns = Vec::new();
        let mut partition = Vec::new();
        let mut clustering = Vec::new();
        loop {
            if self.eat_word("primary") {
                self.expect_word("key")?;
                self.expect_symbol("(")?;
                if self.eat_symbol("(") {
                    loop {
                        partition.push(self.identifier()?);
                        if self.eat_symbol(")") {
                            break;
                        }
                        self.expect_symbol(",")?;
                    }
                } else {
                    partition.push(self.identifier()?);
                }
                while self.eat_symbol(",") {
                    clustering.push(self.identifier()?);
                }
                self.expect_symbol(")")?;
            } else {
                let name = self.identifier()?;
                let cql = self.cql_type()?;
                if self.eat_word("static") {
                    return Err(unsupported("static columns"));
                }
                if self.eat_word("primary") {
                    self.expect_word("key")?;
                    partition.push(name.clone());
                }
                columns.push(ColumnDefinition { name, cql });
            }
            if self.eat_symbol(")") {
                break;
            }
            self.expect_symbol(",")?;
        }
        if partition.is_empty() {
            return Err("a table needs a PRIMARY KEY".to_owned());
        }
        let mut descending = Vec::new();
        if self.eat_word("with") {
            loop {
                if self.eat_word("clustering") {
                    self.expect_word("order")?;
                    self.expect_word("by")?;
                    self.expect_symbol("(")?;
                    loop {
                        let column = self.identifier()?;
                        if self.eat_word("desc") {
                            descending.push(column);
                        } else {
                            self.eat_word("asc");
                        }
                        if self.eat_symbol(")") {
                            break;
                        }
                        self.expect_symbol(",")?;
                    }
                } else if self.eat_word("compact") {
                    self.expect_word("storage")?;
                } else {
                    while let Some(token) = self.peek() {
                        if matches!(token, Token::Symbol(";")) || matches!(token, Token::Word(word) if word == "and") {
                            break;
                        }
                        let opening = matches!(token, Token::Symbol("{"));
                        self.at += 1;
                        if opening {
                            let mut depth = 1usize;
                            while depth > 0 {
                                match self.next() {
                                    Some(Token::Symbol("{")) => depth += 1,
                                    Some(Token::Symbol("}")) => depth -= 1,
                                    None => return Err("a table option's map is not closed".to_owned()),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                if !self.eat_word("and") {
                    break;
                }
            }
        }
        Ok(Statement::CreateTable { table, if_not_exists, columns, partition, clustering, descending })
    }

    fn insert(&mut self) -> Result<Statement, String> {
        self.expect_word("into")?;
        let table = self.table_name()?;
        if self.word_is("json") {
            return Err(unsupported("INSERT JSON"));
        }
        self.expect_symbol("(")?;
        let mut columns = vec![self.identifier()?];
        while self.eat_symbol(",") {
            columns.push(self.identifier()?);
        }
        self.expect_symbol(")")?;
        self.expect_word("values")?;
        self.expect_symbol("(")?;
        let mut values = vec![self.term()?];
        while self.eat_symbol(",") {
            values.push(self.term()?);
        }
        self.expect_symbol(")")?;
        if values.len() != columns.len() {
            return Err(format!("unmatched column names/values: {} names, {} values", columns.len(), values.len()));
        }
        let if_not_exists = self.if_not_exists()?;
        self.using()?;
        Ok(Statement::Insert { table, columns, values, if_not_exists })
    }

    fn update(&mut self) -> Result<Statement, String> {
        let table = self.table_name()?;
        self.using()?;
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            let column = self.identifier()?;
            self.expect_symbol("=")?;
            let assignment = if matches!(self.peek(), Some(Token::Word(word) | Token::Quoted(word)) if *word == column)
                && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("+")))
            {
                self.at += 2;
                Assignment::Add(self.term()?)
            } else {
                Assignment::Set(self.term()?)
            };
            assignments.push((column, assignment));
            if !self.eat_symbol(",") {
                break;
            }
        }
        self.expect_word("where")?;
        let relations = self.relations()?;
        if self.word_is("if") {
            return Err(unsupported("conditional updates (lightweight transactions)"));
        }
        Ok(Statement::Update { table, assignments, relations })
    }

    fn delete(&mut self) -> Result<Statement, String> {
        let mut columns = Vec::new();
        if !self.word_is("from") {
            columns.push(self.identifier()?);
            while self.eat_symbol(",") {
                columns.push(self.identifier()?);
            }
        }
        self.expect_word("from")?;
        let table = self.table_name()?;
        self.using()?;
        self.expect_word("where")?;
        let relations = self.relations()?;
        if self.word_is("if") {
            return Err(unsupported("conditional deletes (lightweight transactions)"));
        }
        Ok(Statement::Delete { table, columns, relations })
    }

    fn select(&mut self) -> Result<Statement, String> {
        if self.word_is("json") || self.word_is("distinct") {
            return Err(unsupported("SELECT JSON and SELECT DISTINCT"));
        }
        let selection = if self.eat_symbol("*") {
            Selection::All
        } else if self.word_is("count") && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("("))) {
            self.at += 2;
            if !(self.eat_symbol("*") || matches!(self.next(), Some(Token::Number(_)))) {
                return Err(unsupported("count of a column"));
            }
            self.expect_symbol(")")?;
            if self.eat_word("as") {
                self.identifier()?;
            }
            Selection::Count
        } else {
            let mut columns = Vec::new();
            loop {
                let name = self.identifier()?;
                if matches!(self.peek(), Some(Token::Symbol("("))) {
                    return Err(unsupported("functions in a selection"));
                }
                let alias = if self.eat_word("as") { Some(self.identifier()?) } else { None };
                columns.push((name, alias));
                if !self.eat_symbol(",") {
                    break;
                }
            }
            Selection::Columns(columns)
        };
        self.expect_word("from")?;
        let table = self.table_name()?;
        let relations = if self.eat_word("where") { self.relations()? } else { Vec::new() };
        let mut order = Vec::new();
        if self.eat_word("order") {
            self.expect_word("by")?;
            loop {
                let column = self.identifier()?;
                let descending = self.eat_word("desc");
                if !descending {
                    self.eat_word("asc");
                }
                order.push((column, descending));
                if !self.eat_symbol(",") {
                    break;
                }
            }
        }
        if self.eat_word("per") {
            return Err(unsupported("PER PARTITION LIMIT"));
        }
        let limit = if self.eat_word("limit") { Some(self.term()?) } else { None };
        if self.eat_word("allow") {
            self.expect_word("filtering")?;
        }
        Ok(Statement::Select { table, selection, relations, order, limit })
    }

    fn batch(&mut self) -> Result<Statement, String> {
        let _ = self.eat_word("unlogged") || self.eat_word("logged") || self.eat_word("counter");
        self.expect_word("batch")?;
        self.using()?;
        let mut statements = Vec::new();
        loop {
            while self.eat_symbol(";") {}
            if self.eat_word("apply") {
                self.expect_word("batch")?;
                return Ok(Statement::Batch(statements));
            }
            let statement = self.statement()?;
            if !matches!(statement, Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. }) {
                return Err("only INSERT, UPDATE and DELETE statements are allowed in a batch".to_owned());
            }
            statements.push(statement);
        }
    }

    fn statement(&mut self) -> Result<Statement, String> {
        let Some(Token::Word(command)) = self.next() else {
            return Err("line 1: no viable alternative at input: a statement starts with a keyword".to_owned());
        };
        match command.as_str() {
            "use" => Ok(Statement::Use(self.identifier()?)),
            "create" => self.create(),
            "drop" => {
                if self.eat_word("keyspace") || self.eat_word("schema") {
                    let if_exists = self.if_exists()?;
                    Ok(Statement::DropKeyspace { if_exists, name: self.identifier()? })
                } else if self.eat_word("table") || self.eat_word("columnfamily") {
                    let if_exists = self.if_exists()?;
                    Ok(Statement::DropTable { if_exists, table: self.table_name()? })
                } else {
                    Err(unsupported("this DROP statement (only keyspaces and tables are)"))
                }
            }
            "truncate" => {
                self.eat_word("table");
                Ok(Statement::Truncate(self.table_name()?))
            }
            "insert" => self.insert(),
            "update" => self.update(),
            "delete" => self.delete(),
            "select" => self.select(),
            "begin" => self.batch(),
            other => Err(unsupported(&format!("the {} statement", other.to_ascii_uppercase()))),
        }
    }
}

/// Parses one CQL statement, with how many bind markers it holds.
pub(crate) fn parse(text: &str) -> Result<(Statement, usize), String> {
    let mut parser = Parser { tokens: tokenize(text)?, at: 0, markers: 0 };
    let statement = parser.statement()?;
    while parser.eat_symbol(";") {}
    if parser.at < parser.tokens.len() {
        return Err("line 1: extraneous input after the statement".to_owned());
    }
    Ok((statement, parser.markers))
}
