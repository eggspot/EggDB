use super::oids;
use super::snapshot::CatalogSnapshot;
use crate::ast::{BinaryOperator, Expr, ExprKind, Literal, TruthTest, UnaryOperator};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::pg_keywords::quote_ident;
use crate::types::SqlType;

const CLOCK_FUNCTIONS: [(&str, &str, SqlType); 5] = [
    ("current_date", "CURRENT_DATE", SqlType::Date),
    (
        "current_timestamp",
        "CURRENT_TIMESTAMP",
        SqlType::TimestampTz(None),
    ),
    ("localtimestamp", "LOCALTIMESTAMP", SqlType::Timestamp(None)),
    ("current_time", "CURRENT_TIME", SqlType::TimeTz(None)),
    ("localtime", "LOCALTIME", SqlType::Time(None)),
];

const TEXT_FUNCTIONS: [&str; 9] = [
    "lower",
    "upper",
    "length",
    "char_length",
    "character_length",
    "btrim",
    "ltrim",
    "rtrim",
    "md5",
];

pub(crate) fn default_text(snapshot: &CatalogSnapshot, table_at: usize, column: usize) -> String {
    let Some(table) = snapshot
        .relations
        .get(table_at)
        .and_then(|entry| entry.table())
    else {
        return String::new();
    };
    let Some(column) = table.columns.get(column) else {
        return String::new();
    };
    let Some(stored) = &column.default else {
        return String::new();
    };
    let deparser = Deparser { table, snapshot };
    deparser
        .default(stored, column.sql_type)
        .unwrap_or_else(|| stored.clone())
}

pub(crate) fn check_text(
    snapshot: &CatalogSnapshot,
    table_at: usize,
    check: usize,
    pretty: bool,
) -> String {
    let Some(table) = snapshot
        .relations
        .get(table_at)
        .and_then(|entry| entry.table())
    else {
        return String::new();
    };
    let Some(check) = table.checks.get(check) else {
        return String::new();
    };
    let deparser = Deparser { table, snapshot };
    let shown = if check.dialect == Dialect::PostgreSql {
        deparser.check(&check.expression)
    } else {
        None
    };
    match (shown, pretty) {
        (Some(shown), true) => shown,
        (Some(shown), false) => format!("({shown})"),
        (None, _) => check.expression.clone(),
    }
}

#[derive(Debug, Clone)]
enum Node {
    Atom(String),
    Cast {
        operand: Box<Node>,
        type_name: String,
    },
    Arith {
        symbol: &'static str,
        precedence: u8,
        left: Box<Node>,
        right: Box<Node>,
    },
    Negate(Box<Node>),
    Compare {
        symbol: &'static str,
        left: Box<Node>,
        right: Box<Node>,
    },
    Quantified {
        left: Box<Node>,
        symbol: &'static str,
        all: bool,
        array: Box<Node>,
    },
    Array {
        items: Vec<Node>,
        cast: Option<String>,
    },
    Not(Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
    Test {
        operand: Box<Node>,
        suffix: &'static str,
    },
}

impl Node {
    fn is_complex(&self) -> bool {
        !matches!(self, Self::Atom(_))
    }

