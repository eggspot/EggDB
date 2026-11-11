use std::rc::Rc;

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    Or,
    And,
    Less,
    Greater,
    LessEqual,
    GreaterEqual,
    NotEqual,
    Equal,
    Concat,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
}

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Not,
    Length,
    Negate,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Nil,
    True,
    False,
    Number(f64),
    String(Rc<[u8]>),
    VarArgs,
    Function(Rc<FunctionBody>),
    Table(Vec<TableField>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Name(String),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>, u32),
    Method(Box<Expr>, String, Vec<Expr>, u32),
    Paren(Box<Expr>),
}

/// One field of a table constructor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TableField {
    Positional(Expr),
    Named(String, Expr),
    Keyed(Expr, Expr),
}

/// A function's parameters and body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionBody {
    pub(crate) parameters: Vec<String>,
    pub(crate) variadic: bool,
    pub(crate) body: Block,
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stat {
    Local(Vec<String>, Vec<Expr>),
    Assign(Vec<Expr>, Vec<Expr>),
    Call(Expr),
    Do(Block),
    While(Expr, Block),
    Repeat(Block, Expr),
    If(Vec<(Expr, Block)>, Option<Block>),
    NumericFor {
        variable: String,
        start: Expr,
        limit: Expr,
        step: Option<Expr>,
        body: Block,
    },
    GenericFor {
        variables: Vec<String>,
        values: Vec<Expr>,
        body: Block,
    },
    Function(Vec<String>, Option<String>, Rc<FunctionBody>),
    LocalFunction(String, Rc<FunctionBody>),
    Return(Vec<Expr>),
    Break,
}

/// A block of statements.
pub(crate) type Block = Vec<Stat>;
