use crate::value::Value;
use crate::{GraphError, GraphResult};

const MAX_DEPTH: usize = 64;
const MOST_OPERATORS: usize = 128;
const MAX_HOPS: u32 = 15;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Text(String),
    Integer(i64),
    Float(f64),
    Parameter(String),
    Symbol(&'static str),
}

const SYMBOLS: [&str; 28] = [
    "..", "<=", ">=", "<>", "=~", "+=", "(", ")", "[", "]", "{", "}", ",", ":", ".", "|", "*", "+",
    "-", "/", "%", "^", "=", "<", ">", ";", "&", "!",
];

fn syntax(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.SyntaxError", message)
}

fn semantic(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.SemanticError", message)
}

fn mixed_label_symbols() -> GraphError {
    syntax(
        "Mixing label expression symbols ('|', '&', '!', and '%') with colon (':') is not allowed. Please only use one set of symbols.",
    )
}

fn tokenize(text: &str) -> GraphResult<Vec<(Token, usize)>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if text[at..].starts_with("//") {
            at = text[at..].find('\n').map_or(bytes.len(), |end| at + end);
            continue;
        }
        if text[at..].starts_with("/*") {
            at = text[at + 2..]
                .find("*/")
                .map(|end| at + 2 + end + 2)
                .ok_or_else(|| syntax("an unterminated comment"))?;
            continue;
        }
        let start = at;
        if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 {
            let rest = &text[at..];
            let length = rest
                .find(|character: char| !(character.is_alphanumeric() || character == '_'))
                .unwrap_or(rest.len());
            if length == 0 {
                return Err(syntax(format!("unexpected character at position {at}")));
            }
            tokens.push((Token::Word(rest[..length].to_owned()), start));
            at += length;
            continue;
        }
        if byte.is_ascii_digit() {
            let rest = &text[at..];
            let mut length = rest
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(rest.len());
            let mut float = false;
            if rest[length..].starts_with('.')
                && rest[length + 1..].starts_with(|character: char| character.is_ascii_digit())
            {
                float = true;
                length += 1 + rest[length + 1..]
                    .find(|character: char| !character.is_ascii_digit())
                    .unwrap_or(rest.len() - length - 1);
            }
            if rest[length..].starts_with(['e', 'E']) {
                let exponent = &rest[length + 1..];
                let sign = usize::from(exponent.starts_with(['+', '-']));
                let digits = exponent[sign..]
                    .find(|character: char| !character.is_ascii_digit())
                    .unwrap_or(exponent.len() - sign);
                if digits > 0 {
                    float = true;
                    length += 1 + sign + digits;
                }
            }
            let number = &rest[..length];
            let token = if float {
                Token::Float(
                    number
                        .parse()
                        .map_err(|_| syntax(format!("invalid number {number}")))?,
                )
            } else {
                Token::Integer(
                    number
                        .parse()
                        .map_err(|_| syntax(format!("integer is too large: {number}")))?,
                )
            };
            tokens.push((token, start));
            at += length;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            let mut out = String::new();
            let mut chars = text[at + 1..].char_indices();
            let mut closed = None;
            while let Some((offset, character)) = chars.next() {
                if character == char::from(byte) {
                    if byte == b'`' && text[at + 1 + offset + 1..].starts_with('`') {
                        chars.next();
                        out.push('`');
                        continue;
                    }
                    closed = Some(offset);
                    break;
                }
                if character == '\\' && byte != b'`' {
                    let Some((_, escaped)) = chars.next() else {
                        break;
                    };
                    out.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                    continue;
                }
                out.push(character);
            }
            let Some(offset) = closed else {
                return Err(syntax("an unterminated string or name"));
            };
            tokens.push((
                if byte == b'`' {
                    Token::Quoted(out)
                } else {
                    Token::Text(out)
                },
                start,
            ));
            at += offset + 2;
            continue;
        }
        if byte == b'$' {
            let rest = &text[at + 1..];
            let length = rest
                .find(|character: char| !(character.is_alphanumeric() || character == '_'))
                .unwrap_or(rest.len());
            if length == 0 {
                return Err(syntax("a parameter needs a name"));
            }
            tokens.push((Token::Parameter(rest[..length].to_owned()), start));
            at += 1 + length;
            continue;
        }
        let Some(symbol) = SYMBOLS
            .iter()
            .find(|symbol| text[at..].starts_with(**symbol))
        else {
            return Err(syntax(format!("unexpected character at position {at}")));
        };
        tokens.push((Token::Symbol(symbol), start));
        at += symbol.len();
    }
    Ok(tokens)
}

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operator {
    Or,
    Xor,
    And,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
    In,
    StartsWith,
    EndsWith,
    Contains,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    Parameter(String),
    Variable(String),
    Property(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Binary(Operator, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Negate(Box<Expr>),
    IsNull(Box<Expr>, bool),
    Call {
        name: String,
        distinct: bool,
        arguments: Vec<Expr>,
        star: bool,
    },
    Case {
        subject: Option<Box<Expr>>,
        arms: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    ListComprehension {
        variable: String,
        list: Box<Expr>,
        filter: Option<Box<Expr>>,
        map: Option<Box<Expr>>,
    },
    PatternComprehension {
        pattern: Box<Pattern>,
        filter: Option<Box<Expr>>,
        map: Box<Expr>,
    },
    LabelPredicate(Box<Expr>, LabelExpression),
    ShortestPath(Box<Pattern>),
    Quantified {
        kind: Quantifier,
        variable: String,
        list: Box<Expr>,
        predicate: Box<Expr>,
    },
}

/// The list predicate functions: `all`, `any`, `none` and `single`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quantifier {
    All,
    Any,
    None,
    Single,
}

/// The labels a node (or the type a relationship) must have: `:A:B` asks for all of them,
/// `:A|B` for any of them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LabelExpression {
    AllOf(Vec<String>),
    AnyOf(Vec<String>),
    Tree(LabelTerm),
}

/// A GQL label expression: `&` (and), `|` (or), `!` (not), `%` (any label) and parentheses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LabelTerm {
    Name(String),
    Wildcard,
    Not(Box<LabelTerm>),
    And(Vec<LabelTerm>),
    Or(Vec<LabelTerm>),
}

