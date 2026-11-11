use crate::json_out::written;
use eggdb_sql::json::{Json, Node};

const MAX_NESTING: usize = 64;
const MAX_OPERATIONS: usize = 1000;
const MAX_VALUE_BYTES: usize = eggdb_http::MAX_BODY_BYTES;
const MAX_SOURCE_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl Arithmetic {
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Remainder => "%",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Text(String),
    Number(String),
    Dot,
    Comma,
    Semicolon,
    OpenParen,
    CloseParen,
    OpenBracket,
    CloseBracket,
    Operator(Arithmetic),
    Assign(Option<Arithmetic>),
    Increment,
    Decrement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Meta {
    Id,
    Index,
    Version,
}

#[derive(Debug, Clone, PartialEq)]
enum Reference {
    Source(Vec<String>),
    Operation,
    Meta(Meta),
    Param(Vec<String>),
    DocField(String),
    DocValue(String),
    DocSize(String),
    DocEmpty(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Expression {
    Literal(Json),
    Read(Reference),
    Negate(Box<Expression>),
    Binary(Box<Expression>, Arithmetic, Box<Expression>),
}

#[derive(Debug, Clone, PartialEq)]
enum Statement {
    SetSource {
        path: Vec<String>,
        operator: Option<Arithmetic>,
        value: Expression,
    },
    SetOperation(Expression),
    Step {
        path: Vec<String>,
        delta: i64,
    },
    Remove {
        path: Vec<String>,
        key: Expression,
    },
    Add {
        path: Vec<String>,
        value: Expression,
    },
}

/// What a script decided for its document: write it, leave it, delete it, or (for a scripted
/// upsert) create it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Index,
    Noop,
    Delete,
    Create,
}

/// The document a script runs on: its `_source` (changed in place), `ctx.op`, and the
/// read-only `ctx._index`, `ctx._id` and `ctx._version`. A scripted upsert (`upsert` true)
/// runs on the `upsert` document of a missing one: `ctx.op` starts as `create`, may become
/// only `create` or `noop`, and `ctx._version` is `null`.
#[derive(Debug)]
pub(crate) struct Context<'c> {
    pub(crate) source: Json,
    pub(crate) operation: Operation,
    pub(crate) index: &'c str,
    pub(crate) id: &'c str,
    pub(crate) version: u64,
    pub(crate) upsert: bool,
}

fn source_and_params(node: &Node) -> Result<(String, Json), String> {
    let (source, params) = match &node.value {
        Json::String(source) => (source.clone(), Json::Object(Vec::new())),
        Json::Object(members) => {
            let mut source = None;
            let mut params = Json::Object(Vec::new());
            for (key, value) in members {
                match (key.as_str(), &value.value) {
                    ("source" | "inline", Json::String(text)) => source = Some(text.clone()),
                    ("lang", Json::String(lang)) if lang == "painless" => {}
                    ("lang", Json::String(lang)) => {
                        return Err(format!("script_lang not supported [{lang}]"));
                    }
                    ("params", Json::Object(_)) => params = value.value.clone(),
                    ("id", _) => {
                        return Err("stored scripts are not supported".to_owned());
                    }
                    ("options", _) => {}
                    (other, _) => return Err(format!("unknown script field [{other}]")),
                }
            }
            let source = source
                .ok_or_else(|| "must specify <source> for an inline script".to_owned())?;
            (source, params)
        }
        _ => return Err("a script is a string or an object".to_owned()),
    };
    Ok((source, params))
}

/// A compiled value script, as `_script` sorts use: one expression over `doc['field'].value`
/// (the smallest of a field's values), `.size()`, `.empty`, `params`, literals and arithmetic,
/// optionally after `return` and before `;`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValueScript {
    expression: Expression,
    script: Script,
}

impl ValueScript {
    /// Compiles a `script` member as an expression.
    pub(crate) fn from_node(node: &Node) -> Result<Self, String> {
        let (source, params) = source_and_params(node)?;
        let mut tokens = tokens(&source)?;
        while tokens.last() == Some(&Token::Semicolon) {
            tokens.pop();
        }
        let start = usize::from(tokens.first() == Some(&Token::Name("return".to_owned())));
        let mut parser = Parser {
            tokens: &tokens[start..],
            at: 0,
            operations: 0,
        };
        let expression = parser.expression(0)?;
        if parser.at < parser.tokens.len() {
            return Err("a value script is one expression".to_owned());
        }
        Ok(Self {
            expression,
            script: Script {
                statements: Vec::new(),
                params,
            },
        })
    }

    /// The expression's value for the document whose source is `source`.
    pub(crate) fn value(&self, source: &Json) -> Result<Json, String> {
        let context = Context {
            source: source.clone(),
            operation: Operation::Noop,
            index: "",
            id: "",
            version: 0,
            upsert: false,
        };
        self.script.evaluate(&self.expression, &context, 0)
    }
}

/// A compiled update script: the subset of Painless that changes `ctx._source` and `ctx.op`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Script {
    statements: Vec<Statement>,
    params: Json,
}

impl Script {
    /// Compiles the `script` member of a request: a string of source, or an object with
    /// `source`, `lang` (only `painless`) and `params`.
    pub(crate) fn from_node(node: &Node) -> Result<Self, String> {
        let (source, params) = source_and_params(node)?;
        let tokens = tokens(&source)?;
        let statements = Parser {
            tokens: &tokens,
            at: 0,
            operations: 0,
        }
        .statements()?;
        Ok(Self { statements, params })
    }

