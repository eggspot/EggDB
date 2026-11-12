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

const SYMBOLS: [&str; 19] = [
    "<=", ">=", "!=", "(", ")", ",", ";", ".", "=", "<", ">", "*", "{", "}", "[", "]", ":", "+", "-",
];

fn is_uuid(bytes: &[u8]) -> bool {
    bytes.len() >= 36
        && bytes[..36].iter().enumerate().all(|(at, byte)| {
            if matches!(at, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && bytes
            .get(36)
            .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
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
            at = text[at..]
                .find('\n')
                .map_or(bytes.len(), |offset| at + offset);
        } else if text[at..].starts_with("/*") {
            at = text[at + 2..]
                .find("*/")
                .map(|offset| at + 2 + offset + 2)
                .ok_or("a comment is not closed")?;
        } else if is_uuid(&bytes[at..]) {
            tokens.push(Token::Uuid(text[at..at + 36].to_ascii_lowercase()));
            at += 36;
        } else if (byte == b'0') && matches!(bytes.get(at + 1), Some(b'x' | b'X')) {
            let end = at
                + 2
                + bytes[at + 2..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_hexdigit())
                    .count();
            tokens.push(Token::Blob(text[at + 2..end].to_ascii_lowercase()));
            at = end;
        } else if byte.is_ascii_digit()
            || (byte == b'-'
                && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
                && !matches!(
                    tokens.last(),
                    Some(Token::Word(_) | Token::Number(_) | Token::Quoted(_) | Token::Symbol(")"))
                ))
        {
            let mut end = at + 1;
            while let Some(&next) = bytes.get(end) {
                let exponent_sign =
                    matches!(next, b'+' | b'-') && matches!(bytes.get(end - 1), Some(b'e' | b'E'));
                if next.is_ascii_digit()
                    || next == b'.'
                    || matches!(next, b'e' | b'E')
                    || exponent_sign
                {
                    end += 1;
                } else {
                    break;
                }
            }
            tokens.push(Token::Number(text[at..end].to_owned()));
            at = end;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let end = at
                + bytes[at..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                    .count();
            tokens.push(Token::Word(text[at..end].to_ascii_lowercase()));
            at = end;
        } else if byte == b'\'' || byte == b'"' {
            let mut value = String::new();
            let mut end = at + 1;
            loop {
                let next = text[end..]
                    .find(char::from(byte))
                    .map(|offset| end + offset)
                    .ok_or("a quoted string or name is not closed")?;
                value.push_str(&text[end..next]);
                if bytes.get(next + 1) == Some(&byte) {
                    value.push(char::from(byte));
                    end = next + 2;
                } else {
                    end = next + 1;
                    break;
                }
            }
            tokens.push(if byte == b'\'' {
                Token::Text(value)
            } else {
                Token::Quoted(value)
            });
            at = end;
        } else if text[at..].starts_with("$$") {
            let end = text[at + 2..]
                .find("$$")
                .map(|offset| at + 2 + offset)
                .ok_or("a $$ string is not closed")?;
            tokens.push(Token::Text(text[at + 2..end].to_owned()));
            at = end + 2;
        } else if byte == b'?' {
            tokens.push(Token::Marker);
            at += 1;
        } else if byte == b':'
            && bytes
                .get(at + 1)
                .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'_')
        {
            let end = at
                + 1
                + bytes[at + 1..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                    .count();
            tokens.push(Token::Marker);
            at = end;
        } else if let Some(symbol) = SYMBOLS
            .iter()
            .find(|symbol| text[at..].starts_with(**symbol))
        {
            tokens.push(Token::Symbol(symbol));
            at += symbol.len();
        } else {
            return Err(format!(
                "line 1:{at} no viable alternative at character '{}'",
                text[at..].chars().next().unwrap_or(' ')
            ));
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
    List(Vec<Term>),
    Set(Vec<Term>),
    Map(Vec<(Term, Term)>),
    Tuple(Vec<Term>),
    Fields(Vec<(String, Term)>),
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
    Contains,
    ContainsKey,
}

/// The `IF` of a lightweight transaction on `UPDATE` or `DELETE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Condition {
    Exists,
    Columns(Vec<Relation>),
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
    Columns(Vec<Selector>),
}

/// One selected column, or one field of a user-type column (`home.street`), and its alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selector {
    pub(crate) column: String,
    pub(crate) field: Option<String>,
    pub(crate) alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Assignment {
    Set(Term),
    Add(Term),
    Prepend(Term),
    Remove(Term),
    Element(Term, Term),
    Fields(Vec<(String, Term)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDefinition {
    pub(crate) name: String,
    pub(crate) cql: CqlType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypeChange {
    Add(ColumnDefinition),
    Rename(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlterChange {
    Add(Vec<ColumnDefinition>),
    Drop(Vec<String>),
    Rename(Vec<(String, String)>),
    Options,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Statement {
    Use(String),
    CreateKeyspace {
        name: String,
        if_not_exists: bool,
    },
    DropKeyspace {
        name: String,
        if_exists: bool,
    },
    CreateTable {
        table: TableName,
        if_not_exists: bool,
        columns: Vec<ColumnDefinition>,
        partition: Vec<String>,
        clustering: Vec<String>,
        descending: Vec<String>,
    },
    DropTable {
        table: TableName,
        if_exists: bool,
    },
    AlterTable {
        table: TableName,
        change: AlterChange,
    },
    CreateIndex {
        name: Option<String>,
        table: TableName,
        column: String,
        if_not_exists: bool,
    },
    DropIndex {
        index: TableName,
        if_exists: bool,
    },
    CreateType {
        name: TableName,
        if_not_exists: bool,
        fields: Vec<ColumnDefinition>,
    },
    DropType {
        name: TableName,
        if_exists: bool,
    },
    AlterType {
        name: TableName,
        change: TypeChange,
    },
    Truncate(TableName),
    Insert {
        table: TableName,
        columns: Vec<String>,
        values: Vec<Term>,
        if_not_exists: bool,
    },
    Update {
        table: TableName,
        assignments: Vec<(String, Assignment)>,
        relations: Vec<Relation>,
        condition: Option<Condition>,
    },
    Delete {
        table: TableName,
        columns: Vec<String>,
        elements: Vec<(String, Term)>,
        relations: Vec<Relation>,
        condition: Option<Condition>,
    },
    Select {
        table: TableName,
        selection: Selection,
        relations: Vec<Relation>,
        order: Vec<(String, bool)>,
        limit: Option<Term>,
    },
    Batch(Vec<Statement>),
}

const MOST_NESTING: usize = 16;

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    markers: usize,
    depth: usize,
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
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(format!(
                "line 1: mismatched input, expecting {}",
                word.to_ascii_uppercase()
            ))
        }
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_symbol(&mut self, symbol: &str) -> Result<(), String> {
        if self.eat_symbol(symbol) {
            Ok(())
        } else {
            Err(format!("line 1: mismatched input, expecting '{symbol}'"))
        }
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
            return Ok(TableName {
                keyspace: Some(first),
                table: self.identifier()?,
            });
        }
        Ok(TableName {
            keyspace: None,
            table: first,
        })
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
                _ => Err(format!(
                    "{word} is not a value: functions in values are not supported yet"
                )),
            },
            Some(Token::Symbol("[")) => Ok(Term::List(self.terms_until("]")?)),
            Some(Token::Symbol("(")) => Ok(Term::Tuple(self.terms_until(")")?)),
            Some(Token::Symbol("{")) => {
                if self.eat_symbol("}") {
                    return Ok(Term::Set(Vec::new()));
                }
                if self.starts_field() {
                    let mut fields = Vec::new();
                    loop {
                        let field = self.identifier()?;
                        self.expect_symbol(":")?;
                        fields.push((field, self.nested_term()?));
                        if !self.eat_symbol(",") {
                            break;
                        }
                    }
                    self.expect_symbol("}")?;
                    return Ok(Term::Fields(fields));
                }
                let first = self.nested_term()?;
                if !self.eat_symbol(":") {
                    let mut items = vec![first];
                    while self.eat_symbol(",") {
                        items.push(self.nested_term()?);
                    }
                    self.expect_symbol("}")?;
                    return Ok(Term::Set(items));
                }
                let mut entries = vec![(first, self.nested_term()?)];
                while self.eat_symbol(",") {
                    let key = self.nested_term()?;
                    self.expect_symbol(":")?;
                    entries.push((key, self.nested_term()?));
                }
                self.expect_symbol("}")?;
                Ok(Term::Map(entries))
            }
            _ => Err("line 1: mismatched input, expecting a value".to_owned()),
        }
    }

    fn starts_field(&self) -> bool {
        let named = match self.peek() {
            Some(Token::Word(word)) => !matches!(word.as_str(), "true" | "false" | "null" | "nan" | "infinity"),
            Some(Token::Quoted(_)) => true,
            _ => false,
        };
        named && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol(":")))
    }

    fn nested_term(&mut self) -> Result<Term, String> {
        self.depth += 1;
        if self.depth > MOST_NESTING {
            return Err("collection values are nested too deeply".to_owned());
        }
        let term = self.term();
        self.depth -= 1;
        term
    }

    fn terms_until(&mut self, close: &str) -> Result<Vec<Term>, String> {
        let mut items = Vec::new();
        if self.eat_symbol(close) {
            return Ok(items);
        }
        loop {
            items.push(self.nested_term()?);
            if self.eat_symbol(close) {
                return Ok(items);
            }
            self.expect_symbol(",")?;
        }
    }

    fn cql_type(&mut self) -> Result<CqlType, String> {
        self.depth += 1;
        if self.depth > MOST_NESTING {
            return Err("types are nested too deeply".to_owned());
        }
        let parsed = self.cql_type_inner();
        self.depth -= 1;
        parsed
    }

    fn cql_type_inner(&mut self) -> Result<CqlType, String> {
        let name = self.identifier()?.to_ascii_lowercase();
        if !matches!(name.as_str(), "list" | "set" | "map" | "frozen" | "tuple") {
            if let Some(cql) = CqlType::from_name(&name) {
                return Ok(cql);
            }
            let name = if self.eat_symbol(".") { self.identifier()? } else { name };
            return crate::types::unresolved_user_type(&name);
        }
        self.expect_symbol("<")?;
        let mut arguments = vec![self.cql_type()?];
        while self.eat_symbol(",") {
            arguments.push(self.cql_type()?);
        }
        self.expect_symbol(">")?;
        match (name.as_str(), arguments.as_slice()) {
            ("frozen", [inner]) => Ok(*inner),
            ("list", [element]) => Ok(CqlType::List(crate::types::interned(*element)?)),
            ("set", [element]) => Ok(CqlType::Set(crate::types::interned(*element)?)),
            ("map", [key, value]) => Ok(CqlType::Map(crate::types::interned(*key)?, crate::types::interned(*value)?)),
            ("tuple", items) => Ok(CqlType::Tuple(crate::types::interned_tuple(items.to_vec())?)),
            _ => Err(format!("wrong number of type arguments for {name}")),
        }
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
                Some(Token::Word(word)) if word == "contains" => {
                    if self.eat_word("key") {
                        Operator::ContainsKey
                    } else {
                        Operator::Contains
                    }
                }
                Some(Token::Word(word)) if word == "like" => {
                    return Err(unsupported("LIKE relations"));
                }
                _ => {
                    return Err(
                        "line 1: mismatched input, expecting a relation operator".to_owned()
                    );
                }
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
            relations.push(Relation {
                column,
                operator,
                terms,
            });
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
            return Ok(Statement::CreateKeyspace {
                name,
                if_not_exists,
            });
        }
        if self.eat_word("type") {
            let if_not_exists = self.if_not_exists()?;
            let name = self.table_name()?;
            self.expect_symbol("(")?;
            let mut fields: Vec<ColumnDefinition> = Vec::new();
            loop {
                let field = self.identifier()?;
                if fields.iter().any(|existing| existing.name == field) {
                    return Err(format!("Duplicate field name {field} in type {}", name.table));
                }
                let cql = self.cql_type()?;
                fields.push(ColumnDefinition { name: field, cql });
                if !self.eat_symbol(",") {
                    break;
                }
            }
            self.expect_symbol(")")?;
            return Ok(Statement::CreateType {
                name,
                if_not_exists,
                fields,
            });
        }
        let custom = self.eat_word("custom");
        if self.eat_word("index") {
            let if_not_exists = self.if_not_exists()?;
            let name = if self.eat_word("on") {
                None
            } else {
                let name = self.identifier()?;
                self.expect_word("on")?;
                Some(name)
            };
            let table = self.table_name()?;
            self.expect_symbol("(")?;
            let column = self.identifier()?;
            if matches!(column.as_str(), "keys" | "values" | "entries" | "full") && self.eat_symbol("(") {
                return Err(unsupported("indexes on collections"));
            }
            self.expect_symbol(")")?;
            if self.eat_word("using") {
                if !matches!(self.next(), Some(Token::Quoted(_))) {
                    return Err("line 1: mismatched input, expecting a quoted index class".to_owned());
                }
                if self.eat_word("with") {
                    self.skip_options();
                }
            } else if custom {
                return Err("line 1: CUSTOM INDEX needs USING '<class>'".to_owned());
            }
            return Ok(Statement::CreateIndex {
                name,
                table,
                column,
                if_not_exists,
            });
        }
        if !(self.eat_word("table") || self.eat_word("columnfamily")) {
            return Err(unsupported(
                "this CREATE statement (only keyspaces, tables and indexes are)",
            ));
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
                        if matches!(token, Token::Symbol(";"))
                            || matches!(token, Token::Word(word) if word == "and")
                        {
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
                                    None => {
                                        return Err("a table option's map is not closed".to_owned());
                                    }
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
        Ok(Statement::CreateTable {
            table,
            if_not_exists,
            columns,
            partition,
            clustering,
            descending,
        })
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
            return Err(format!(
                "unmatched column names/values: {} names, {} values",
                columns.len(),
                values.len()
            ));
        }
        let if_not_exists = self.if_not_exists()?;
        self.using()?;
        Ok(Statement::Insert {
            table,
            columns,
            values,
            if_not_exists,
        })
    }

    fn update(&mut self) -> Result<Statement, String> {
        let table = self.table_name()?;
        self.using()?;
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            let column = self.identifier()?;
            if self.eat_symbol(".") {
                let field = self.identifier()?;
                self.expect_symbol("=")?;
                let term = self.term()?;
                match assignments.iter_mut().find(|(name, _)| *name == column) {
                    Some((_, Assignment::Fields(fields))) => fields.push((field, term)),
                    Some(_) => return Err(format!("Multiple incompatible setting of column {column}")),
                    None => assignments.push((column, Assignment::Fields(vec![(field, term)]))),
                }
                if !self.eat_symbol(",") {
                    break;
                }
                continue;
            }
            let element = if self.eat_symbol("[") {
                let index = self.term()?;
                self.expect_symbol("]")?;
                Some(index)
            } else {
                None
            };
            self.expect_symbol("=")?;
            let names_column = |token: Option<&Token>| matches!(token, Some(Token::Word(word) | Token::Quoted(word)) if *word == column);
            let assignment = if let Some(index) = element {
                Assignment::Element(index, self.term()?)
            } else if names_column(self.peek()) && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("+"))) {
                self.at += 2;
                Assignment::Add(self.term()?)
            } else if names_column(self.peek()) && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("-"))) {
                self.at += 2;
                Assignment::Remove(self.term()?)
            } else {
                let term = self.term()?;
                if matches!(self.peek(), Some(Token::Symbol("+"))) && names_column(self.tokens.get(self.at + 1)) {
                    self.at += 2;
                    Assignment::Prepend(term)
                } else {
                    Assignment::Set(term)
                }
            };
            assignments.push((column, assignment));
            if !self.eat_symbol(",") {
                break;
            }
        }
        self.expect_word("where")?;
        let relations = self.relations()?;
        let condition = self.condition()?;
        Ok(Statement::Update {
            table,
            assignments,
            relations,
            condition,
        })
    }

    fn delete(&mut self) -> Result<Statement, String> {
        let mut columns = Vec::new();
        let mut elements = Vec::new();
        if !self.word_is("from") {
            loop {
                let column = self.identifier()?;
                if self.eat_symbol("[") {
                    let element = self.term()?;
                    self.expect_symbol("]")?;
                    elements.push((column, element));
                } else {
                    columns.push(column);
                }
                if !self.eat_symbol(",") {
                    break;
                }
            }
        }
        self.expect_word("from")?;
        let table = self.table_name()?;
        self.using()?;
        self.expect_word("where")?;
        let relations = self.relations()?;
        let condition = self.condition()?;
        Ok(Statement::Delete {
            table,
            columns,
            elements,
            relations,
            condition,
        })
    }

    fn condition(&mut self) -> Result<Option<Condition>, String> {
        if !self.eat_word("if") {
            return Ok(None);
        }
        if self.eat_word("exists") {
            return Ok(Some(Condition::Exists));
        }
        Ok(Some(Condition::Columns(self.relations()?)))
    }

    fn select(&mut self) -> Result<Statement, String> {
        if self.word_is("json") || self.word_is("distinct") {
            return Err(unsupported("SELECT JSON and SELECT DISTINCT"));
        }
        let selection = if self.eat_symbol("*") {
            Selection::All
        } else if self.word_is("count")
            && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("(")))
        {
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
                let column = self.identifier()?;
                if matches!(self.peek(), Some(Token::Symbol("("))) {
                    return Err(unsupported("functions in a selection"));
                }
                let field = if self.eat_symbol(".") {
                    Some(self.identifier()?)
                } else {
                    None
                };
                let alias = if self.eat_word("as") {
                    Some(self.identifier()?)
                } else {
                    None
                };
                columns.push(Selector { column, field, alias });
                if !self.eat_symbol(",") {
                    break;
                }
            }
            Selection::Columns(columns)
        };
        self.expect_word("from")?;
        let table = self.table_name()?;
        let relations = if self.eat_word("where") {
            self.relations()?
        } else {
            Vec::new()
        };
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
        let limit = if self.eat_word("limit") {
            Some(self.term()?)
        } else {
            None
        };
        if self.eat_word("allow") {
            self.expect_word("filtering")?;
        }
        Ok(Statement::Select {
            table,
            selection,
            relations,
            order,
            limit,
        })
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
            if self.word_is("begin") {
                return Err("a batch cannot hold another batch".to_owned());
            }
            let statement = self.statement()?;
            if !matches!(
                statement,
                Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. }
            ) {
                return Err(
                    "only INSERT, UPDATE and DELETE statements are allowed in a batch".to_owned(),
                );
            }
            statements.push(statement);
        }
    }

    fn statement(&mut self) -> Result<Statement, String> {
        let Some(Token::Word(command)) = self.next() else {
            return Err(
                "line 1: no viable alternative at input: a statement starts with a keyword"
                    .to_owned(),
            );
        };
        match command.as_str() {
            "use" => Ok(Statement::Use(self.identifier()?)),
            "create" => self.create(),
            "drop" => {
                if self.eat_word("keyspace") || self.eat_word("schema") {
                    let if_exists = self.if_exists()?;
                    Ok(Statement::DropKeyspace {
                        if_exists,
                        name: self.identifier()?,
                    })
                } else if self.eat_word("table") || self.eat_word("columnfamily") {
                    let if_exists = self.if_exists()?;
                    Ok(Statement::DropTable {
                        if_exists,
                        table: self.table_name()?,
                    })
                } else if self.eat_word("index") {
                    let if_exists = self.if_exists()?;
                    Ok(Statement::DropIndex {
                        if_exists,
                        index: self.table_name()?,
                    })
                } else if self.eat_word("type") {
                    let if_exists = self.if_exists()?;
                    Ok(Statement::DropType {
                        if_exists,
                        name: self.table_name()?,
                    })
                } else {
                    Err(unsupported(
                        "this DROP statement (only keyspaces, tables, indexes and types are)",
                    ))
                }
            }
            "alter" => {
                if self.eat_word("type") {
                    let name = self.table_name()?;
                    let change = if self.eat_word("add") {
                        let _ = self.if_not_exists()?;
                        let field = self.identifier()?;
                        let cql = self.cql_type()?;
                        TypeChange::Add(ColumnDefinition { name: field, cql })
                    } else if self.eat_word("rename") {
                        let _ = self.if_exists()?;
                        let mut renames = Vec::new();
                        loop {
                            let from = self.identifier()?;
                            self.expect_word("to")?;
                            renames.push((from, self.identifier()?));
                            if !self.eat_word("and") {
                                break;
                            }
                        }
                        TypeChange::Rename(renames)
                    } else {
                        return Err(unsupported("this ALTER TYPE (ADD and RENAME are)"));
                    };
                    return Ok(Statement::AlterType { name, change });
                }
                if !(self.eat_word("table") || self.eat_word("columnfamily")) {
                    return Err(unsupported("this ALTER statement (only tables and types are)"));
                }
                let table = self.table_name()?;
                let change = if self.eat_word("add") {
                    let _ = self.if_not_exists()?;
                    let mut columns = Vec::new();
                    let listed = self.eat_symbol("(");
                    loop {
                        let name = self.identifier()?;
                        let cql = self.cql_type()?;
                        columns.push(ColumnDefinition { name, cql });
                        if !listed || !self.eat_symbol(",") {
                            break;
                        }
                    }
                    if listed {
                        self.expect_symbol(")")?;
                    }
                    AlterChange::Add(columns)
                } else if self.eat_word("drop") {
                    let _ = self.if_exists()?;
                    let mut names = Vec::new();
                    let listed = self.eat_symbol("(");
                    loop {
                        names.push(self.identifier()?);
                        if !listed || !self.eat_symbol(",") {
                            break;
                        }
                    }
                    if listed {
                        self.expect_symbol(")")?;
                    }
                    AlterChange::Drop(names)
                } else if self.eat_word("rename") {
                    let _ = self.if_exists()?;
                    let mut pairs = Vec::new();
                    loop {
                        let from = self.identifier()?;
                        if !self.eat_word("to") {
                            return Err("line 1: mismatched input, expecting TO".to_owned());
                        }
                        pairs.push((from, self.identifier()?));
                        if !self.eat_word("and") {
                            break;
                        }
                    }
                    AlterChange::Rename(pairs)
                } else if self.eat_word("with") {
                    self.skip_options();
                    AlterChange::Options
                } else {
                    return Err(unsupported("this ALTER TABLE change (ADD, DROP, RENAME and WITH are)"));
                };
                Ok(Statement::AlterTable { table, change })
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
            other => Err(unsupported(&format!(
                "the {} statement",
                other.to_ascii_uppercase()
            ))),
        }
    }
}

/// Parses one CQL statement, with how many bind markers it holds.
pub(crate) fn parse(text: &str) -> Result<(Statement, usize), String> {
    let mut parser = Parser {
        tokens: tokenize(text)?,
        at: 0,
        markers: 0,
        depth: 0,
    };
    let statement = parser.statement()?;
    while parser.eat_symbol(";") {}
    if parser.at < parser.tokens.len() {
        return Err("line 1: extraneous input after the statement".to_owned());
    }
    Ok((statement, parser.markers))
}