    fn print(&self) -> String {
        match self {
            Self::Atom(text) => text.clone(),
            Self::Cast { operand, type_name } => {
                if operand.is_complex() {
                    format!("({})::{type_name}", operand.print())
                } else {
                    format!("{}::{type_name}", operand.print())
                }
            }
            Self::Arith {
                symbol,
                precedence,
                left,
                right,
            } => {
                let left_wrapped = match left.as_ref() {
                    Self::Arith {
                        precedence: inner, ..
                    } => inner < precedence,
                    other => other.needs_operand_parens(),
                };
                let right_wrapped = match right.as_ref() {
                    Self::Arith {
                        precedence: inner, ..
                    } => inner <= precedence,
                    other => other.needs_operand_parens(),
                };
                format!(
                    "{} {symbol} {}",
                    wrapped(left, left_wrapped),
                    wrapped(right, right_wrapped)
                )
            }
            Self::Negate(operand) => {
                format!("- {}", wrapped(operand, operand.is_complex()))
            }
            Self::Compare {
                symbol,
                left,
                right,
            } => format!(
                "{} {symbol} {}",
                comparison_operand(left),
                comparison_operand(right)
            ),
            Self::Quantified {
                left,
                symbol,
                all,
                array,
            } => format!(
                "{} {symbol} {} ({})",
                comparison_operand(left),
                if *all { "ALL" } else { "ANY" },
                array.print()
            ),
            Self::Array { items, cast } => {
                let listed: Vec<String> = items.iter().map(Self::print).collect();
                match cast {
                    Some(type_name) => format!("ARRAY[{}]::{type_name}[]", listed.join(", ")),
                    None => format!("ARRAY[{}]", listed.join(", ")),
                }
            }
            Self::Not(operand) => format!(
                "NOT {}",
                wrapped(operand, matches!(**operand, Self::And(_) | Self::Or(_)))
            ),
            Self::And(items) => items
                .iter()
                .map(|item| wrapped(item, matches!(item, Self::Or(_))))
                .collect::<Vec<_>>()
                .join(" AND "),
            Self::Or(items) => items
                .iter()
                .map(Self::print)
                .collect::<Vec<_>>()
                .join(" OR "),
            Self::Test { operand, suffix } => format!(
                "{}{suffix}",
                wrapped(
                    operand,
                    !matches!(**operand, Self::Atom(_) | Self::Cast { .. })
                )
            ),
        }
    }

    fn needs_operand_parens(&self) -> bool {
        match self {
            Self::Atom(_) => false,
            Self::Cast { operand, .. } => operand.is_complex(),
            _ => true,
        }
    }
}

fn wrapped(node: &Node, wrap: bool) -> String {
    if wrap {
        format!("({})", node.print())
    } else {
        node.print()
    }
}

fn comparison_operand(node: &Node) -> String {
    wrapped(node, node.needs_operand_parens())
}

#[derive(Debug, Clone)]
enum Piece {
    Typed(Node, SqlType),
    Untyped(Node),
    Text(String),
    Integer(i64),
    Decimal(String),
}

fn integer_type(value: i64) -> SqlType {
    if i32::try_from(value).is_ok() {
        SqlType::Integer
    } else {
        SqlType::BigInt
    }
}

impl Piece {
    fn static_type(&self) -> Option<SqlType> {
        match self {
            Self::Typed(_, sql_type) => Some(*sql_type),
            Self::Untyped(_) | Self::Text(_) => None,
            Self::Integer(value) => Some(integer_type(*value)),
            Self::Decimal(_) => Some(SqlType::Numeric(None)),
        }
    }

    fn own(self) -> Node {
        match self {
            Self::Typed(node, _) | Self::Untyped(node) => node,
            Self::Text(text) => string_constant(&text, SqlType::Text),
            Self::Integer(value) => constant(&value.to_string(), integer_type(value)),
            Self::Decimal(text) => constant(&text, SqlType::Numeric(None)),
        }
    }

    fn coerce(self, target: SqlType) -> Node {
        match self {
            Self::Typed(node, sql_type) => {
                if oids::type_oid(sql_type) == oids::type_oid(target) {
                    node
                } else {
                    Node::Cast {
                        operand: Box::new(node),
                        type_name: type_name(target),
                    }
                }
            }
            Self::Untyped(node) => node,
            Self::Text(text) => string_constant(&text, target),
            Self::Integer(value) => {
                if is_integer(target) {
                    constant(&value.to_string(), integer_type(value))
                } else {
                    constant(&value.to_string(), target)
                }
            }
            Self::Decimal(text) => constant(&text, target),
        }
    }