    /// Runs the script on `context`; an error leaves `context` partly changed, so the caller
    /// discards it.
    pub(crate) fn run(&self, context: &mut Context<'_>) -> Result<(), String> {
        let mut source_bytes = footprint(&context.source, 0)?;
        for statement in &self.statements {
            source_bytes = self.execute(statement, context, source_bytes)?;
        }
        Ok(())
    }

    fn execute(
        &self,
        statement: &Statement,
        context: &mut Context<'_>,
        source_bytes: usize,
    ) -> Result<usize, String> {
        match statement {
            Statement::SetSource {
                path,
                operator,
                value,
            } => {
                let value = self.evaluate(value, context, 0)?;
                let value = match operator {
                    Some(operator) => {
                        let current = lookup(&context.source, path).cloned().unwrap_or(Json::Null);
                        arithmetic(&current, *operator, &value)?
                    }
                    None => value,
                };
                let added = footprint(&value, path.len())?;
                let grown = match (path.last(), lookup(&context.source, path)) {
                    (None, _) => added,
                    (Some(_), Some(replaced)) => {
                        (source_bytes + added).saturating_sub(footprint(replaced, path.len())?)
                    }
                    (Some(name), None) => source_bytes + member_overhead(name) + added,
                };
                assign(&mut context.source, path, value)?;
                within_limit(grown)
            }
            Statement::SetOperation(value) => {
                let upsert = context.upsert;
                context.operation = match self.evaluate(value, context, 0)? {
                    Json::String(op) if op == "noop" || op == "none" => Operation::Noop,
                    Json::String(op) if upsert && op == "create" => Operation::Create,
                    other if upsert => {
                        return Err(format!(
                            "Operation type [{}] not allowed, only [noop, create] are allowed",
                            display(&other)
                        ));
                    }
                    Json::String(op) if op == "index" => Operation::Index,
                    Json::String(op) if op == "delete" => Operation::Delete,
                    other => {
                        return Err(format!(
                            "Operation type [{}] not allowed, only [noop, index, delete] are allowed",
                            display(&other)
                        ));
                    }
                };
                Ok(source_bytes)
            }
            Statement::Step { path, delta } => {
                let current = match lookup(&context.source, path) {
                    Some(value @ Json::Number(_)) => value.clone(),
                    _ => {
                        return Err(format!(
                            "cannot increment or decrement [ctx._source.{}]: it is not a number",
                            path.join(".")
                        ));
                    }
                };
                let stepped =
                    arithmetic(&current, Arithmetic::Add, &Json::Number(delta.to_string()))?;
                let grown = (source_bytes + footprint(&stepped, path.len())?)
                    .saturating_sub(footprint(&current, path.len())?);
                assign(&mut context.source, path, stepped)?;
                within_limit(grown)
            }
            Statement::Remove { path, key } => {
                let key = match self.evaluate(key, context, 0)? {
                    Json::String(key) => key,
                    other => display(&other),
                };
                let members = object_at(&mut context.source, path)?;
                let mut removed = 0usize;
                for (name, member) in members.iter().filter(|(name, _)| *name == key) {
                    removed += member_overhead(name) + footprint(&member.value, 0)?;
                }
                members.retain(|(name, _)| *name != key);
                Ok(source_bytes.saturating_sub(removed))
            }
            Statement::Add { path, value } => {
                let value = self.evaluate(value, context, 0)?;
                let grown = source_bytes + 1 + footprint(&value, path.len() + 1)?;
                match value_at(&mut context.source, path)? {
                    Json::Array(items) => {
                        items.push(node(value));
                        within_limit(grown)
                    }
                    _ => Err(format!(
                        "cannot add to [ctx._source.{}]: it is not a list",
                        path.join(".")
                    )),
                }
            }
        }
    }

    fn evaluate(
        &self,
        expression: &Expression,
        context: &Context<'_>,
        depth: usize,
    ) -> Result<Json, String> {
        if depth > MAX_OPERATIONS {
            return Err("the script nests too deeply".to_owned());
        }
        Ok(match expression {
            Expression::Literal(value) => value.clone(),
            Expression::Read(Reference::Source(path)) => {
                lookup(&context.source, path).cloned().unwrap_or(Json::Null)
            }
            Expression::Read(Reference::Param(path)) => {
                lookup(&self.params, path).cloned().unwrap_or(Json::Null)
            }
            Expression::Read(Reference::DocField(field)) => {
                return Err(format!("read doc['{field}'] with .value, .size() or .empty"));
            }
            Expression::Read(Reference::DocValue(field)) => doc_values(&context.source, field)
                .into_iter()
                .next()
                .ok_or_else(|| format!("A document doesn't have a value for a field! Use doc['{field}'].size()==0 to check if a document is missing a field!"))?,
            Expression::Read(Reference::DocSize(field)) => {
                Json::Number(doc_values(&context.source, field).len().to_string())
            }
            Expression::Read(Reference::DocEmpty(field)) => {
                Json::Boolean(doc_values(&context.source, field).is_empty())
            }
            Expression::Read(Reference::Operation) => Json::String(
                match context.operation {
                    Operation::Index => "index",
                    Operation::Noop => "noop",
                    Operation::Delete => "delete",
                    Operation::Create => "create",
                }
                .to_owned(),
            ),
            Expression::Read(Reference::Meta(Meta::Id)) => Json::String(context.id.to_owned()),
            Expression::Read(Reference::Meta(Meta::Index)) => {
                Json::String(context.index.to_owned())
            }
            Expression::Read(Reference::Meta(Meta::Version)) if context.upsert => Json::Null,
            Expression::Read(Reference::Meta(Meta::Version)) => {
                Json::Number(context.version.to_string())
            }
            Expression::Negate(inner) => {
                let value = self.evaluate(inner, context, depth + 1)?;
                arithmetic(&Json::Number("0".to_owned()), Arithmetic::Subtract, &value)?
            }
            Expression::Binary(left, operator, right) => {
                let left = self.evaluate(left, context, depth + 1)?;
                let right = self.evaluate(right, context, depth + 1)?;
                arithmetic(&left, *operator, &right)?
            }
        })
    }
}

