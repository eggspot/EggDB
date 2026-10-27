use crate::types::SqlType;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Statement {
    CreateTable(CreateTable),
    DropTable { names: Vec<Name>, if_exists: bool },
    Insert(Insert),
    Select(Select),
    Update(Update),
    Delete(Delete),
    Begin { read_only: bool },
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Name {
    pub(crate) text: String,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateTable {
    pub(crate) name: Name,
    pub(crate) if_not_exists: bool,
    pub(crate) columns: Vec<ColumnDefinition>,
    pub(crate) primary_key: Option<(Vec<Name>, usize)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnDefinition {
    pub(crate) name: Name,
    pub(crate) sql_type: SqlType,
    pub(crate) not_null: bool,
    pub(crate) primary_key: bool,
    pub(crate) default: Option<(Expr, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableReference {
    pub(crate) name: Name,
    pub(crate) alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InsertSource {
    Values(Vec<Vec<InsertValue>>),
    DefaultValues,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InsertValue {
    Expr(Expr),
    Default,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Insert {
    pub(crate) table: TableReference,
    pub(crate) columns: Option<Vec<Name>>,
    pub(crate) source: InsertSource,
    pub(crate) returning: Option<Vec<SelectItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Update {
    pub(crate) table: TableReference,
    pub(crate) assignments: Vec<(Name, InsertValue)>,
    pub(crate) filter: Option<Expr>,
    pub(crate) returning: Option<Vec<SelectItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Delete {
    pub(crate) table: TableReference,
    pub(crate) filter: Option<Expr>,
    pub(crate) returning: Option<Vec<SelectItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectItem {
    Wildcard {
        qualifier: Option<String>,
        position: usize,
    },
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OrderItem {
    pub(crate) expr: Expr,
    pub(crate) descending: bool,
    pub(crate) nulls_first: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Select {
    pub(crate) items: Vec<SelectItem>,
    pub(crate) from: Option<TableReference>,
    pub(crate) filter: Option<Expr>,
    pub(crate) order_by: Vec<OrderItem>,
    pub(crate) limit: Option<Expr>,
    pub(crate) offset: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
    Concat,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl BinaryOperator {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Modulo => "%",
            Self::Power => "^",
            Self::Concat => "||",
            Self::Equal => "=",
            Self::NotEqual => "<>",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOperator {
    Minus,
    Plus,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TruthTest {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Literal {
    Null,
    Boolean(bool),
    Integer(i64),
    Decimal(String),
    String(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Expr {
    pub(crate) kind: ExprKind,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExprKind {
    Literal(Literal),
    Column {
        qualifier: Option<String>,
        name: String,
    },
    Unary(UnaryOperator, Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Binary(BinaryOperator, Box<Expr>, Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    IsTruth {
        expr: Box<Expr>,
        test: TruthTest,
        negated: bool,
    },
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
    },
    Cast {
        expr: Box<Expr>,
        sql_type: SqlType,
    },
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Function {
        name: String,
        arguments: Vec<Expr>,
    },
}