    fn element_of(self, target: SqlType) -> Node {
        match self {
            Self::Integer(value) => constant(&value.to_string(), target),
            other => other.coerce(target),
        }
    }
}

fn type_name(sql_type: SqlType) -> String {
    oids::format_type(oids::type_oid(sql_type), Some(-1))
}

fn is_integer(sql_type: SqlType) -> bool {
    matches!(
        sql_type,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
    )
}

fn plain_number(text: &str) -> bool {
    let mut points = 0;
    !text.is_empty()
        && text.bytes().all(|byte| {
            if byte == b'.' {
                points += 1;
                true
            } else {
                byte.is_ascii_digit()
            }
        })
        && points <= 1
}

fn constant(text: &str, sql_type: SqlType) -> Node {
    let name = type_name(sql_type);
    let fits_integer = text.bytes().all(|byte| byte.is_ascii_digit())
        && text
            .parse::<i64>()
            .is_ok_and(|value| i32::try_from(value).is_ok());
    let decimal = plain_number(text) && text.contains('.');
    let bare = match sql_type {
        SqlType::Integer => fits_integer,
        SqlType::Numeric(_) => decimal,
        _ => false,
    };
    if bare {
        return Node::Atom(text.to_owned());
    }
    if fits_integer || decimal {
        return Node::Atom(format!("{text}::{name}"));
    }
    Node::Atom(format!("'{text}'::{name}"))
}

fn string_constant(text: &str, sql_type: SqlType) -> Node {
    let canonical = if sql_type.is_character() {
        text.to_owned()
    } else {
        crate::types::parse_input(text, sql_type.without_modifier())
            .ok()
            .and_then(|value| value.to_text())
            .unwrap_or_else(|| text.to_owned())
    };
    Node::Atom(format!(
        "'{}'::{}",
        canonical.replace('\'', "''"),
        type_name(sql_type)
    ))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    SmallInt,
    Integer,
    BigInt,
    Numeric,
    Real,
    Double,
}

fn rank(sql_type: SqlType) -> Option<Rank> {
    Some(match sql_type {
        SqlType::SmallInt => Rank::SmallInt,
        SqlType::Integer => Rank::Integer,
        SqlType::BigInt => Rank::BigInt,
        SqlType::Numeric(_) => Rank::Numeric,
        SqlType::Real => Rank::Real,
        SqlType::Double => Rank::Double,
        _ => return None,
    })
}

fn is_float(rank: Rank) -> bool {
    matches!(rank, Rank::Real | Rank::Double)
}

fn operand_targets(left: SqlType, right: SqlType) -> (SqlType, SqlType) {
    if left.is_character() && right.is_character() {
        let shown = |own: SqlType, other: SqlType| match own {
            SqlType::Varchar(_) => SqlType::Text,
            SqlType::Bpchar(_) if !matches!(other, SqlType::Bpchar(_)) => SqlType::Text,
            other => other,
        };
        return (shown(left, right), shown(right, left));
    }
    let (Some(left_rank), Some(right_rank)) = (rank(left), rank(right)) else {
        return (left, right);
    };
    let integer = |rank: Rank| rank <= Rank::BigInt;
    if (integer(left_rank) && integer(right_rank)) || (is_float(left_rank) && is_float(right_rank))
    {
        return (left, right);
    }
    if is_float(left_rank) {
        return (left, SqlType::Double);
    }
    if is_float(right_rank) {
        return (SqlType::Double, right);
    }
    if left_rank == Rank::Numeric {
        (left, SqlType::Numeric(None))
    } else {
        (SqlType::Numeric(None), right)
    }
}

fn unknown_target(other: SqlType) -> SqlType {
    match other {
        SqlType::Varchar(_) => SqlType::Text,
        other => other,
    }
}

fn arithmetic_result(left: SqlType, right: SqlType) -> SqlType {
    match (rank(left), rank(right)) {
        (Some(a), Some(b)) if a >= b => left,
        (Some(_), Some(_)) => right,
        _ => left,
    }
}

fn promoted(element: SqlType, other: SqlType) -> SqlType {
    if element.is_character() || other.is_character() {
        return element;
    }
    match (rank(element), rank(other)) {
        (Some(a), Some(b)) if is_float(a) || is_float(b) => SqlType::Double,
        (Some(a), Some(b)) if a >= b => element,
        (Some(_), Some(_)) => other,
        _ => element,
    }
}

struct Deparser<'a> {
    table: &'a TableDefinition,
    snapshot: &'a CatalogSnapshot,
}