fn node(value: Json) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

fn display(value: &Json) -> String {
    match value {
        Json::Null => "null".to_owned(),
        Json::String(text) => text.clone(),
        other => written(&node(other.clone())),
    }
}

fn type_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Boolean(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "String",
        Json::Array(_) => "List",
        Json::Object(_) => "Map",
    }
}

fn integer(text: &str) -> Option<i64> {
    if text.contains(['.', 'e', 'E']) {
        None
    } else {
        text.parse().ok()
    }
}

fn float_text(value: f64) -> Result<String, String> {
    if value.is_finite() {
        Ok(format!("{value:?}"))
    } else {
        Err("the result is not a finite number".to_owned())
    }
}

fn too_large() -> String {
    format!(
        "the document would grow past {MAX_VALUE_BYTES} bytes or {MAX_SOURCE_DEPTH} levels of nesting"
    )
}

fn within_limit(source_bytes: usize) -> Result<usize, String> {
    if source_bytes > MAX_VALUE_BYTES {
        Err(too_large())
    } else {
        Ok(source_bytes)
    }
}

fn member_overhead(name: &str) -> usize {
    name.len() + 4
}

fn footprint(value: &Json, depth: usize) -> Result<usize, String> {
    let mut remaining = MAX_VALUE_BYTES;
    if fits(value, depth, &mut remaining) {
        Ok(MAX_VALUE_BYTES - remaining)
    } else {
        Err(too_large())
    }
}

fn fits(value: &Json, depth: usize, remaining: &mut usize) -> bool {
    if depth > MAX_SOURCE_DEPTH {
        return false;
    }
    let own = match value {
        Json::Null | Json::Boolean(_) => 5,
        Json::Number(text) => text.len(),
        Json::String(text) => text.len() + 2,
        Json::Array(items) => items.len() + 2,
        Json::Object(members) => {
            2 + members
                .iter()
                .map(|(key, _)| member_overhead(key))
                .sum::<usize>()
        }
    };
    let Some(left) = remaining.checked_sub(own) else {
        return false;
    };
    *remaining = left;
    match value {
        Json::Array(items) => items
            .iter()
            .all(|item| fits(&item.value, depth + 1, remaining)),
        Json::Object(members) => members
            .iter()
            .all(|(_, member)| fits(&member.value, depth + 1, remaining)),
        _ => true,
    }
}

fn concatenated(left: &Json, right: &Json) -> Result<Json, String> {
    let (left, right) = (display(left), display(right));
    if left.len().saturating_add(right.len()) > MAX_VALUE_BYTES {
        return Err(format!(
            "the string would grow past {MAX_VALUE_BYTES} bytes"
        ));
    }
    Ok(Json::String(left + &right))
}

fn arithmetic(left: &Json, operator: Arithmetic, right: &Json) -> Result<Json, String> {
    if operator == Arithmetic::Add
        && (matches!(left, Json::String(_)) || matches!(right, Json::String(_)))
    {
        return concatenated(left, right);
    }
    let (Json::Number(left_text), Json::Number(right_text)) = (left, right) else {
        return Err(format!(
            "cannot apply [{}] to types [{}] and [{}]",
            operator.symbol(),
            type_name(left),
            type_name(right)
        ));
    };
    if let (Some(a), Some(b)) = (integer(left_text), integer(right_text)) {
        let result = match operator {
            Arithmetic::Add => a.wrapping_add(b),
            Arithmetic::Subtract => a.wrapping_sub(b),
            Arithmetic::Multiply => a.wrapping_mul(b),
            Arithmetic::Divide | Arithmetic::Remainder if b == 0 => {
                return Err("/ by zero".to_owned());
            }
            Arithmetic::Divide => a.wrapping_div(b),
            Arithmetic::Remainder => a.wrapping_rem(b),
        };
        return Ok(Json::Number(result.to_string()));
    }
    let (Ok(a), Ok(b)) = (left_text.parse::<f64>(), right_text.parse::<f64>()) else {
        return Err("a number in the document cannot be read".to_owned());
    };
    let result = match operator {
        Arithmetic::Add => a + b,
        Arithmetic::Subtract => a - b,
        Arithmetic::Multiply => a * b,
        Arithmetic::Divide => a / b,
        Arithmetic::Remainder => a % b,
    };
    Ok(Json::Number(float_text(result)?))
}

fn values_along<'j>(source: &'j Json, field: &str) -> Vec<&'j Json> {
    let mut current = vec![source];
    for part in field.split('.') {
        let mut next = Vec::new();
        for value in current {
            let objects: Vec<&Json> = match value {
                Json::Array(items) => items.iter().map(|item| &item.value).collect(),
                other => vec![other],
            };
            for object in objects {
                if let Json::Object(members) = object {
                    next.extend(members.iter().filter(|(key, _)| key == part).map(|(_, member)| &member.value));
                }
            }
        }
        current = next;
    }
    current
        .into_iter()
        .flat_map(|value| match value {
            Json::Array(items) => items.iter().map(|item| &item.value).collect::<Vec<_>>(),
            other => vec![other],
        })
        .filter(|value| matches!(value, Json::Number(_) | Json::String(_) | Json::Boolean(_)))
        .collect()
}

