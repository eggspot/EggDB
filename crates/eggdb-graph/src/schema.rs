use crate::store::{GraphRead, GraphWrite, SchemaKind, SchemaRule};
use crate::value::Value;
use crate::{GraphError, GraphResult, QueryResult};

/// A schema command: Neo4j 5's constraint and index statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaCommand {
    Create {
        rule: SchemaRule,
        if_not_exists: bool,
    },
    Drop {
        kind: SchemaKind,
        name: String,
        if_exists: bool,
    },
    Show(SchemaKind),
}

fn syntax(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.SyntaxError", message)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Symbol(char),
}

fn tokens(text: &str) -> GraphResult<Vec<Token>> {
    let mut out = Vec::new();
    let mut characters = text.chars().peekable();
    while let Some(&character) = characters.peek() {
        if character.is_whitespace() || character == ';' {
            characters.next();
        } else if character == '`' {
            characters.next();
            let mut name = String::new();
            loop {
                match characters.next() {
                    Some('`') if characters.peek() == Some(&'`') => {
                        characters.next();
                        name.push('`');
                    }
                    Some('`') => break,
                    Some(other) => name.push(other),
                    None => return Err(syntax("a quoted name is not closed")),
                }
            }
            out.push(Token::Word(name));
        } else if character.is_alphanumeric() || character == '_' || character == '$' {
            let mut word = String::new();
            while let Some(&next) = characters.peek() {
                if next.is_alphanumeric() || next == '_' || next == '$' {
                    word.push(next);
                    characters.next();
                } else {
                    break;
                }
            }
            out.push(Token::Word(word));
        } else if "():.,".contains(character) {
            out.push(Token::Symbol(character));
            characters.next();
        } else {
            return Err(syntax(format!("unexpected {character} in a schema command")));
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn word(&self, offset: usize) -> Option<&str> {
        match self.tokens.get(self.at + offset) {
            Some(Token::Word(word)) => Some(word),
            _ => None,
        }
    }

    fn is(&self, keyword: &str) -> bool {
        self.word(0).is_some_and(|word| word.eq_ignore_ascii_case(keyword))
    }

    fn take(&mut self, keyword: &str) -> bool {
        let found = self.is(keyword);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect(&mut self, keyword: &str) -> GraphResult<()> {
        if self.take(keyword) {
            Ok(())
        } else {
            Err(syntax(format!("expected {}", keyword.to_ascii_uppercase())))
        }
    }

    fn symbol(&mut self, symbol: char) -> GraphResult<()> {
        if self.tokens.get(self.at) == Some(&Token::Symbol(symbol)) {
            self.at += 1;
            Ok(())
        } else {
            Err(syntax(format!("expected '{symbol}'")))
        }
    }

    fn take_symbol(&mut self, symbol: char) -> bool {
        let found = self.tokens.get(self.at) == Some(&Token::Symbol(symbol));
        if found {
            self.at += 1;
        }
        found
    }

    fn name(&mut self) -> GraphResult<String> {
        match self.tokens.get(self.at) {
            Some(Token::Word(word)) => {
                self.at += 1;
                Ok(word.clone())
            }
            _ => Err(syntax("expected a name")),
        }
    }

    fn if_not_exists(&mut self) -> GraphResult<bool> {
        if self.take("if") {
            self.expect("not")?;
            self.expect("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn optional_name(&mut self) -> Option<String> {
        let reserved = ["if", "for", "on"];
        match self.word(0) {
            Some(word) if !reserved.iter().any(|keyword| word.eq_ignore_ascii_case(keyword)) => {
                let name = word.to_owned();
                self.at += 1;
                Some(name)
            }
            _ => None,
        }
    }

    fn node_pattern(&mut self) -> GraphResult<(String, String)> {
        self.symbol('(')?;
        let variable = self.name()?;
        self.symbol(':')?;
        let label = self.name()?;
        self.symbol(')')?;
        Ok((variable, label))
    }

    fn properties(&mut self, variable: &str) -> GraphResult<Vec<String>> {
        let grouped = self.take_symbol('(');
        let mut properties = Vec::new();
        loop {
            let owner = self.name()?;
            if owner != variable {
                return Err(syntax(format!("the property must belong to {variable}, not {owner}")));
            }
            self.symbol('.')?;
            properties.push(self.name()?);
            if !grouped || !self.take_symbol(',') {
                break;
            }
        }
        if grouped {
            self.symbol(')')?;
        }
        Ok(properties)
    }
}

fn default_name(kind: SchemaKind, label: &str, properties: &[String]) -> String {
    let prefix = match kind {
        SchemaKind::Unique => "constraint_unique",
        SchemaKind::Exists => "constraint_exists",
        SchemaKind::Index => "index",
    };
    format!("{prefix}_{label}_{}", properties.join("_"))
}

/// The schema command `text` holds, or `None` when it is a query.
pub(crate) fn parse(text: &str) -> Option<GraphResult<SchemaCommand>> {
    let first = text.trim_start().split_whitespace().take(3).collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    let schema = ["create constraint", "drop constraint", "create index", "drop index", "show constraints", "show indexes", "show index", "show all"]
        .iter()
        .any(|start| first.starts_with(start))
        || ["create range index", "create text index", "create point index"]
            .iter()
            .any(|start| first.starts_with(start));
    if !schema {
        return None;
    }
    Some(parse_command(text))
}

fn parse_command(text: &str) -> GraphResult<SchemaCommand> {
    let mut parser = Parser {
        tokens: tokens(text)?,
        at: 0,
    };
    let command = if parser.take("show") {
        let _ = parser.take("all") || parser.take("range");
        if parser.take("constraints") || parser.take("constraint") {
            SchemaCommand::Show(SchemaKind::Unique)
        } else if parser.take("indexes") || parser.take("index") {
            SchemaCommand::Show(SchemaKind::Index)
        } else {
            return Err(syntax("expected CONSTRAINTS or INDEXES"));
        }
    } else if parser.take("drop") {
        let kind = if parser.take("constraint") {
            SchemaKind::Unique
        } else {
            parser.expect("index")?;
            SchemaKind::Index
        };
        let name = parser.name()?;
        let if_exists = if parser.take("if") {
            parser.expect("exists")?;
            true
        } else {
            false
        };
        SchemaCommand::Drop {
            kind,
            name,
            if_exists,
        }
    } else {
        parser.expect("create")?;
        if parser.take("constraint") {
            let name = parser.optional_name();
            let if_not_exists = parser.if_not_exists()?;
            let legacy = parser.take("on");
            if !legacy {
                parser.expect("for")?;
            }
            let (variable, label) = parser.node_pattern()?;
            if legacy {
                parser.expect("assert")?;
            } else {
                parser.expect("require")?;
            }
            let properties = parser.properties(&variable)?;
            parser.expect("is")?;
            let kind = if parser.take("not") {
                parser.expect("null")?;
                SchemaKind::Exists
            } else {
                let _ = parser.take("node");
                parser.expect("unique")?;
                SchemaKind::Unique
            };
            if kind == SchemaKind::Exists && properties.len() != 1 {
                return Err(syntax("an existence constraint takes one property"));
            }
            SchemaCommand::Create {
                rule: SchemaRule {
                    name: name.unwrap_or_else(|| default_name(kind, &label, &properties)),
                    kind,
                    label,
                    properties,
                },
                if_not_exists,
            }
        } else {
            let _ = parser.take("range") || parser.take("text") || parser.take("point");
            parser.expect("index")?;
            let name = parser.optional_name();
            let if_not_exists = parser.if_not_exists()?;
            parser.expect("for")?;
            let (variable, label) = parser.node_pattern()?;
            parser.expect("on")?;
            let properties = parser.properties(&variable)?;
            SchemaCommand::Create {
                rule: SchemaRule {
                    name: name.unwrap_or_else(|| default_name(SchemaKind::Index, &label, &properties)),
                    kind: SchemaKind::Index,
                    label,
                    properties,
                },
                if_not_exists,
            }
        }
    };
    if parser.at != parser.tokens.len() {
        return Err(syntax("unexpected text after the schema command"));
    }
    Ok(command)
}

fn list(items: &[String]) -> Value {
    Value::List(items.iter().cloned().map(Value::String).collect())
}

/// Runs a schema command in a write transaction (or a read one for `SHOW`).
pub(crate) fn show(read: &GraphRead<'_>, kind: SchemaKind) -> GraphResult<QueryResult> {
    let rules = read.schema_rules()?;
    let wanted = |rule: &SchemaRule| match kind {
        SchemaKind::Index => rule.kind == SchemaKind::Index,
        _ => rule.kind != SchemaKind::Index,
    };
    let rows = rules
        .iter()
        .filter(|rule| wanted(rule))
        .enumerate()
        .map(|(at, rule)| {
            let kind = match rule.kind {
                SchemaKind::Unique => "UNIQUENESS",
                SchemaKind::Exists => "NODE_PROPERTY_EXISTENCE",
                SchemaKind::Index => "RANGE",
            };
            let mut row = vec![
                Value::Integer(at as i64 + 1),
                Value::String(rule.name.clone()),
            ];
            if rule.kind == SchemaKind::Index {
                row.push(Value::String("ONLINE".to_owned()));
            }
            row.extend([
                Value::String(kind.to_owned()),
                Value::String("NODE".to_owned()),
                list(std::slice::from_ref(&rule.label)),
                list(&rule.properties),
            ]);
            row
        })
        .collect();
    let mut columns = vec!["id".to_owned(), "name".to_owned()];
    if kind == SchemaKind::Index {
        columns.push("state".to_owned());
    }
    columns.extend(
        ["type", "entityType", "labelsOrTypes", "properties"]
            .iter()
            .map(|column| (*column).to_owned()),
    );
    Ok(QueryResult { columns, rows })
}

pub(crate) fn change(write: &mut GraphWrite<'_>, command: &SchemaCommand) -> GraphResult<QueryResult> {
    match command {
        SchemaCommand::Create {
            rule,
            if_not_exists,
        } => {
            let existing = write.read().schema_rules()?;
            if let Some(found) = existing.iter().find(|found| {
                found.name == rule.name
                    || (found.kind == rule.kind
                        && found.label == rule.label
                        && found.properties == rule.properties)
            }) {
                if *if_not_exists {
                    return Ok(QueryResult::default());
                }
                return Err(GraphError::new(
                    if rule.kind == SchemaKind::Index {
                        "Neo.ClientError.Schema.EquivalentSchemaRuleAlreadyExists"
                    } else {
                        "Neo.ClientError.Schema.ConstraintAlreadyExists"
                    },
                    format!("An equivalent rule already exists: '{}'", found.name),
                ));
            }
            write.check_existing_nodes(rule)?;
            write.put_schema_rule(rule)?;
            Ok(QueryResult::default())
        }
        SchemaCommand::Drop {
            kind,
            name,
            if_exists,
        } => {
            let removed = write.delete_schema_rule(name, *kind == SchemaKind::Index)?;
            if !removed && !*if_exists {
                return Err(GraphError::new(
                    "Neo.DatabaseError.Schema.ConstraintDropFailed",
                    format!("No such {}: '{name}'", if *kind == SchemaKind::Index { "index" } else { "constraint" }),
                ));
            }
            Ok(QueryResult::default())
        }
        SchemaCommand::Show(_) => Ok(QueryResult::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neo4j_5_schema_commands_parse() {
        let command = parse("CREATE CONSTRAINT person_email IF NOT EXISTS FOR (p:Person) REQUIRE p.email IS UNIQUE")
            .unwrap()
            .unwrap();
        assert_eq!(
            command,
            SchemaCommand::Create {
                rule: SchemaRule {
                    name: "person_email".to_owned(),
                    kind: SchemaKind::Unique,
                    label: "Person".to_owned(),
                    properties: vec!["email".to_owned()],
                },
                if_not_exists: true,
            }
        );
        assert!(parse("MATCH (n) RETURN n").is_none());
        assert!(matches!(parse("CREATE INDEX FOR (n:City) ON (n.name, n.country)"), Some(Ok(_))));
        assert!(matches!(parse("DROP CONSTRAINT person_email IF EXISTS"), Some(Ok(_))));
    }
}