impl LabelTerm {
    fn matches(&self, held: &[String]) -> bool {
        match self {
            LabelTerm::Name(name) => held.contains(name),
            LabelTerm::Wildcard => !held.is_empty(),
            LabelTerm::Not(inner) => !inner.matches(held),
            LabelTerm::And(items) => items.iter().all(|item| item.matches(held)),
            LabelTerm::Or(items) => items.iter().any(|item| item.matches(held)),
        }
    }

    fn written(&self) -> String {
        let joined = |items: &[LabelTerm], separator: &str| {
            let parts: Vec<String> = items
                .iter()
                .map(|item| match item {
                    LabelTerm::And(_) | LabelTerm::Or(_) => format!("({})", item.written()),
                    other => other.written(),
                })
                .collect();
            parts.join(separator)
        };
        match self {
            LabelTerm::Name(name) => name.clone(),
            LabelTerm::Wildcard => "%".to_owned(),
            LabelTerm::Not(inner) => match inner.as_ref() {
                LabelTerm::And(_) | LabelTerm::Or(_) => format!("!({})", inner.written()),
                other => format!("!{}", other.written()),
            },
            LabelTerm::And(items) => joined(items, "&"),
            LabelTerm::Or(items) => joined(items, "|"),
        }
    }
}

impl LabelExpression {
    pub(crate) fn matches(&self, held: &[String]) -> bool {
        match self {
            LabelExpression::AllOf(labels) => labels.iter().all(|label| held.contains(label)),
            LabelExpression::AnyOf(labels) => labels.iter().any(|label| held.contains(label)),
            LabelExpression::Tree(term) => term.matches(held),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            LabelExpression::AllOf(labels) | LabelExpression::AnyOf(labels) => labels.is_empty(),
            LabelExpression::Tree(_) => false,
        }
    }

    pub(crate) fn written(&self) -> String {
        let (labels, separator) = match self {
            LabelExpression::AllOf(labels) => (labels, ":"),
            LabelExpression::AnyOf(labels) => (labels, "|"),
            LabelExpression::Tree(term) => return format!(":{}", term.written()),
        };
        if labels.is_empty() {
            return String::new();
        }
        format!(":{}", labels.join(separator))
    }
}

const MAX_LABEL_DEPTH: usize = 32;