fn doc_values(source: &Json, field: &str) -> Vec<Json> {
    let mut found = values_along(source, field);
    if found.is_empty()
        && let Some(base) = field.strip_suffix(".keyword")
    {
        found = values_along(source, base);
    }
    let mut values: Vec<Json> = found.into_iter().cloned().collect();
    values.sort_by(|left, right| match (left, right) {
        (Json::Number(left), Json::Number(right)) => left
            .parse::<f64>()
            .unwrap_or(f64::NAN)
            .total_cmp(&right.parse::<f64>().unwrap_or(f64::NAN)),
        (Json::String(left), Json::String(right)) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    });
    values
}

fn lookup<'j>(root: &'j Json, path: &[String]) -> Option<&'j Json> {
    let mut current = root;
    for name in path {
        let Json::Object(members) = current else {
            return None;
        };
        current = &members.iter().find(|(key, _)| key == name)?.1.value;
    }
    Some(current)
}

fn value_at<'j>(root: &'j mut Json, path: &[String]) -> Result<&'j mut Json, String> {
    let mut current = root;
    for (at, name) in path.iter().enumerate() {
        let Json::Object(members) = current else {
            return Err(format!(
                "[ctx._source.{}] is not a map",
                path.get(..at).unwrap_or_default().join(".")
            ));
        };
        let Some((_, child)) = members.iter_mut().find(|(key, _)| key == name) else {
            return Err(format!(
                "null pointer: [ctx._source.{}] is missing",
                path.get(..=at).unwrap_or_default().join(".")
            ));
        };
        current = &mut child.value;
    }
    Ok(current)
}

fn object_at<'j>(
    root: &'j mut Json,
    path: &[String],
) -> Result<&'j mut Vec<(String, Node)>, String> {
    match value_at(root, path)? {
        Json::Object(members) => Ok(members),
        _ => Err(format!("[ctx._source.{}] is not a map", path.join("."))),
    }
}

fn assign(root: &mut Json, path: &[String], value: Json) -> Result<(), String> {
    let Some((last, parents)) = path.split_last() else {
        return match value {
            Json::Object(_) => {
                *root = value;
                Ok(())
            }
            _ => Err("ctx._source must stay a map".to_owned()),
        };
    };
    let members = object_at(root, parents)?;
    match members.iter_mut().find(|(key, _)| key == last) {
        Some((_, existing)) => existing.value = value,
        None => members.push((last.clone(), node(value))),
    }
    Ok(())
}

fn tokens(source: &str) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(&character) = characters.get(at) {
        let next = characters.get(at + 1).copied();
        at += 1;
        let token = match character {
            ' ' | '\t' | '\r' | '\n' => continue,
            '.' if !next.is_some_and(|next| next.is_ascii_digit()) => Token::Dot,
            ',' => Token::Comma,
            ';' => Token::Semicolon,
            '(' => Token::OpenParen,
            ')' => Token::CloseParen,
            '[' => Token::OpenBracket,
            ']' => Token::CloseBracket,
            '+' | '-' if next == Some(character) => {
                at += 1;
                if character == '+' {
                    Token::Increment
                } else {
                    Token::Decrement
                }
            }
            '+' | '-' | '*' | '/' | '%' => {
                let operator = match character {
                    '+' => Arithmetic::Add,
                    '-' => Arithmetic::Subtract,
                    '*' => Arithmetic::Multiply,
                    '/' => Arithmetic::Divide,
                    _ => Arithmetic::Remainder,
                };
                if next == Some('=') {
                    at += 1;
                    Token::Assign(Some(operator))
                } else {
                    Token::Operator(operator)
                }
            }
            '=' if next == Some('=') => {
                return Err("comparisons are not supported in update scripts".to_owned());
            }
            '=' => Token::Assign(None),
            '\'' | '"' => {
                let mut text = String::new();
                loop {
                    let Some(&inner) = characters.get(at) else {
                        return Err("unterminated string".to_owned());
                    };
                    at += 1;
                    if inner == character {
                        break;
                    }
                    if inner == '\\' {
                        let Some(&escaped) = characters.get(at) else {
                            return Err("unterminated string".to_owned());
                        };
                        at += 1;
                        text.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            other => other,
                        });
                    } else {
                        text.push(inner);
                    }
                }
                Token::Text(text)
            }
            digit if digit.is_ascii_digit() || digit == '.' => {
                let mut text = String::from(digit);
                while let Some(&more) = characters.get(at) {
                    let exponent_sign = matches!(more, '+' | '-') && text.ends_with(['e', 'E']);
                    if more.is_ascii_digit()
                        || more == '.'
                        || more == 'e'
                        || more == 'E'
                        || exponent_sign
                    {
                        text.push(more);
                        at += 1;
                    } else {
                        break;
                    }
                }
                Token::Number(number_literal(&text)?)
            }
            letter if letter.is_alphabetic() || letter == '_' => {
                let mut name = String::from(letter);
                while let Some(&more) = characters.get(at) {
                    if more.is_alphanumeric() || more == '_' {
                        name.push(more);
                        at += 1;
                    } else {
                        break;
                    }
                }
                Token::Name(name)
            }
            other => return Err(format!("unexpected character [{other}]")),
        };
        out.push(token);
    }
    Ok(out)
}