fn parse(text: &str) -> Option<Expr> {
    let tokens = crate::lexer::tokenize_in(text, Dialect::PostgreSql).ok()?;
    let mut parser = crate::parser::Parser::new(text, &tokens).in_dialect(Dialect::PostgreSql);
    let expr = parser.expression().ok()?;
    parser.at_end().then_some(expr)
}

fn literal_of_cast(expr: &Expr, target: SqlType) -> Option<Piece> {
    let ExprKind::Literal(literal) = &expr.kind else {
        return None;
    };
    let numeric = is_integer(target) || matches!(target, SqlType::Numeric(_));
    Some(match literal {
        Literal::String(text) if numeric => number_piece(text.trim())?,
        Literal::String(text) => Piece::Text(text.clone()),
        Literal::Integer(value) => Piece::Integer(*value),
        Literal::Decimal(text) => Piece::Decimal(text.clone()),
        Literal::Boolean(flag) => Piece::Typed(Node::Atom(flag.to_string()), SqlType::Boolean),
        Literal::Null => Piece::Untyped(Node::Atom("NULL".to_owned())),
    })
}

fn number_piece(text: &str) -> Option<Piece> {
    if let Ok(value) = text.parse::<i64>() {
        return Some(Piece::Integer(value));
    }
    let digits = text.strip_prefix('-').unwrap_or(text);
    plain_number(digits).then(|| Piece::Decimal(text.to_owned()))
}

fn negated_piece(piece: Piece) -> Option<Piece> {
    Some(match piece {
        Piece::Integer(value) => Piece::Integer(value.checked_neg()?),
        Piece::Decimal(text) => match text.strip_prefix('-') {
            Some(positive) => Piece::Decimal(positive.to_owned()),
            None => Piece::Decimal(format!("-{text}")),
        },
        Piece::Typed(node, sql_type) => Piece::Typed(Node::Negate(Box::new(node)), sql_type),
        Piece::Untyped(node) => Piece::Untyped(Node::Negate(Box::new(node))),
        Piece::Text(_) => return None,
    })
}

