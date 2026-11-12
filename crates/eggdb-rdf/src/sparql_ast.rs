//! The syntax tree of SPARQL 1.1 queries and updates, close to the algebra of the SPARQL 1.1
//! Query Language recommendation, section 18.

use crate::term::Term;

/// A variable or an RDF term in a pattern.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Variable(String),
    Term(Term),
}

/// A property path (section 9).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Path {
    Iri(String),
    Inverse(Box<Path>),
    Sequence(Vec<Path>),
    Alternative(Vec<Path>),
    ZeroOrMore(Box<Path>),
    OneOrMore(Box<Path>),
    ZeroOrOne(Box<Path>),
    Negated(Vec<(String, bool)>),
}

/// What sits between a triple pattern's subject and object.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verb {
    Variable(String),
    Path(Path),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriplePattern {
    pub(crate) subject: Node,
    pub(crate) verb: Verb,
    pub(crate) object: Node,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateKind {
    Count,
    Sum,
    Min,
    Max,
    Avg,
    Sample,
    GroupConcat,
}

/// An expression (section 17).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Variable(String),
    Constant(Term),
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Compare(Comparison, Box<Expr>, Box<Expr>),
    Arithmetic(Arithmetic, Box<Expr>, Box<Expr>),
    Negate(Box<Expr>),
    In(Box<Expr>, Vec<Expr>, bool),
    Call(String, Vec<Expr>),
    Exists(Box<GraphPattern>, bool),
    Aggregate {
        kind: AggregateKind,
        distinct: bool,
        argument: Option<Box<Expr>>,
        separator: Option<String>,
    },
}

/// A graph pattern, as the algebra translates it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GraphPattern {
    Triples(Vec<TriplePattern>),
    Join(Box<GraphPattern>, Box<GraphPattern>),
    Optional(Box<GraphPattern>, Box<GraphPattern>, Option<Expr>),
    Union(Box<GraphPattern>, Box<GraphPattern>),
    Minus(Box<GraphPattern>, Box<GraphPattern>),
    Filter(Expr, Box<GraphPattern>),
    Bind(Box<GraphPattern>, String, Expr),
    Values(Vec<String>, Vec<Vec<Option<Term>>>),
    Graph(Node, Box<GraphPattern>),
    Select(Box<Query>),
}

impl GraphPattern {
    pub(crate) fn empty() -> Self {
        Self::Triples(Vec::new())
    }
}

/// One projected column: an expression and its name.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Projection {
    pub(crate) expr: Expr,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum QueryForm {
    Select {
        distinct: bool,
        projection: Option<Vec<Projection>>,
    },
    Ask,
    Construct(Vec<TriplePattern>),
    Describe(Vec<Node>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Dataset {
    pub(crate) default: Vec<String>,
    pub(crate) named: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OrderKey {
    pub(crate) expr: Expr,
    pub(crate) descending: bool,
}

/// A query.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Query {
    pub(crate) form: QueryForm,
    pub(crate) dataset: Option<Dataset>,
    pub(crate) pattern: GraphPattern,
    pub(crate) group_by: Vec<Projection>,
    pub(crate) having: Vec<Expr>,
    pub(crate) order_by: Vec<OrderKey>,
    pub(crate) limit: Option<u64>,
    pub(crate) offset: u64,
}

/// A triple pattern in a graph, for updates: `graph` is `None` for the default graph.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QuadPattern {
    pub(crate) subject: Node,
    pub(crate) predicate: Node,
    pub(crate) object: Node,
    pub(crate) graph: Option<Node>,
}

/// What `CLEAR`, `DROP`, `ADD`, `MOVE` and `COPY` act on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GraphTarget {
    Default,
    Named(Term),
    AllNamed,
    All,
}

/// One operation of an update request (SPARQL 1.1 Update, section 3).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UpdateOperation {
    InsertData(Vec<QuadPattern>),
    DeleteData(Vec<QuadPattern>),
    Modify {
        with: Option<Term>,
        delete: Vec<QuadPattern>,
        insert: Vec<QuadPattern>,
        using: Option<Dataset>,
        pattern: GraphPattern,
    },
    Clear(GraphTarget, bool),
    Create(Term, bool),
    Transfer {
        kind: TransferKind,
        from: GraphTarget,
        to: GraphTarget,
        silent: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferKind {
    Add,
    Move,
    Copy,
}