fn number_literal(text: &str) -> Result<String, String> {
    let invalid = || format!("invalid number [{text}]");
    if text.contains(['.', 'e', 'E']) {
        let value = text.parse::<f64>().map_err(|_| invalid())?;
        return float_text(value).map_err(|_| invalid());
    }
    let value = match text.strip_prefix('0') {
        Some(octal) if !octal.is_empty() => i64::from_str_radix(octal, 8),
        _ => text.parse::<i64>(),
    };
    value.map(|value| value.to_string()).map_err(|_| invalid())
}

enum Path {
    Reference(Reference),
    Call {
        target: Reference,
        method: String,
        arguments: Vec<Expression>,
    },
}

struct Parser<'t> {
    tokens: &'t [Token],
    at: usize,
    operations: usize,
}

impl Parser<'_> {
    fn count_operation(&mut self) -> Result<(), String> {
        self.operations += 1;
        if self.operations > MAX_OPERATIONS {
            Err(format!(
                "the script has more than {MAX_OPERATIONS} operations"
            ))
        } else {
            Ok(())
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn expect(&mut self, wanted: &Token) -> Result<(), String> {
        match self.next() {
            Some(token) if token == *wanted => Ok(()),
            _ => Err(format!("expected [{}]", describe(wanted))),
        }
    }

    fn statements(&mut self) -> Result<Vec<Statement>, String> {
        let mut statements = Vec::new();
        while self.peek().is_some() {
            if self.peek() == Some(&Token::Semicolon) {
                self.at += 1;
                continue;
            }
            self.count_operation()?;
            statements.push(self.statement()?);
            match self.next() {
                None | Some(Token::Semicolon) => {}
                Some(_) => return Err("expected [;] between statements".to_owned()),
            }
        }
        if statements.is_empty() {
            return Err("the script is empty".to_owned());
        }
        Ok(statements)
    }

    fn statement(&mut self) -> Result<Statement, String> {
        if let Some(Token::Increment | Token::Decrement) = self.peek() {
            let delta = if self.next() == Some(Token::Increment) {
                1
            } else {
                -1
            };
            return match self.path(0)? {
                Path::Reference(Reference::Source(path)) if !path.is_empty() => {
                    Ok(Statement::Step { path, delta })
                }
                _ => Err("only fields of ctx._source can be incremented".to_owned()),
            };
        }
        match self.path(0)? {
            Path::Call {
                target: Reference::Source(path),
                method,
                arguments,
            } => {
                let mut arguments = arguments.into_iter();
                let (Some(argument), None) = (arguments.next(), arguments.next()) else {
                    return Err(format!("[{method}] takes one argument"));
                };
                match method.as_str() {
                    "remove" => Ok(Statement::Remove {
                        path,
                        key: argument,
                    }),
                    "add" => Ok(Statement::Add {
                        path,
                        value: argument,
                    }),
                    other => Err(format!(
                        "method [{other}] is not supported in update scripts"
                    )),
                }
            }
            Path::Call { method, .. } => Err(format!(
                "method [{method}] is not supported in update scripts"
            )),
            Path::Reference(reference) => match (reference, self.next()) {
                (Reference::Source(path), Some(step @ (Token::Increment | Token::Decrement)))
                    if !path.is_empty() =>
                {
                    let delta = if step == Token::Increment { 1 } else { -1 };
                    Ok(Statement::Step { path, delta })
                }
                (Reference::Source(path), Some(Token::Assign(operator))) => {
                    let value = self.expression(0)?;
                    Ok(Statement::SetSource {
                        path,
                        operator,
                        value,
                    })
                }
                (Reference::Operation, Some(Token::Assign(None))) => {
                    Ok(Statement::SetOperation(self.expression(0)?))
                }
                (Reference::Param(_) | Reference::Meta(_), Some(Token::Assign(_))) => {
                    Err("only ctx._source and ctx.op can be changed".to_owned())
                }
                _ => Err(
                    "a statement assigns to ctx._source or ctx.op, or calls add or remove"
                        .to_owned(),
                ),
            },
        }
    }

    fn path(&mut self, depth: usize) -> Result<Path, String> {
        let root = match self.next() {
            Some(Token::Name(name)) => name,
            _ => return Err("expected [ctx] or [params]".to_owned()),
        };
        let mut steps: Vec<String> = Vec::new();
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.at += 1;
                    let Some(Token::Name(name)) = self.next() else {
                        return Err("expected a field name after [.]".to_owned());
                    };
                    if self.peek() == Some(&Token::OpenParen) {
                        self.at += 1;
                        self.count_operation()?;
                        let target = reference(&root, steps)?;
                        let arguments = self.arguments(depth + 1)?;
                        return Ok(Path::Call {
                            target,
                            method: name,
                            arguments,
                        });
                    }
                    steps.push(name);
                }
                Some(Token::OpenBracket) => {
                    self.at += 1;
                    let Some(Token::Text(name)) = self.next() else {
                        return Err("expected a quoted field name in [ ]".to_owned());
                    };
                    self.expect(&Token::CloseBracket)?;
                    steps.push(name);
                }
                _ => break,
            }
        }
        Ok(Path::Reference(reference(&root, steps)?))
    }

    fn arguments(&mut self, depth: usize) -> Result<Vec<Expression>, String> {
        let mut arguments = Vec::new();
        if self.peek() == Some(&Token::CloseParen) {
            self.at += 1;
            return Ok(arguments);
        }
        loop {
            self.count_operation()?;
            arguments.push(self.expression(depth)?);
            match self.next() {
                Some(Token::Comma) => {}
                Some(Token::CloseParen) => return Ok(arguments),
                _ => return Err("expected [,] or [)]".to_owned()),
            }
        }
    }

    fn expression(&mut self, depth: usize) -> Result<Expression, String> {
        let mut left = self.term(depth)?;
        while let Some(Token::Operator(operator @ (Arithmetic::Add | Arithmetic::Subtract))) =
            self.peek().cloned()
        {
            self.at += 1;
            self.count_operation()?;
            let right = self.term(depth)?;
            left = Expression::Binary(Box::new(left), operator, Box::new(right));
        }
        Ok(left)
    }

    fn term(&mut self, depth: usize) -> Result<Expression, String> {
        let mut left = self.unary(depth)?;
        while let Some(Token::Operator(
            operator @ (Arithmetic::Multiply | Arithmetic::Divide | Arithmetic::Remainder),
        )) = self.peek().cloned()
        {
            self.at += 1;
            self.count_operation()?;
            let right = self.unary(depth)?;
            left = Expression::Binary(Box::new(left), operator, Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self, depth: usize) -> Result<Expression, String> {
        if depth > MAX_NESTING {
            return Err("the script nests too deeply".to_owned());
        }
        match self.peek().cloned() {
            Some(Token::Operator(Arithmetic::Subtract)) => {
                self.at += 1;
                self.count_operation()?;
                Ok(Expression::Negate(Box::new(self.unary(depth + 1)?)))
            }
            Some(Token::OpenParen) => {
                self.at += 1;
                let inner = self.expression(depth + 1)?;
                self.expect(&Token::CloseParen)?;
                Ok(inner)
            }
            Some(Token::Number(text)) => {
                self.at += 1;
                Ok(Expression::Literal(Json::Number(text)))
            }
            Some(Token::Text(text)) => {
                self.at += 1;
                Ok(Expression::Literal(Json::String(text)))
            }
            Some(Token::Name(name)) if name == "true" || name == "false" => {
                self.at += 1;
                Ok(Expression::Literal(Json::Boolean(name == "true")))
            }
            Some(Token::Name(name)) if name == "null" => {
                self.at += 1;
                Ok(Expression::Literal(Json::Null))
            }
            Some(Token::Name(_)) => match self.path(depth)? {
                Path::Reference(reference) => Ok(Expression::Read(reference)),
                Path::Call {
                    target: Reference::DocField(field),
                    method,
                    arguments,
                } if arguments.is_empty() && (method == "size" || method == "isEmpty") => {
                    Ok(Expression::Read(if method == "size" {
                        Reference::DocSize(field)
                    } else {
                        Reference::DocEmpty(field)
                    }))
                }
                Path::Call { method, .. } => Err(format!(
                    "method [{method}] is not supported in update scripts"
                )),
            },
            _ => Err("expected a value".to_owned()),
        }
    }
}

fn reference(root: &str, steps: Vec<String>) -> Result<Reference, String> {
    match root {
        "params" => Ok(Reference::Param(steps)),
        "doc" => match <[String; 1]>::try_from(steps.clone()) {
            Ok([field]) => Ok(Reference::DocField(field)),
            Err(_) => match steps.as_slice() {
                [field, read] if read == "value" => Ok(Reference::DocValue(field.clone())),
                [field, read] if read == "empty" => Ok(Reference::DocEmpty(field.clone())),
                [field, read] if read == "length" => Ok(Reference::DocSize(field.clone())),
                _ => Err("doc offers doc['field'].value, .size(), .length and .empty".to_owned()),
            },
        },
        "ctx" => {
            let mut steps = steps.into_iter();
            let first = steps.next();
            let rest: Vec<String> = steps.collect();
            match (first.as_deref(), rest.is_empty()) {
                (Some("_source"), _) => Ok(Reference::Source(rest)),
                (Some("op"), true) => Ok(Reference::Operation),
                (Some("_id"), true) => Ok(Reference::Meta(Meta::Id)),
                (Some("_index"), true) => Ok(Reference::Meta(Meta::Index)),
                (Some("_version"), true) => Ok(Reference::Meta(Meta::Version)),
                _ => Err("ctx offers _source, op, _id, _index and _version".to_owned()),
            }
        }
        other => Err(format!("cannot resolve symbol [{other}]")),
    }
}

fn describe(token: &Token) -> &'static str {
    match token {
        Token::CloseParen => ")",
        Token::CloseBracket => "]",
        _ => "token",
    }
}