impl Deparser<'_> {
    fn check(&self, text: &str) -> Option<String> {
        let expr = parse(text)?;
        Some(self.piece(&expr)?.own().print())
    }

    fn default(&self, text: &str, sql_type: SqlType) -> Option<String> {
        let expr = parse(text)?;
        let node = match self.piece(&expr)? {
            Piece::Text(text) => string_constant(&text, sql_type),
            other => other.own(),
        };
        Some(node.print())
    }

    fn column(&self, name: &str) -> Option<Piece> {
        let column = self
            .table
            .columns
            .iter()
            .find(|column| column.name == name)?;
        Some(Piece::Typed(Node::Atom(quote_ident(name)), column.sql_type))
    }

    fn piece(&self, expr: &Expr) -> Option<Piece> {
        match &expr.kind {
            ExprKind::Literal(literal) => Some(match literal {
                Literal::String(text) => Piece::Text(text.clone()),
                Literal::Integer(value) => Piece::Integer(*value),
                Literal::Decimal(text) => Piece::Decimal(text.clone()),
                Literal::Boolean(flag) => {
                    Piece::Typed(Node::Atom(flag.to_string()), SqlType::Boolean)
                }
                Literal::Null => Piece::Untyped(Node::Atom("NULL".to_owned())),
            }),
            ExprKind::Column {
                qualifier: None,
                name,
            } => self.column(name),
            ExprKind::Cast { expr, sql_type } => self.cast(expr, *sql_type),
            ExprKind::Unary(UnaryOperator::Minus, operand) => negated_piece(self.piece(operand)?),
            ExprKind::Unary(UnaryOperator::Plus, operand) => self.piece(operand),
            ExprKind::Unary(UnaryOperator::Not, operand) => Some(Piece::Typed(
                Node::Not(Box::new(self.boolean(operand)?)),
                SqlType::Boolean,
            )),
            ExprKind::And(items) => Some(Piece::Typed(
                Node::And(
                    items
                        .iter()
                        .map(|item| self.boolean(item))
                        .collect::<Option<_>>()?,
                ),
                SqlType::Boolean,
            )),
            ExprKind::Or(items) => Some(Piece::Typed(
                Node::Or(
                    items
                        .iter()
                        .map(|item| self.boolean(item))
                        .collect::<Option<_>>()?,
                ),
                SqlType::Boolean,
            )),
            ExprKind::Binary(operator, left, right) => self.binary(*operator, left, right),
            ExprKind::IsNull { expr, negated } => Some(Piece::Typed(
                Node::Test {
                    operand: Box::new(self.piece(expr)?.own()),
                    suffix: if *negated { " IS NOT NULL" } else { " IS NULL" },
                },
                SqlType::Boolean,
            )),
            ExprKind::IsTruth {
                expr,
                test,
                negated,
            } => Some(Piece::Typed(
                Node::Test {
                    operand: Box::new(self.boolean(expr)?),
                    suffix: match (test, negated) {
                        (TruthTest::True, false) => " IS TRUE",
                        (TruthTest::True, true) => " IS NOT TRUE",
                        (TruthTest::False, false) => " IS FALSE",
                        (TruthTest::False, true) => " IS NOT FALSE",
                        (TruthTest::Unknown, false) => " IS UNKNOWN",
                        (TruthTest::Unknown, true) => " IS NOT UNKNOWN",
                    },
                },
                SqlType::Boolean,
            )),
            ExprKind::IsDistinctFrom {
                left,
                right,
                negated,
            } => self.comparison(
                if *negated {
                    "IS NOT DISTINCT FROM"
                } else {
                    "IS DISTINCT FROM"
                },
                left,
                right,
            ),
            ExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let (first, second) = if *negated { ("<", ">") } else { (">=", "<=") };
                let both = vec![
                    self.comparison_node(first, expr, low)?,
                    self.comparison_node(second, expr, high)?,
                ];
                let joined = if *negated {
                    Node::Or(both)
                } else {
                    Node::And(both)
                };
                Some(Piece::Typed(joined, SqlType::Boolean))
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => self.in_list(expr, list, *negated),
            ExprKind::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => self.comparison(
                match (*negated, *case_insensitive) {
                    (false, false) => "~~",
                    (true, false) => "!~~",
                    (false, true) => "~~*",
                    (true, true) => "!~~*",
                },
                expr,
                pattern,
            ),
            ExprKind::Function { name, arguments } => self.function(name, arguments),
            _ => None,
        }
    }

    fn boolean(&self, expr: &Expr) -> Option<Node> {
        Some(self.piece(expr)?.own())
    }

    fn cast(&self, expr: &Expr, sql_type: SqlType) -> Option<Piece> {
        if let Some(literal) = literal_of_cast(expr, sql_type) {
            return Some(literal);
        }
        if let ExprKind::Cast {
            expr: inner,
            sql_type: inner_type,
        } = &expr.kind
            && literal_of_cast(inner, *inner_type).is_some()
        {
            return self.cast(inner, *inner_type);
        }
        let operand = self.piece(expr)?;
        if let (Piece::Typed(_, own), SqlType::Text) = (&operand, sql_type)
            && matches!(own, SqlType::Varchar(_) | SqlType::Bpchar(_))
        {
            return Some(operand);
        }
        Some(Piece::Typed(operand.coerce(sql_type), sql_type))
    }

    fn comparison_node(&self, symbol: &'static str, left: &Expr, right: &Expr) -> Option<Node> {
        let (left, right) = self.paired(self.piece(left)?, self.piece(right)?);
        Some(Node::Compare {
            symbol,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    fn comparison(&self, symbol: &'static str, left: &Expr, right: &Expr) -> Option<Piece> {
        Some(Piece::Typed(
            self.comparison_node(symbol, left, right)?,
            SqlType::Boolean,
        ))
    }

    fn paired(&self, left: Piece, right: Piece) -> (Node, Node) {
        match (left.static_type(), right.static_type()) {
            (None, None) => (left.coerce(SqlType::Text), right.coerce(SqlType::Text)),
            (None, Some(other)) => {
                let target = unknown_target(other);
                (left.coerce(target), right.coerce(target))
            }
            (Some(other), None) => {
                let target = unknown_target(other);
                (left.coerce(target), right.coerce(target))
            }
            (Some(left_type), Some(right_type)) => {
                let (left_target, right_target) = operand_targets(left_type, right_type);
                (left.coerce(left_target), right.coerce(right_target))
            }
        }
    }

    fn binary(&self, operator: BinaryOperator, left: &Expr, right: &Expr) -> Option<Piece> {
        let symbol = operator.symbol();
        match operator {
            BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessOrEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterOrEqual
            | BinaryOperator::RegexMatch
            | BinaryOperator::RegexMatchFolded
            | BinaryOperator::RegexNoMatch
            | BinaryOperator::RegexNoMatchFolded => self.comparison(symbol, left, right),
            BinaryOperator::Concat => {
                let text = |piece: Piece| match piece.static_type() {
                    Some(SqlType::Text) | None => piece.coerce(SqlType::Text),
                    Some(sql_type) if sql_type.is_character() => piece.coerce(SqlType::Text),
                    Some(_) => piece.own(),
                };
                Some(Piece::Typed(
                    Node::Arith {
                        symbol: "||",
                        precedence: 0,
                        left: Box::new(text(self.piece(left)?)),
                        right: Box::new(text(self.piece(right)?)),
                    },
                    SqlType::Text,
                ))
            }
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::Power => {
                let precedence = match operator {
                    BinaryOperator::Add | BinaryOperator::Subtract => 1,
                    BinaryOperator::Power => 3,
                    _ => 2,
                };
                let (left, right) = (self.piece(left)?, self.piece(right)?);
                let (left_type, right_type) = (left.static_type()?, right.static_type()?);
                let (left_target, right_target) = if operator == BinaryOperator::Power
                    && !matches!(left_type, SqlType::Numeric(_))
                    && !matches!(right_type, SqlType::Numeric(_))
                {
                    (SqlType::Double, SqlType::Double)
                } else {
                    operand_targets(left_type, right_type)
                };
                Some(Piece::Typed(
                    Node::Arith {
                        symbol,
                        precedence,
                        left: Box::new(left.coerce(left_target)),
                        right: Box::new(right.coerce(right_target)),
                    },
                    arithmetic_result(left_target, right_target),
                ))
            }
            _ => None,
        }
    }

    fn in_list(&self, expr: &Expr, list: &[Expr], negated: bool) -> Option<Piece> {
        let symbol = if negated { "<>" } else { "=" };
        if let [only] = list {
            return self.comparison(symbol, expr, only);
        }
        let left = self.piece(expr)?;
        let items: Vec<Piece> = list
            .iter()
            .map(|item| self.piece(item))
            .collect::<Option<_>>()?;
        let mut element = left.static_type()?;
        for item in &items {
            if let Some(found) = item.static_type() {
                element = promoted(element, found);
            }
        }
        let (left_target, array_cast) = match element {
            SqlType::Varchar(_) => (SqlType::Text, Some("text".to_owned())),
            _ => (
                left.static_type()
                    .filter(|own| is_integer(*own) && is_integer(element))
                    .unwrap_or(element),
                None,
            ),
        };
        let items: Vec<Node> = items
            .into_iter()
            .map(|item| item.element_of(element))
            .collect();
        Some(Piece::Typed(
            Node::Quantified {
                left: Box::new(left.coerce(left_target)),
                symbol,
                all: negated,
                array: Box::new(Node::Array {
                    items,
                    cast: array_cast,
                }),
            },
            SqlType::Boolean,
        ))
    }

    fn function(&self, name: &str, arguments: &[Expr]) -> Option<Piece> {
        if arguments.is_empty()
            && let Some((_, shown, sql_type)) = CLOCK_FUNCTIONS
                .iter()
                .find(|(internal, ..)| *internal == name)
        {
            return Some(Piece::Typed(Node::Atom((*shown).to_owned()), *sql_type));
        }
        if matches!(name, "nextval" | "currval" | "setval")
            && let Some((first, rest)) = arguments.split_first()
        {
            let sequence = self.sequence_argument(first)?;
            let mut shown = vec![sequence];
            for argument in rest {
                shown.push(self.piece(argument)?.own().print());
            }
            return Some(Piece::Typed(
                Node::Atom(format!("{name}({})", shown.join(", "))),
                SqlType::BigInt,
            ));
        }
        let text_arguments = TEXT_FUNCTIONS.contains(&name);
        let mut shown = Vec::with_capacity(arguments.len());
        let mut first_type = None;
        for argument in arguments {
            let piece = self.piece(argument)?;
            first_type = first_type.or_else(|| piece.static_type());
            let node = match piece.static_type() {
                None => piece.coerce(SqlType::Text),
                Some(sql_type) if text_arguments && sql_type.is_character() => {
                    piece.coerce(SqlType::Text)
                }
                Some(_) => piece.own(),
            };
            shown.push(node.print());
        }
        let (shown_name, result) = match name {
            "coalesce" => ("COALESCE".to_owned(), first_type),
            "lower" | "upper" | "btrim" | "ltrim" | "rtrim" | "md5" => {
                (name.to_owned(), Some(SqlType::Text))
            }
            "length" | "char_length" | "character_length" => {
                (name.to_owned(), Some(SqlType::Integer))
            }
            "abs" => (name.to_owned(), first_type),
            "now" => (name.to_owned(), Some(SqlType::TimestampTz(None))),
            _ => (name.to_owned(), None),
        };
        let node = Node::Atom(format!("{shown_name}({})", shown.join(", ")));
        Some(match result {
            Some(sql_type) => Piece::Typed(node, sql_type),
            None => Piece::Untyped(node),
        })
    }

    fn sequence_argument(&self, expr: &Expr) -> Option<String> {
        let ExprKind::Function { name, arguments } = &expr.kind else {
            return match &expr.kind {
                ExprKind::Literal(Literal::String(text)) => {
                    Some(format!("'{}'::text", text.replace('\'', "''")))
                }
                _ => None,
            };
        };
        if name != "regclass_cast" {
            return None;
        }
        let [argument] = arguments.as_slice() else {
            return None;
        };
        let ExprKind::Literal(Literal::String(text)) = &argument.kind else {
            return None;
        };
        let shown = super::types::split_qualified(text)
            .ok()
            .and_then(|(schema, name)| self.snapshot.find_relation(schema.as_deref(), &name))
            .map_or_else(|| text.clone(), |at| self.snapshot.display_name(at));
        Some(format!("'{}'::regclass", shown.replace('\'', "''")))
    }
}