/// The direction a relationship pattern follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Outgoing,
    Incoming,
    Either,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodePattern {
    pub(crate) variable: Option<String>,
    pub(crate) labels: LabelExpression,
    pub(crate) properties: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RelationshipPattern {
    pub(crate) variable: Option<String>,
    pub(crate) kinds: Vec<String>,
    pub(crate) properties: Vec<(String, Expr)>,
    pub(crate) direction: Direction,
    pub(crate) hops: Option<(u32, u32)>,
}

/// Which shortest paths a `shortestPath()` or `allShortestPaths()` pattern asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shortest {
    One,
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pattern {
    pub(crate) path: Option<String>,
    pub(crate) shortest: Option<Shortest>,
    pub(crate) start: NodePattern,
    pub(crate) steps: Vec<(RelationshipPattern, NodePattern)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Projection {
    pub(crate) expr: Expr,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Projecting {
    pub(crate) distinct: bool,
    pub(crate) star: bool,
    pub(crate) items: Vec<Projection>,
    pub(crate) order: Vec<(Expr, bool)>,
    pub(crate) skip: Option<Expr>,
    pub(crate) limit: Option<Expr>,
    pub(crate) filter: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SetItem {
    Property(Expr, String, Expr),
    Replace(String, Expr, bool),
    Labels(String, Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Clause {
    Match {
        optional: bool,
        patterns: Vec<Pattern>,
        filter: Option<Expr>,
    },
    Create(Vec<Pattern>),
    Merge {
        pattern: Pattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    Foreach {
        variable: String,
        list: Expr,
        body: Vec<Clause>,
    },
    Set(Vec<SetItem>),
    Remove(Vec<SetItem>),
    Delete {
        detach: bool,
        targets: Vec<Expr>,
    },
    Unwind(Expr, String),
    Call {
        procedure: String,
        arguments: Vec<Expr>,
        yields: Vec<(String, String)>,
    },
    With(Projecting),
    Return(Projecting),
    Subquery(Box<Subquery>),
}

/// Which outer variables a part of a `CALL { ... }` subquery sees: the ones its importing
/// `WITH a, b` names, or all of them for `WITH *`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Imports {
    Variables(Vec<String>),
    Everything,
}

/// One part of a `CALL { ... }` subquery (several when it holds `UNION`), without its importing
/// `WITH`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubqueryPart {
    pub(crate) imports: Imports,
    pub(crate) clauses: Vec<Clause>,
}

/// A `CALL { ... }` subquery, run once for each incoming row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Subquery {
    pub(crate) parts: Vec<SubqueryPart>,
    pub(crate) union_all: bool,
}

impl Subquery {
    pub(crate) fn writes(&self) -> bool {
        self.parts
            .iter()
            .flat_map(|part| &part.clauses)
            .any(Clause::writes)
    }

    pub(crate) fn returns(&self) -> bool {
        self.parts
            .first()
            .and_then(|part| part.clauses.last())
            .is_some_and(|clause| matches!(clause, Clause::Return(_) | Clause::Call { .. }))
    }
}

/// A parsed query: one part, or several joined by `UNION` (`union_all` keeps duplicates).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Query {
    pub(crate) parts: Vec<Vec<Clause>>,
    pub(crate) union_all: bool,
}

impl Query {
    pub(crate) fn writes(&self) -> bool {
        self.parts.iter().flatten().any(Clause::writes)
    }
}

impl Clause {
    pub(crate) fn writes(&self) -> bool {
        match self {
            Clause::Create(_)
            | Clause::Merge { .. }
            | Clause::Foreach { .. }
            | Clause::Set(_)
            | Clause::Remove(_)
            | Clause::Delete { .. } => true,
            Clause::Subquery(subquery) => subquery.writes(),
            Clause::Match { .. }
            | Clause::Unwind(..)
            | Clause::Call { .. }
            | Clause::With(_)
            | Clause::Return(_) => false,
        }
    }
}

impl Pattern {
    pub(crate) fn nodes(&self) -> impl Iterator<Item = &NodePattern> {
        std::iter::once(&self.start).chain(self.steps.iter().map(|(_, node)| node))
    }
}

fn refuse_label_alternatives(patterns: &[Pattern]) -> GraphResult<()> {
    let alternatives = patterns.iter().any(|pattern| {
        pattern
            .nodes()
            .any(|node| !matches!(node.labels, LabelExpression::AllOf(_)))
    });
    if alternatives {
        return Err(syntax(
            "Label expressions with '|' are not allowed in CREATE and MERGE patterns; join the labels of a node with ':'",
        ));
    }
    Ok(())
}

fn importing_with(projecting: &Projecting) -> GraphResult<Option<Imports>> {
    if !projecting
        .items
        .iter()
        .all(|item| matches!(item.expr, Expr::Variable(_)))
    {
        return Ok(None);
    }
    let refuse = |what: &str| {
        semantic(format!(
            "Importing WITH should consist only of simple references to outside variables. {what} is not allowed."
        ))
    };
    if projecting
        .items
        .iter()
        .any(|item| !matches!(&item.expr, Expr::Variable(name) if *name == item.name))
    {
        return Err(semantic(
            "Importing WITH should consist only of simple references to outside variables. Aliasing or expressions are not supported.",
        ));
    }
    if projecting.distinct {
        return Err(refuse("DISTINCT"));
    }
    if !projecting.order.is_empty() {
        return Err(refuse("ORDER BY"));
    }
    if projecting.skip.is_some() {
        return Err(refuse("SKIP"));
    }
    if projecting.limit.is_some() {
        return Err(refuse("LIMIT"));
    }
    if projecting.filter.is_some() {
        return Err(refuse("WHERE"));
    }
    Ok(Some(if projecting.star {
        Imports::Everything
    } else {
        Imports::Variables(
            projecting
                .items
                .iter()
                .map(|item| item.name.clone())
                .collect(),
        )
    }))
}

fn subquery_part(mut clauses: Vec<Clause>) -> GraphResult<SubqueryPart> {
    let imports = match clauses.first() {
        Some(Clause::With(projecting)) => importing_with(projecting)?,
        _ => None,
    };
    let imports = match imports {
        Some(imports) => {
            clauses.remove(0);
            imports
        }
        None => Imports::Variables(Vec::new()),
    };
    Ok(SubqueryPart { imports, clauses })
}

struct Parser<'t> {
    text: &'t str,
    tokens: Vec<(Token, usize)>,
    at: usize,
    depth: usize,
    operators: usize,
    bar_ends_part: bool,
}

impl Parser<'_> {
    fn count_operator(&mut self) -> GraphResult<()> {
        self.operators += 1;
        if self.operators > MOST_OPERATORS {
            return Err(syntax(
                "the statement has too many operators; a statement may have at most 128",
            ));
        }
        Ok(())
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|(token, _)| token)
    }

    fn peek_at(&self, ahead: usize) -> Option<&Token> {
        self.tokens.get(self.at + ahead).map(|(token, _)| token)
    }

    fn offset(&self) -> usize {
        self.tokens
            .get(self.at)
            .map_or(self.text.len(), |(_, offset)| *offset)
    }

    fn unexpected(&self) -> GraphError {
        match self.peek() {
            Some(token) => syntax(format!(
                "Invalid input {token:?} at position {}",
                self.offset()
            )),
            None => syntax("Unexpected end of input"),
        }
    }

    fn is_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
    }

    fn is_keyword_at(&self, ahead: usize, word: &str) -> bool {
        matches!(self.peek_at(ahead), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
    }

    fn take_keyword(&mut self, word: &str) -> bool {
        let found = self.is_keyword(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_keyword(&mut self, word: &str) -> GraphResult<()> {
        if self.take_keyword(word) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn is_symbol(&self, symbol: &str) -> bool {
        matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol)
    }

    fn take_symbol(&mut self, symbol: &str) -> bool {
        let found = self.is_symbol(symbol);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_symbol(&mut self, symbol: &str) -> GraphResult<()> {
        if self.take_symbol(symbol) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn name(&mut self) -> GraphResult<String> {
        match self.peek().cloned() {
            Some(Token::Word(word) | Token::Quoted(word)) => {
                self.at += 1;
                Ok(word)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn integer(&mut self) -> GraphResult<u32> {
        match self.peek() {
            Some(Token::Integer(number)) if (0..=i64::from(u32::MAX)).contains(number) => {
                let number = *number as u32;
                self.at += 1;
                Ok(number)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn map_literal(&mut self) -> GraphResult<Vec<(String, Expr)>> {
        self.expect_symbol("{")?;
        let mut entries = Vec::new();
        if !self.take_symbol("}") {
            loop {
                let key = self.name()?;
                self.expect_symbol(":")?;
                entries.push((key, self.expression()?));
                if !self.take_symbol(",") {
                    break;
                }
            }
            self.expect_symbol("}")?;
        }
        Ok(entries)
    }

    fn node_pattern(&mut self) -> GraphResult<NodePattern> {
        self.expect_symbol("(")?;
        let variable = match self.peek() {
            Some(Token::Word(_) | Token::Quoted(_)) => Some(self.name()?),
            _ => None,
        };
        let labels = if self.is_symbol(":") {
            self.label_expression(false)?
        } else {
            LabelExpression::AllOf(Vec::new())
        };
        let properties = if self.is_symbol("{") {
            self.map_literal()?
        } else {
            Vec::new()
        };
        self.expect_symbol(")")?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
        })
    }

    fn label_expression(&mut self, bar_may_end_it: bool) -> GraphResult<LabelExpression> {
        self.expect_symbol(":")?;
        let first = self.label_or(bar_may_end_it, 0)?;
        if self.is_symbol(":") {
            let LabelTerm::Name(name) = first else {
                return Err(mixed_label_symbols());
            };
            let mut labels = vec![name];
            while self.take_symbol(":") {
                labels.push(self.name()?);
            }
            if (self.is_symbol("|") && !bar_may_end_it)
                || self.is_symbol("&")
                || self.is_symbol("!")
            {
                return Err(mixed_label_symbols());
            }
            return Ok(LabelExpression::AllOf(labels));
        }
        Ok(match first {
            LabelTerm::Name(name) => LabelExpression::AllOf(vec![name]),
            LabelTerm::Or(items) if items.iter().all(|item| matches!(item, LabelTerm::Name(_))) => {
                LabelExpression::AnyOf(
                    items
                        .into_iter()
                        .filter_map(|item| match item {
                            LabelTerm::Name(name) => Some(name),
                            _ => None,
                        })
                        .collect(),
                )
            }
            other => LabelExpression::Tree(other),
        })
    }

    fn label_or(&mut self, bar_may_end_it: bool, depth: usize) -> GraphResult<LabelTerm> {
        if depth > MAX_LABEL_DEPTH {
            return Err(syntax("the label expression nests too deeply"));
        }
        let mut items = vec![self.label_and(bar_may_end_it, depth)?];
        while !bar_may_end_it && self.take_symbol("|") {
            if self.is_symbol(":") {
                return Err(mixed_label_symbols());
            }
            items.push(self.label_and(bar_may_end_it, depth)?);
        }
        Ok(if items.len() == 1 {
            items.remove(0)
        } else {
            LabelTerm::Or(items)
        })
    }

    fn label_and(&mut self, bar_may_end_it: bool, depth: usize) -> GraphResult<LabelTerm> {
        let mut items = vec![self.label_not(bar_may_end_it, depth)?];
        while self.take_symbol("&") {
            items.push(self.label_not(bar_may_end_it, depth)?);
        }
        Ok(if items.len() == 1 {
            items.remove(0)
        } else {
            LabelTerm::And(items)
        })
    }

    fn label_not(&mut self, bar_may_end_it: bool, depth: usize) -> GraphResult<LabelTerm> {
        if self.take_symbol("!") {
            if depth > MAX_LABEL_DEPTH {
                return Err(syntax("the label expression nests too deeply"));
            }
            return Ok(LabelTerm::Not(Box::new(self.label_not(
                bar_may_end_it,
                depth + 1,
            )?)));
        }
        if self.take_symbol("%") {
            return Ok(LabelTerm::Wildcard);
        }
        if self.take_symbol("(") {
            let inner = self.label_or(false, depth + 1)?;
            self.expect_symbol(")")?;
            return Ok(inner);
        }
        Ok(LabelTerm::Name(self.name()?))
    }

    fn relationship_pattern(&mut self) -> GraphResult<Option<RelationshipPattern>> {
        let incoming = if self.is_symbol("<") && matches!(self.peek_at(1), Some(Token::Symbol("-")))
        {
            self.at += 2;
            true
        } else if self.take_symbol("-") {
            false
        } else {
            return Ok(None);
        };
        let mut pattern = RelationshipPattern {
            variable: None,
            kinds: Vec::new(),
            properties: Vec::new(),
            direction: Direction::Either,
            hops: None,
        };
        if self.take_symbol("[") {
            if matches!(self.peek(), Some(Token::Word(_) | Token::Quoted(_))) {
                pattern.variable = Some(self.name()?);
            }
            if self.take_symbol(":") {
                pattern.kinds.push(self.name()?);
                while self.take_symbol("|") {
                    self.take_symbol(":");
                    pattern.kinds.push(self.name()?);
                }
            }
            if self.take_symbol("*") {
                let low = if matches!(self.peek(), Some(Token::Integer(_))) {
                    Some(self.integer()?)
                } else {
                    None
                };
                let (min, max) = if self.take_symbol("..") {
                    let high = if matches!(self.peek(), Some(Token::Integer(_))) {
                        Some(self.integer()?)
                    } else {
                        None
                    };
                    (low.unwrap_or(1), high.unwrap_or(MAX_HOPS))
                } else {
                    match low {
                        Some(exact) => (exact, exact),
                        None => (1, MAX_HOPS),
                    }
                };
                if max > MAX_HOPS || min > max {
                    return Err(syntax(format!(
                        "variable-length relationships go at most {MAX_HOPS} hops"
                    )));
                }
                pattern.hops = Some((min, max));
            }
            if self.is_symbol("{") {
                pattern.properties = self.map_literal()?;
            }
            self.expect_symbol("]")?;
        }
        self.expect_symbol("-")?;
        let outgoing = self.take_symbol(">");
        pattern.direction = match (incoming, outgoing) {
            (true, true) => return Err(syntax("a relationship cannot point both ways")),
            (true, false) => Direction::Incoming,
            (false, true) => Direction::Outgoing,
            (false, false) => Direction::Either,
        };
        Ok(Some(pattern))
    }

    fn pattern(&mut self) -> GraphResult<Pattern> {
        let path = if matches!(self.peek(), Some(Token::Word(_) | Token::Quoted(_)))
            && matches!(self.peek_at(1), Some(Token::Symbol("=")))
        {
            let name = self.name()?;
            self.at += 1;
            Some(name)
        } else {
            None
        };
        let shortest = match self.peek() {
            Some(Token::Word(word)) if matches!(self.peek_at(1), Some(Token::Symbol("("))) => {
                if word.eq_ignore_ascii_case("shortestpath") {
                    Some(Shortest::One)
                } else if word.eq_ignore_ascii_case("allshortestpaths") {
                    Some(Shortest::All)
                } else {
                    return Err(self.unexpected());
                }
            }
            _ => None,
        };
        if shortest.is_some() {
            self.at += 2;
        }
        let start = self.node_pattern()?;
        let mut steps = Vec::new();
        while let Some(relationship) = self.relationship_pattern()? {
            steps.push((relationship, self.node_pattern()?));
        }
        if shortest.is_some() {
            self.expect_symbol(")")?;
            let [(relationship, _)] = steps.as_slice() else {
                return Err(syntax(
                    "shortestPath(...) requires a pattern containing a single relationship",
                ));
            };
            if relationship.hops.is_some_and(|(min, _)| min > 1) {
                return Err(syntax(
                    "shortestPath(...) does not support a minimal length different from 0 or 1",
                ));
            }
        }
        Ok(Pattern {
            path,
            shortest,
            start,
            steps,
        })
    }

    fn quantified(&mut self, name: &str) -> GraphResult<Expr> {
        let kind = match name {
            "all" => Quantifier::All,
            "any" => Quantifier::Any,
            "none" => Quantifier::None,
            _ => Quantifier::Single,
        };
        self.at += 1;
        self.expect_symbol("(")?;
        let variable = self.name()?;
        self.expect_keyword("in")?;
        let list = Box::new(self.expression()?);
        self.expect_keyword("where")?;
        let predicate = Box::new(self.expression()?);
        self.expect_symbol(")")?;
        Ok(Expr::Quantified {
            kind,
            variable,
            list,
            predicate,
        })
    }

    fn list_comprehension(&mut self) -> GraphResult<Expr> {
        let variable = self.name()?;
        self.expect_keyword("in")?;
        let list = Box::new(self.expression_ended_by_bar()?);
        let filter = if self.take_keyword("where") {
            Some(Box::new(self.expression_ended_by_bar()?))
        } else {
            None
        };
        let map = if self.take_symbol("|") {
            Some(Box::new(self.expression()?))
        } else {
            None
        };
        self.expect_symbol("]")?;
        Ok(Expr::ListComprehension {
            variable,
            list,
            filter,
            map,
        })
    }

    fn pattern_comprehension(&mut self) -> GraphResult<Option<Expr>> {
        let starts_like_a_pattern = matches!(self.peek_at(1), Some(Token::Symbol("(")))
            || (matches!(self.peek_at(1), Some(Token::Word(_) | Token::Quoted(_)))
                && matches!(self.peek_at(2), Some(Token::Symbol("="))));
        if !starts_like_a_pattern || !self.bracket_holds_a_comprehension() {
            return Ok(None);
        }
        self.at += 1;
        let pattern = self.pattern()?;
        if pattern.steps.is_empty() || pattern.shortest.is_some() {
            return Err(syntax(
                "a pattern comprehension needs a relationship pattern and cannot be a shortest path",
            ));
        }
        let filter = if self.take_keyword("where") {
            Some(Box::new(self.expression_ended_by_bar()?))
        } else {
            None
        };
        self.expect_symbol("|")?;
        let map = Box::new(self.expression()?);
        self.expect_symbol("]")?;
        Ok(Some(Expr::PatternComprehension {
            pattern: Box::new(pattern),
            filter,
            map,
        }))
    }

    fn bracket_holds_a_comprehension(&self) -> bool {
        let mut nesting = 0usize;
        for (token, _) in self.tokens.iter().skip(self.at) {
            match token {
                Token::Symbol("[" | "(" | "{") => nesting += 1,
                Token::Symbol("]" | ")" | "}") => {
                    nesting = nesting.saturating_sub(1);
                    if nesting == 0 {
                        return false;
                    }
                }
                Token::Symbol("|") if nesting == 1 => return true,
                Token::Word(word) if nesting == 1 && word.eq_ignore_ascii_case("where") => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn patterns(&mut self) -> GraphResult<Vec<Pattern>> {
        let mut patterns = vec![self.pattern()?];
        while self.take_symbol(",") {
            patterns.push(self.pattern()?);
        }
        Ok(patterns)
    }

    fn expression(&mut self) -> GraphResult<Expr> {
        self.expression_where_bar_ends(false)
    }

    fn expression_ended_by_bar(&mut self) -> GraphResult<Expr> {
        self.expression_where_bar_ends(true)
    }

    fn expression_where_bar_ends(&mut self, bar_ends_part: bool) -> GraphResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(syntax("the expression is nested too deeply"));
        }
        let outer = std::mem::replace(&mut self.bar_ends_part, bar_ends_part);
        let parsed = self.or();
        self.bar_ends_part = outer;
        self.depth -= 1;
        parsed
    }

    fn binary(
        &mut self,
        operator: Operator,
        left: Expr,
        next: fn(&mut Self) -> GraphResult<Expr>,
    ) -> GraphResult<Expr> {
        self.count_operator()?;
        Ok(Expr::Binary(
            operator,
            Box::new(left),
            Box::new(next(self)?),
        ))
    }

    fn or(&mut self) -> GraphResult<Expr> {
        let mut left = self.xor()?;
        while self.take_keyword("or") {
            left = self.binary(Operator::Or, left, Self::xor)?;
        }
        Ok(left)
    }

    fn xor(&mut self) -> GraphResult<Expr> {
        let mut left = self.and()?;
        while self.take_keyword("xor") {
            left = self.binary(Operator::Xor, left, Self::and)?;
        }
        Ok(left)
    }

    fn and(&mut self) -> GraphResult<Expr> {
        let mut left = self.not()?;
        while self.take_keyword("and") {
            left = self.binary(Operator::And, left, Self::not)?;
        }
        Ok(left)
    }

    fn not(&mut self) -> GraphResult<Expr> {
        if self.take_keyword("not") {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err(syntax("the expression is nested too deeply"));
            }
            let inner = self.not();
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(inner?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> GraphResult<Expr> {
        let mut left = self.additive()?;
        loop {
            let operator = match self.peek() {
                Some(Token::Symbol("=")) => Operator::Equal,
                Some(Token::Symbol("<>")) => Operator::NotEqual,
                Some(Token::Symbol("<")) => Operator::Less,
                Some(Token::Symbol("<=")) => Operator::LessOrEqual,
                Some(Token::Symbol(">")) => Operator::Greater,
                Some(Token::Symbol(">=")) => Operator::GreaterOrEqual,
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("in") => Operator::In,
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("contains") => {
                    Operator::Contains
                }
                Some(Token::Word(word))
                    if word.eq_ignore_ascii_case("starts") && self.is_keyword_at(1, "with") =>
                {
                    Operator::StartsWith
                }
                Some(Token::Word(word))
                    if word.eq_ignore_ascii_case("ends") && self.is_keyword_at(1, "with") =>
                {
                    Operator::EndsWith
                }
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("is") => {
                    self.at += 1;
                    let negated = self.take_keyword("not");
                    self.expect_keyword("null")?;
                    self.count_operator()?;
                    left = Expr::IsNull(Box::new(left), negated);
                    continue;
                }
                _ => return Ok(left),
            };
            self.at += if matches!(operator, Operator::StartsWith | Operator::EndsWith) {
                2
            } else {
                1
            };
            left = self.binary(operator, left, Self::additive)?;
        }
    }

    fn additive(&mut self) -> GraphResult<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let operator = if self.take_symbol("+") {
                Operator::Add
            } else if self.is_symbol("-") {
                self.at += 1;
                Operator::Subtract
            } else {
                return Ok(left);
            };
            left = self.binary(operator, left, Self::multiplicative)?;
        }
    }

    fn multiplicative(&mut self) -> GraphResult<Expr> {
        let mut left = self.power()?;
        loop {
            let operator = if self.take_symbol("*") {
                Operator::Multiply
            } else if self.take_symbol("/") {
                Operator::Divide
            } else if self.take_symbol("%") {
                Operator::Modulo
            } else {
                return Ok(left);
            };
            left = self.binary(operator, left, Self::power)?;
        }
    }

    fn power(&mut self) -> GraphResult<Expr> {
        let left = self.unary()?;
        if self.take_symbol("^") {
            return self.binary(Operator::Power, left, Self::unary);
        }
        Ok(left)
    }

    fn unary(&mut self) -> GraphResult<Expr> {
        if self.take_symbol("-") {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err(syntax("the expression is nested too deeply"));
            }
            let inner = self.unary();
            self.depth -= 1;
            return Ok(Expr::Negate(Box::new(inner?)));
        }
        self.take_symbol("+");
        self.postfix()
    }

    fn postfix(&mut self) -> GraphResult<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.take_symbol(".") {
                self.count_operator()?;
                expr = Expr::Property(Box::new(expr), self.name()?);
            } else if self.take_symbol("[") {
                self.count_operator()?;
                let index = self.expression()?;
                self.expect_symbol("]")?;
                expr = Expr::Index(Box::new(expr), Box::new(index));
            } else if self.is_symbol(":") {
                self.count_operator()?;
                let labels = self.label_expression(self.bar_ends_part)?;
                return Ok(Expr::LabelPredicate(Box::new(expr), labels));
            } else {
                return Ok(expr);
            }
        }
    }

    fn primary(&mut self) -> GraphResult<Expr> {
        let Some(token) = self.peek().cloned() else {
            return Err(self.unexpected());
        };
        match token {
            Token::Integer(number) => {
                self.at += 1;
                Ok(Expr::Literal(Value::Integer(number)))
            }
            Token::Float(number) => {
                self.at += 1;
                Ok(Expr::Literal(Value::Float(number)))
            }
            Token::Text(text) => {
                self.at += 1;
                Ok(Expr::Literal(Value::String(text)))
            }
            Token::Parameter(name) => {
                self.at += 1;
                Ok(Expr::Parameter(name))
            }
            Token::Symbol("(") => {
                self.at += 1;
                let inner = self.expression()?;
                self.expect_symbol(")")?;
                Ok(inner)
            }
            Token::Symbol("[") => {
                if let Some(comprehension) = self.pattern_comprehension()? {
                    return Ok(comprehension);
                }
                self.at += 1;
                let names_a_variable = match self.peek() {
                    Some(Token::Quoted(_)) => true,
                    Some(Token::Word(word)) => !["true", "false", "null"]
                        .iter()
                        .any(|literal| word.eq_ignore_ascii_case(literal)),
                    _ => false,
                };
                if names_a_variable && self.is_keyword_at(1, "in") {
                    return self.list_comprehension();
                }
                let mut items = Vec::new();
                if !self.take_symbol("]") {
                    loop {
                        items.push(self.expression()?);
                        if !self.take_symbol(",") {
                            break;
                        }
                    }
                    self.expect_symbol("]")?;
                }
                Ok(Expr::List(items))
            }
            Token::Symbol("{") => Ok(Expr::Map(self.map_literal()?)),
            Token::Quoted(name) => {
                self.at += 1;
                Ok(Expr::Variable(name))
            }
            Token::Word(word) => {
                let lower = word.to_ascii_lowercase();
                match lower.as_str() {
                    "true" | "false" => {
                        self.at += 1;
                        Ok(Expr::Literal(Value::Boolean(lower == "true")))
                    }
                    "null" => {
                        self.at += 1;
                        Ok(Expr::Literal(Value::Null))
                    }
                    "case" => self.case(),
                    "all" | "any" | "none" | "single"
                        if matches!(self.peek_at(1), Some(Token::Symbol("(")))
                            && matches!(self.peek_at(2), Some(Token::Word(_) | Token::Quoted(_)))
                            && matches!(self.peek_at(3), Some(Token::Word(word)) if word.eq_ignore_ascii_case("in")) =>
                    {
                        self.quantified(&lower)
                    }
                    "shortestpath" | "allshortestpaths"
                        if matches!(self.peek_at(1), Some(Token::Symbol("("))) =>
                    {
                        Ok(Expr::ShortestPath(Box::new(self.pattern()?)))
                    }
                    _ if matches!(self.peek_at(1), Some(Token::Symbol("("))) => self.call(lower),
                    _ => {
                        self.at += 1;
                        Ok(Expr::Variable(word))
                    }
                }
            }
            Token::Symbol(_) => Err(self.unexpected()),
        }
    }

    fn case(&mut self) -> GraphResult<Expr> {
        self.expect_keyword("case")?;
        let subject = if self.is_keyword("when") {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        let mut arms = Vec::new();
        while self.take_keyword("when") {
            let when = self.expression()?;
            self.expect_keyword("then")?;
            arms.push((when, self.expression()?));
        }
        let otherwise = if self.take_keyword("else") {
            Some(Box::new(self.expression()?))
        } else {
            None
        };
        self.expect_keyword("end")?;
        Ok(Expr::Case {
            subject,
            arms,
            otherwise,
        })
    }

    fn call(&mut self, mut name: String) -> GraphResult<Expr> {
        self.at += 2;
        if self.take_symbol("*") {
            self.expect_symbol(")")?;
            return Ok(Expr::Call {
                name,
                distinct: false,
                arguments: Vec::new(),
                star: true,
            });
        }
        let distinct = self.take_keyword("distinct");
        let mut arguments = Vec::new();
        if !self.take_symbol(")") {
            loop {
                arguments.push(self.expression()?);
                if !self.take_symbol(",") {
                    break;
                }
            }
            self.expect_symbol(")")?;
        }
        name.make_ascii_lowercase();
        Ok(Expr::Call {
            name,
            distinct,
            arguments,
            star: false,
        })
    }

    fn projecting(&mut self, returning: bool) -> GraphResult<Projecting> {
        let distinct = self.take_keyword("distinct");
        let mut star = false;
        let mut items = Vec::new();
        if self.take_symbol("*") {
            star = true;
        } else {
            loop {
                let start = self.offset();
                let expr = self.expression()?;
                let end = self
                    .tokens
                    .get(self.at)
                    .map_or(self.text.len(), |(_, offset)| *offset);
                let name = if self.take_keyword("as") {
                    self.name()?
                } else {
                    match &expr {
                        Expr::Variable(name) => name.clone(),
                        _ if !returning => {
                            return Err(syntax("Expression in WITH must be aliased (use AS)"));
                        }
                        _ => self.text[start..end].trim().to_owned(),
                    }
                };
                items.push(Projection { expr, name });
                if !self.take_symbol(",") {
                    break;
                }
            }
        }
        let mut order = Vec::new();
        if self.is_keyword("order") && self.is_keyword_at(1, "by") {
            self.at += 2;
            loop {
                let expr = self.expression()?;
                let descending = self.take_keyword("desc") || self.take_keyword("descending");
                if !descending {
                    let _ = self.take_keyword("asc") || self.take_keyword("ascending");
                }
                order.push((expr, descending));
                if !self.take_symbol(",") {
                    break;
                }
            }
        }
        let skip = if self.take_keyword("skip") {
            Some(self.expression()?)
        } else {
            None
        };
        let limit = if self.take_keyword("limit") {
            Some(self.expression()?)
        } else {
            None
        };
        let filter = if !returning && self.take_keyword("where") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(Projecting {
            distinct,
            star,
            items,
            order,
            skip,
            limit,
            filter,
        })
    }

    fn set_items(&mut self, removing: bool) -> GraphResult<Vec<SetItem>> {
        let mut items = Vec::new();
        loop {
            let variable = self.name()?;
            if self.is_symbol(":") {
                let mut labels = Vec::new();
                while self.take_symbol(":") {
                    labels.push(self.name()?);
                }
                items.push(SetItem::Labels(variable, labels));
            } else if !removing && (self.is_symbol("=") || self.is_symbol("+=")) {
                let merge = self.take_symbol("+=");
                if !merge {
                    self.expect_symbol("=")?;
                }
                items.push(SetItem::Replace(variable, self.expression()?, merge));
            } else {
                self.expect_symbol(".")?;
                let property = self.name()?;
                let value = if removing {
                    Expr::Literal(Value::Null)
                } else {
                    self.expect_symbol("=")?;
                    self.expression()?
                };
                items.push(SetItem::Property(Expr::Variable(variable), property, value));
            }
            if !self.take_symbol(",") {
                return Ok(items);
            }
        }
    }

    fn clause(&mut self) -> GraphResult<Clause> {
        if self.take_keyword("optional") {
            self.expect_keyword("match")?;
            return self.match_rest(true);
        }
        if self.take_keyword("match") {
            return self.match_rest(false);
        }
        if self.take_keyword("create") {
            let patterns = self.patterns()?;
            if patterns.iter().any(|pattern| pattern.shortest.is_some()) {
                return Err(syntax("shortestPath(...) cannot be used to CREATE"));
            }
            refuse_label_alternatives(&patterns)?;
            return Ok(Clause::Create(patterns));
        }
        if self.take_keyword("merge") {
            let pattern = self.pattern()?;
            if pattern.shortest.is_some() {
                return Err(syntax("shortestPath(...) cannot be used to MERGE"));
            }
            refuse_label_alternatives(std::slice::from_ref(&pattern))?;
            let mut on_create = Vec::new();
            let mut on_match = Vec::new();
            while self.take_keyword("on") {
                let creating = self.take_keyword("create");
                if !creating {
                    self.expect_keyword("match")?;
                }
                self.expect_keyword("set")?;
                let items = self.set_items(false)?;
                if creating {
                    on_create.extend(items);
                } else {
                    on_match.extend(items);
                }
            }
            return Ok(Clause::Merge {
                pattern,
                on_create,
                on_match,
            });
        }
        if self.take_keyword("foreach") {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err(syntax("FOREACH is nested too deeply"));
            }
            let parsed = self.foreach_rest();
            self.depth -= 1;
            return parsed;
        }
        if self.take_keyword("set") {
            return Ok(Clause::Set(self.set_items(false)?));
        }
        if self.take_keyword("remove") {
            return Ok(Clause::Remove(self.set_items(true)?));
        }
        let detach = self.take_keyword("detach");
        if self.take_keyword("delete") {
            let mut targets = vec![self.expression()?];
            while self.take_symbol(",") {
                targets.push(self.expression()?);
            }
            return Ok(Clause::Delete { detach, targets });
        }
        if detach {
            return Err(self.unexpected());
        }
        if self.take_keyword("unwind") {
            let list = self.expression()?;
            self.expect_keyword("as")?;
            return Ok(Clause::Unwind(list, self.name()?));
        }
        if self.take_keyword("with") {
            return Ok(Clause::With(self.projecting(false)?));
        }
        if self.take_keyword("return") {
            return Ok(Clause::Return(self.projecting(true)?));
        }
        if self.take_keyword("call") {
            return self.call_rest();
        }
        if self.is_keyword("load") {
            return Err(GraphError::not_supported("LOAD CSV"));
        }
        Err(self.unexpected())
    }

    fn foreach_rest(&mut self) -> GraphResult<Clause> {
        self.expect_symbol("(")?;
        let variable = self.name()?;
        self.expect_keyword("in")?;
        let list = self.expression_ended_by_bar()?;
        self.expect_symbol("|")?;
        let mut body = Vec::new();
        while !self.take_symbol(")") {
            if self.peek().is_none() {
                return Err(self.unexpected());
            }
            let clause = self.clause()?;
            if !clause.writes() || matches!(clause, Clause::Subquery(_)) {
                return Err(syntax(
                    "Invalid use of a reading clause inside FOREACH: only CREATE, MERGE, SET, REMOVE, DELETE and FOREACH can be used",
                ));
            }
            body.push(clause);
        }
        if body.is_empty() {
            return Err(syntax("FOREACH needs at least one updating clause"));
        }
        Ok(Clause::Foreach {
            variable,
            list,
            body,
        })
    }

    fn scope_imports(&mut self) -> GraphResult<Imports> {
        self.expect_symbol("(")?;
        if self.take_symbol("*") {
            self.expect_symbol(")")?;
            return Ok(Imports::Everything);
        }
        let mut names = Vec::new();
        if !self.take_symbol(")") {
            loop {
                let name = self.name()?;
                if names.contains(&name) {
                    return Err(semantic(format!(
                        "Variable `{name}` is imported into the CALL subquery twice"
                    )));
                }
                names.push(name);
                if !self.take_symbol(",") {
                    break;
                }
            }
            self.expect_symbol(")")?;
        }
        Ok(Imports::Variables(names))
    }

    fn in_transactions(&mut self) -> GraphResult<()> {
        if matches!(self.peek(), Some(Token::Integer(_))) {
            self.at += 1;
            self.expect_keyword("concurrent")?;
        } else {
            let _ = self.take_keyword("concurrent");
        }
        self.expect_keyword("transactions")?;
        if self.take_keyword("of") {
            match self.peek() {
                Some(Token::Integer(rows)) if *rows > 0 => self.at += 1,
                _ => return Err(syntax("OF needs a positive number of rows")),
            }
            if !self.take_keyword("rows") {
                self.expect_keyword("row")?;
            }
        }
        if self.take_keyword("on") {
            self.expect_keyword("error")?;
            if !self.take_keyword("fail") {
                return Err(GraphError::not_supported(
                    "CALL { ... } IN TRANSACTIONS ON ERROR CONTINUE or BREAK",
                ));
            }
        }
        if self.is_keyword("report") {
            return Err(GraphError::not_supported(
                "CALL { ... } IN TRANSACTIONS REPORT STATUS",
            ));
        }
        Ok(())
    }

    fn subquery_rest(&mut self) -> GraphResult<Clause> {
        self.subquery_with_scope(None)
    }

    fn subquery_with_scope(&mut self, scope: Option<Imports>) -> GraphResult<Clause> {
        self.expect_symbol("{")?;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(syntax("CALL subqueries are nested too deeply"));
        }
        let body = self.query();
        self.depth -= 1;
        let body = body?;
        self.expect_symbol("}")?;
        if self.take_keyword("in") {
            self.in_transactions()?;
        }
        let parts: Vec<SubqueryPart> = match scope {
            None => body
                .parts
                .into_iter()
                .map(subquery_part)
                .collect::<GraphResult<_>>()?,
            Some(imports) => {
                let mut parts = Vec::with_capacity(body.parts.len());
                for clauses in body.parts {
                    if let Some(Clause::With(projecting)) = clauses.first()
                        && importing_with(projecting)?.is_some()
                    {
                        return Err(semantic(
                            "Importing WITH is not allowed in a CALL subquery with a variable scope clause",
                        ));
                    }
                    parts.push(SubqueryPart {
                        imports: imports.clone(),
                        clauses,
                    });
                }
                parts
            }
        };
        Ok(Clause::Subquery(Box::new(Subquery {
            parts,
            union_all: body.union_all,
        })))
    }

    fn call_rest(&mut self) -> GraphResult<Clause> {
        if self.is_symbol("{") {
            return self.subquery_rest();
        }
        if self.is_symbol("(") {
            let imports = self.scope_imports()?;
            return self.subquery_with_scope(Some(imports));
        }
        let mut procedure = self.name()?;
        while self.take_symbol(".") {
            procedure.push('.');
            procedure.push_str(&self.name()?);
        }
        self.expect_symbol("(")?;
        let mut arguments = Vec::new();
        if !self.take_symbol(")") {
            loop {
                arguments.push(self.expression()?);
                if !self.take_symbol(",") {
                    break;
                }
            }
            self.expect_symbol(")")?;
        }
        let mut yields = Vec::new();
        if self.take_keyword("yield") {
            loop {
                let column = self.name()?;
                let alias = if self.take_keyword("as") {
                    self.name()?
                } else {
                    column.clone()
                };
                yields.push((column, alias));
                if !self.take_symbol(",") {
                    break;
                }
            }
        }
        Ok(Clause::Call {
            procedure,
            arguments,
            yields,
        })
    }

    fn match_rest(&mut self, optional: bool) -> GraphResult<Clause> {
        let patterns = self.patterns()?;
        let filter = if self.take_keyword("where") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(Clause::Match {
            optional,
            patterns,
            filter,
        })
    }

    fn ends_a_part(&self) -> bool {
        self.peek().is_none()
            || self.is_symbol(";")
            || self.is_symbol("}")
            || self.is_keyword("union")
    }

    fn query(&mut self) -> GraphResult<Query> {
        let mut parts = Vec::new();
        let mut union_all = None;
        loop {
            let mut clauses = Vec::new();
            while !self.ends_a_part() {
                clauses.push(self.clause()?);
            }
            check_part(&clauses)?;
            parts.push(clauses);
            if !self.take_keyword("union") {
                break;
            }
            let all = self.take_keyword("all");
            if union_all.is_some_and(|known| known != all) {
                return Err(syntax("Invalid combination of UNION and UNION ALL"));
            }
            union_all = Some(all);
        }
        if parts.len() > 1
            && parts.iter().any(|clauses| {
                !matches!(
                    clauses.last(),
                    Some(Clause::Return(_) | Clause::Call { .. })
                )
            })
        {
            return Err(syntax(
                "All sub queries in an UNION must end with a RETURN clause",
            ));
        }
        Ok(Query {
            parts,
            union_all: union_all.unwrap_or(false),
        })
    }
}

/// Parses a query into its parts and clauses.
pub(crate) fn parse(text: &str) -> GraphResult<Query> {
    let tokens = tokenize(text)?;
    let mut parser = Parser {
        text,
        tokens,
        at: 0,
        depth: 0,
        operators: 0,
        bar_ends_part: false,
    };
    let query = parser.query()?;
    parser.take_symbol(";");
    if parser.peek().is_some() {
        return Err(parser.unexpected());
    }
    Ok(query)
}

fn check_part(clauses: &[Clause]) -> GraphResult<()> {
    if clauses.is_empty() {
        return Err(syntax("an empty query"));
    }
    let last = clauses.last();
    if !matches!(last, Some(Clause::Return(_) | Clause::Call { .. }))
        && !clauses.iter().any(Clause::writes)
    {
        return Err(syntax(
            "Query cannot conclude with MATCH, WITH or UNWIND (must be a RETURN clause or an update clause)",
        ));
    }
    if clauses
        .iter()
        .rev()
        .skip(1)
        .any(|clause| matches!(clause, Clause::Return(_)))
    {
        return Err(syntax("RETURN can only be used at the end of the query"));
    }
    Ok(())
}