#[cfg(test)]
mod tests {
    use super::{Context, Operation, Script};
    use eggdb_sql::json::parse_document;

    fn run(script: &str, source: &str) -> Result<(String, Operation), String> {
        let script = Script::from_node(&parse_document(script).expect("the script is JSON"))?;
        let mut context = Context {
            source: parse_document(source).expect("the source is JSON").value,
            operation: Operation::Index,
            index: "my-index-000001",
            id: "1",
            version: 3,
            upsert: false,
        };
        script.run(&mut context)?;
        let written = crate::json_out::written(&eggdb_sql::json::Node {
            value: context.source,
            start: 0,
            end: 0,
        });
        Ok((written, context.operation))
    }

    #[test]
    fn the_reference_examples_run() {
        assert_eq!(
            run(
                "{\"source\":\"ctx._source.count++\",\"lang\":\"painless\"}",
                "{\"count\":1}"
            ),
            Ok(("{\"count\":2}".to_owned(), Operation::Index))
        );
        assert_eq!(
            run(
                "{\"source\":\"ctx._source.counter += params.count\",\"lang\":\"painless\",\"params\":{\"count\":4}}",
                "{\"counter\":1}"
            ),
            Ok(("{\"counter\":5}".to_owned(), Operation::Index))
        );
        assert_eq!(
            run(
                "{\"source\":\"ctx._source.tags.add(params['tag'])\",\"params\":{\"tag\":\"blue\"}}",
                "{\"tags\":[\"red\"]}"
            ),
            Ok(("{\"tags\":[\"red\",\"blue\"]}".to_owned(), Operation::Index))
        );
        assert_eq!(
            run("\"ctx._source.new_field = 'value_of_new_field'\"", "{}"),
            Ok((
                "{\"new_field\":\"value_of_new_field\"}".to_owned(),
                Operation::Index
            ))
        );
        assert_eq!(
            run(
                "\"ctx._source.remove('new_field')\"",
                "{\"a\":1,\"new_field\":2}"
            ),
            Ok(("{\"a\":1}".to_owned(), Operation::Index))
        );
        assert_eq!(
            run(
                "\"ctx._source['my-object'].counter -= 2; ctx.op = 'noop'\"",
                "{\"my-object\":{\"counter\":1}}"
            ),
            Ok((
                "{\"my-object\":{\"counter\":-1}}".to_owned(),
                Operation::Noop
            ))
        );
        assert_eq!(
            run("\"ctx.op = 'delete'\"", "{}"),
            Ok(("{}".to_owned(), Operation::Delete))
        );
    }

    #[test]
    fn arithmetic_follows_java() {
        assert_eq!(
            run(
                "\"ctx._source.a = (ctx._source.a + 1) * 2 / 4; ctx._source.b = ctx._source.b * 1.5; ctx._source.c = 'n' + ctx._source.c + 1; ctx._source.d = -ctx._source.d % 3\"",
                "{\"a\":3,\"b\":3,\"c\":2,\"d\":7}"
            ),
            Ok((
                "{\"a\":2,\"b\":4.5,\"c\":\"n21\",\"d\":-1}".to_owned(),
                Operation::Index
            ))
        );
        assert_eq!(
            run(
                "\"ctx._source.version = ctx._version; ctx._source.id = ctx._id + '@' + ctx._index\"",
                "{}"
            ),
            Ok((
                "{\"version\":3,\"id\":\"1@my-index-000001\"}".to_owned(),
                Operation::Index
            ))
        );
    }

    #[test]
    fn bad_scripts_are_errors_not_panics() {
        for (script, source) in [
            ("\"ctx._source.count++\"", "{}"),
            ("\"ctx._source.count++\"", "{\"count\":\"x\"}"),
            ("\"ctx._source.a = 1 / 0\"", "{}"),
            ("\"ctx._source.a = 1.0 / 0\"", "{}"),
            ("\"ctx._source.a.b = 1\"", "{}"),
            ("\"ctx._source.a.add(1)\"", "{\"a\":1}"),
            ("\"ctx.op = 'explode'\"", "{}"),
            ("\"ctx._source = 1\"", "{}"),
            ("\"ctx._source.a = true + 1\"", "{}"),
            ("\"\"", "{}"),
            ("\"ctx._id = 'x'\"", "{}"),
            ("\"params.a = 1\"", "{}"),
            ("\"ctx._source.a = 'unterminated\"", "{}"),
            ("\"if (ctx._source.a == 1) { ctx.op = 'delete' }\"", "{}"),
            ("\"ctx._source.a = ((((1\"", "{}"),
            ("\"ctx._source.a.size()\"", "{}"),
            ("\"System.exit(0)\"", "{}"),
            ("{\"id\":\"stored\"}", "{}"),
            (
                "{\"source\":\"ctx.op = 'noop'\",\"lang\":\"expression\"}",
                "{}",
            ),
            ("42", "{}"),
        ] {
            assert!(run(script, source).is_err(), "{script} on {source}");
        }
        let deep = format!("\"ctx._source.a = {}1\"", "- ".repeat(4000));
        assert_eq!(
            run(&deep, "{}"),
            Err("the script nests too deeply".to_owned())
        );
        let open = format!("\"ctx._source.a = {}1\"", "(".repeat(4000));
        assert!(run(&open, "{}").is_err());
        let long = format!("\"ctx._source.a = {}1\"", "1 + ".repeat(5000));
        assert!(run(&long, "{}").is_err());
        let fits = format!("\"ctx._source.a = {}1\"", "1 + ".repeat(500));
        assert_eq!(
            run(&fits, "{}"),
            Ok(("{\"a\":501}".to_owned(), super::Operation::Index))
        );
    }

    #[test]
    fn a_whole_source_can_be_replaced_by_a_map() {
        let script = "{\"source\":\"ctx._source = params.doc\",\"params\":{\"doc\":{\"x\":1}}}";
        assert_eq!(
            run(script, "{\"y\":2}"),
            Ok(("{\"x\":1}".to_owned(), Operation::Index))
        );
    }

    #[test]
    fn number_literals_are_stored_as_valid_json() {
        for (literal, stored) in [
            ("1.", "1.0"),
            (".5", "0.5"),
            ("1e3", "1000.0"),
            ("2.50", "2.5"),
            ("0", "0"),
            ("42", "42"),
            ("007", "7"),
            ("010", "8"),
        ] {
            assert_eq!(
                run(&format!("\"ctx._source.a = {literal}\""), "{}"),
                Ok((format!("{{\"a\":{stored}}}"), Operation::Index)),
                "{literal}"
            );
            let (written, _) =
                run(&format!("\"ctx._source.a = {literal}\""), "{}").expect("the literal is valid");
            assert!(parse_document(&written).is_ok(), "{written}");
        }
        for literal in ["08", "1..2", "99999999999999999999", "1e400"] {
            assert!(
                run(&format!("\"ctx._source.a = {literal}\""), "{}").is_err(),
                "{literal}"
            );
        }
    }

    #[test]
    fn scripts_cannot_grow_a_document_without_bound() {
        let doubling = format!("\"{}\"", "ctx._source.s += ctx._source.s; ".repeat(40));
        let error = run(&doubling, "{\"s\":\"x\"}").expect_err("the string stops growing");
        assert!(error.contains("bytes"), "{error}");
        let lists = format!("\"{}\"", "ctx._source.a.add(ctx._source.a); ".repeat(40));
        let error = run(&lists, "{\"a\":[1]}").expect_err("the list stops growing");
        assert!(error.contains("bytes"), "{error}");
        let nesting = format!("\"{}\"", "ctx._source.b = ctx._source; ".repeat(300));
        let error = run(&nesting, "{}").expect_err("the nesting stops growing");
        assert!(error.contains("levels"), "{error}");
        let many = format!("\"{}\"", "ctx._source.n++; ".repeat(1001));
        assert!(run(&many, "{\"n\":0}").is_err());
        let fits = format!("\"{}\"", "ctx._source.s += ctx._source.s; ".repeat(10));
        let (written, _) = run(&fits, "{\"s\":\"x\"}").expect("1 KiB fits");
        assert_eq!(written.len(), "{\"s\":\"\"}".len() + 1024);
    }

    #[test]
    fn nested_calls_stop_at_the_nesting_limit_instead_of_the_stack() {
        let levels = 100_000;
        for root in ["params", "ctx._source.list", "a", "ctx"] {
            let script = format!(
                "\"ctx._source.n = {}1{}\"",
                format!("{root}.f(").repeat(levels),
                ")".repeat(levels)
            );
            let error = run(&script, "{\"list\":[]}").expect_err("nested calls are refused");
            assert!(
                error.contains("nests too deeply")
                    || error.contains("cannot resolve")
                    || error.contains("ctx offers"),
                "{root}: {error}"
            );
        }
        let statement = format!(
            "\"{}1{}\"",
            "ctx._source.list.add(".repeat(levels),
            ")".repeat(levels)
        );
        let error = run(&statement, "{\"list\":[]}").expect_err("nested adds are refused");
        assert!(error.contains("nests too deeply"), "{error}");
    }

    #[test]
    fn many_statements_on_a_large_document_do_not_rescan_it() {
        let items = vec!["1"; 200_000].join(",");
        let source = format!("{{\"a\":[{items}],\"n\":0}}");
        let script = format!(
            "\"{}\"",
            "ctx.op = 'index'; ctx._source.x = 1; ctx._source.n++; ".repeat(333)
        );
        let started = std::time::Instant::now();
        let (written, operation) = run(&script, &source).expect("the script fits");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(operation, Operation::Index);
        assert!(
            written.ends_with(",\"n\":333,\"x\":1}"),
            "{}",
            &written[written.len() - 20..]
        );
    }

    #[test]
    fn removing_and_re_adding_a_member_keeps_the_size_exact() {
        let big = "x".repeat(600_000);
        let source = format!("{{\"big\":\"{big}\"}}");
        let script = format!(
            "{{\"source\":\"{}\",\"params\":{{\"big\":\"{big}\"}}}}",
            "ctx._source.remove('big'); ctx._source.big = params.big; ".repeat(5)
        );
        let (written, _) = run(&script, &source).expect("the document stays the same size");
        assert_eq!(written, source);
        let twice = format!(
            "{{\"source\":\"ctx._source.other = params.big\",\"params\":{{\"big\":\"{big}\"}}}}"
        );
        let error = run(&twice, &source).expect_err("two copies pass 1 MiB");
        assert!(error.contains("bytes"), "{error}");
    }

    fn run_upsert(script: &str, source: &str) -> Result<(String, Operation), String> {
        let script = Script::from_node(&parse_document(script).expect("the script is JSON"))?;
        let mut context = Context {
            source: parse_document(source).expect("the source is JSON").value,
            operation: Operation::Create,
            index: "sessions",
            id: "dh3sgudg8gsrgl",
            version: 0,
            upsert: true,
        };
        script.run(&mut context)?;
        let written = crate::json_out::written(&eggdb_sql::json::Node {
            value: context.source,
            start: 0,
            end: 0,
        });
        Ok((written, context.operation))
    }

    #[test]
    fn a_scripted_upsert_starts_as_create_and_may_only_create_or_skip() {
        assert_eq!(
            run_upsert(
                "{\"source\":\"ctx._source.op = ctx.op; ctx._source.v = ctx._version\"}",
                "{}"
            ),
            Ok((
                "{\"op\":\"create\",\"v\":null}".to_owned(),
                Operation::Create
            ))
        );
        assert_eq!(
            run_upsert("\"ctx.op = 'none'\"", "{}"),
            Ok(("{}".to_owned(), Operation::Noop))
        );
        assert_eq!(
            run_upsert("\"ctx.op = 'noop'; ctx.op = 'create'\"", "{}"),
            Ok(("{}".to_owned(), Operation::Create))
        );
        assert_eq!(
            run_upsert("\"ctx.op = 'delete'\"", "{}"),
            Err("Operation type [delete] not allowed, only [noop, create] are allowed".to_owned())
        );
        assert!(run("\"ctx.op = 'create'\"", "{}").is_err());
    }
}
