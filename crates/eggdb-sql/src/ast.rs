use crate::dialect::Dialect;
use crate::types::{SqlType, Value};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Statement {
    CreateTable(CreateTable),
    DropTable {
        names: Vec<Name>,
        if_exists: bool,
        cascade: bool,
    },
    Insert(Insert),
    Select(Select),
    Query(Box<Query>),
    Update(Update),
    Delete(Delete),
    Begin {
        read_only: bool,
    },
    Commit,
    Rollback,
    Set {
        name: String,
        value: Option<String>,
    },
    Show {
        name: Option<String>,
    },
    Reset {
        name: Option<String>,
    },
    MySql(MySqlStatement),
    Pragma(Pragma),
    Truncate(TableReference),
    CreateIndex(CreateIndex),
    DropIndex(DropIndex),
    Explain(Explain),
    CreateSequence(CreateSequence),
    AlterSequence(AlterSequence),
    DropSequence {
        names: Vec<Name>,
        if_exists: bool,
        cascade: bool,
    },
    AlterTable(AlterTable),
    CreateSchema {
        name: Name,
        if_not_exists: bool,
    },
    DropSchema {
        names: Vec<Name>,
        if_exists: bool,
        cascade: bool,
    },
    RenameSchema {
        from: Name,
        to: Name,
    },
    CreateView(Box<CreateView>),
    DropView {
        names: Vec<Name>,
        if_exists: bool,
        cascade: bool,
    },
    AlterView(AlterView),
    Comment(Comment),
    Copy(Box<CopyStatement>),
    Ignored {
        tag: &'static str,
        notice: Option<String>,
    },
    SqlServer(SqlServerStatement),
    Merge(Box<MergeStatement>),
    AlterSession(Vec<(String, String)>),
    Discard(DiscardTarget),
    With(Box<WithStatement>),
    CreateType(Box<CreateType>),
    CreateDomain(Box<CreateDomain>),
    DropType {
        names: Vec<Name>,
        if_exists: bool,
        cascade: bool,
        domain: bool,
    },
    AlterType(Box<AlterType>),
    AlterDomain(Box<AlterDomain>),
    SetConstraints {
        names: Option<Vec<Name>>,
        deferred: bool,
    },
    CreateTrigger(Box<CreateTrigger>),
    DropTrigger(DropTrigger),
    EnableTrigger(EnableTrigger),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateTrigger {
    pub(crate) name: Name,
    pub(crate) table: Name,
    pub(crate) replace: bool,
    pub(crate) if_not_exists: bool,
    pub(crate) placement: Option<TriggerPlacement>,
    pub(crate) definition: crate::triggers::TriggerDefinition,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriggerPlacement {
    pub(crate) follows: bool,
    pub(crate) other: Name,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriggerName {
    pub(crate) schema: Option<String>,
    pub(crate) name: Name,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DropTrigger {
    pub(crate) names: Vec<TriggerName>,
    pub(crate) table: Option<Name>,
    pub(crate) if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EnableTrigger {
    pub(crate) names: Option<Vec<Name>>,
    pub(crate) table: Name,
    pub(crate) enable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateType {
    pub(crate) name: Name,
    pub(crate) body: TypeSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TypeSpec {
    Enum(Vec<String>),
    Composite(Vec<(Name, SqlType)>),
    Range { subtype: SqlType, position: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DomainConstraintSpec {
    pub(crate) name: Option<Name>,
    pub(crate) expr: Expr,
    pub(crate) text: String,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateDomain {
    pub(crate) name: Name,
    pub(crate) base: SqlType,
    pub(crate) default: Option<(Expr, String)>,
    pub(crate) not_null: bool,
    pub(crate) checks: Vec<DomainConstraintSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterType {
    pub(crate) name: Name,
    pub(crate) action: AlterTypeAction,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AlterTypeAction {
    AddValue {
        label: String,
        if_not_exists: bool,
        neighbor: Option<(String, bool)>,
    },
    RenameValue {
        from: String,
        to: String,
    },
    RenameTo(Name),
    SetSchema(Name),
    Accepted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterDomain {
    pub(crate) name: Name,
    pub(crate) action: AlterDomainAction,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AlterDomainAction {
    SetDefault(Option<(Expr, String)>),
    SetNotNull(bool),
    AddCheck {
        spec: DomainConstraintSpec,
        not_valid: bool,
    },
    DropConstraint {
        name: Name,
        if_exists: bool,
    },
    ValidateConstraint(Name),
    RenameConstraint {
        from: Name,
        to: Name,
    },
    RenameTo(Name),
    SetSchema(Name),
    Accepted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WithStatement {
    pub(crate) tables: Vec<WithTable>,
    pub(crate) recursive: bool,
    pub(crate) body: Statement,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WithTable {
    Query(CommonTable),
    Modifying {
        name: Name,
        column_names: Vec<Name>,
        statement: Statement,
    },
}

impl WithTable {
    pub(crate) fn name(&self) -> &Name {
        match self {
            Self::Query(table) => &table.name,
            Self::Modifying { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscardTarget {
    All,
    Plans,
    Sequences,
    Temporary,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeStatement {
    pub(crate) target: TableReference,
    pub(crate) source: Select,
    pub(crate) source_label: Option<String>,
    pub(crate) on: Expr,
    pub(crate) update: Option<MergeUpdate>,
    pub(crate) insert: Option<MergeInsert>,
    pub(crate) output: Option<Vec<SelectItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeUpdate {
    pub(crate) assignments: Vec<(Name, Expr)>,
    pub(crate) condition: Option<Expr>,
    pub(crate) delete_condition: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeInsert {
    pub(crate) columns: Option<Vec<Name>>,
    pub(crate) values: Vec<Expr>,
    pub(crate) condition: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SqlServerStatement {
    Use(Name),
    NoCount(bool),
    ImplicitTransactions(bool),
    IdentityInsert {
        table: Name,
        on: bool,
    },
    Execute(ProcedureCall),
    DeclareTable {
        name: Name,
        columns: Vec<(Name, SqlType)>,
    },
    InsertVariable {
        variable: Name,
        columns: Option<Vec<Name>>,
        source: VariableSource,
    },
    UpdateVariable {
        variable: Name,
        assignments: Vec<(Name, Expr)>,
        filter: Option<Expr>,
    },
    DeleteVariable {
        variable: Name,
        filter: Option<Expr>,
    },
    FormatOnly(bool),
    XactAbort(bool),
    InsertBulk(crate::bulk_load::InsertBulk),
    Declare(Vec<VariableDeclaration>),
    SetVariable {
        variable: Name,
        value: Expr,
    },
    SelectAssign {
        targets: Vec<AssignmentTarget>,
        query: Box<Statement>,
    },
    Condition(Expr),
    Print(Expr),
    RaiseError(RaiseError),
    Throw(Option<ThrowArguments>),
    ExecuteDynamic(DynamicSql),
    SaveTransaction(Name),
    RollbackTo(Name),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VariableDeclaration {
    pub(crate) name: Name,
    pub(crate) declared: crate::sqlserver_types::SqlServerType,
    pub(crate) initial: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AssignmentTarget {
    pub(crate) variable: Name,
    pub(crate) operator: Option<BinaryOperator>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum VariableSource {
    Values(Vec<Vec<InsertValue>>),
    Query(Box<Statement>),
    Execute(ProcedureCall),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RaiseError {
    pub(crate) message: Expr,
    pub(crate) severity: Expr,
    pub(crate) state: Expr,
    pub(crate) arguments: Vec<Expr>,
    pub(crate) set_error: bool,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThrowArguments {
    pub(crate) number: Expr,
    pub(crate) message: Expr,
    pub(crate) state: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DynamicSql {
    pub(crate) statement: Expr,
    pub(crate) declarations: Option<Expr>,
    pub(crate) arguments: Vec<ProcedureArgument>,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcedureCall {
    pub(crate) procedure: Name,
    pub(crate) arguments: Vec<ProcedureArgument>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcedureArgument {
    pub(crate) name: Option<String>,
    pub(crate) value: Option<Expr>,
    pub(crate) output_variable: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CopyStatement {
    pub(crate) source: CopySource,
    pub(crate) direction: CopyDirection,
    pub(crate) options: crate::copy::CopyOptions,
    pub(crate) condition: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CopySource {
    Table { name: Name, columns: Vec<Name> },
    Query(Box<Query>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyDirection {
    FromClient,
    ToClient,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateView {
    pub(crate) name: Name,
    pub(crate) or_replace: bool,
    pub(crate) existing_only: bool,
    pub(crate) if_not_exists: bool,
    pub(crate) columns: Vec<Name>,
    pub(crate) query: Query,
    pub(crate) text: String,
    pub(crate) check_option: Option<crate::encoding::CheckOption>,
    pub(crate) temporary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterView {
    pub(crate) name: Name,
    pub(crate) if_exists: bool,
    pub(crate) action: AlterViewAction,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AlterViewAction {
    RenameTo(Name),
    SetSchema(Name),
    Accepted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommentTarget {
    Relation { kind: RelationKind, name: Name },
    Column { table: Name, column: Name },
    Constraint { table: Name, name: Name },
    Index(Name),
    Schema(Name),
    Extension(Name),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelationKind {
    Table,
    View,
    Sequence,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Comment {
    pub(crate) target: CommentTarget,
    pub(crate) text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct SequenceOptions {
    pub(crate) sql_type: Option<(SqlType, usize)>,
    pub(crate) increment: Option<i64>,
    pub(crate) min: Option<Option<i64>>,
    pub(crate) max: Option<Option<i64>>,
    pub(crate) start: Option<i64>,
    pub(crate) restart: Option<Option<i64>>,
    pub(crate) cache: Option<i64>,
    pub(crate) cycle: Option<bool>,
    pub(crate) owned_by: Option<Option<(Name, Name)>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateSequence {
    pub(crate) name: Name,
    pub(crate) if_not_exists: bool,
    pub(crate) options: SequenceOptions,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterSequence {
    pub(crate) name: Name,
    pub(crate) if_exists: bool,
    pub(crate) options: SequenceOptions,
    pub(crate) rename: Option<Name>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ReferentialAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl ReferentialAction {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::NoAction => 0,
            Self::Restrict => 1,
            Self::Cascade => 2,
            Self::SetNull => 3,
            Self::SetDefault => 4,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::NoAction,
            1 => Self::Restrict,
            2 => Self::Cascade,
            3 => Self::SetNull,
            4 => Self::SetDefault,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForeignKeySpec {
    pub(crate) columns: Vec<Name>,
    pub(crate) parent: Name,
    pub(crate) parent_columns: Option<Vec<Name>>,
    pub(crate) on_delete: ReferentialAction,
    pub(crate) on_update: ReferentialAction,
    pub(crate) deferrable: bool,
    pub(crate) initially_deferred: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConstraintKind {
    PrimaryKey(Vec<Name>),
    Unique(Vec<Name>),
    Check { expr: Expr, text: String },
    ForeignKey(ForeignKeySpec),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConstraintSpec {
    pub(crate) name: Option<Name>,
    pub(crate) kind: ConstraintKind,
    pub(crate) position: usize,
    pub(crate) column: Option<Name>,
    pub(crate) using_index: Option<Name>,
    pub(crate) not_valid: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterTable {
    pub(crate) name: Name,
    pub(crate) if_exists: bool,
    pub(crate) actions: Vec<AlterAction>,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum AlterAction {
    AddColumn {
        column: ColumnDefinition,
        if_not_exists: bool,
    },
    DropColumn {
        name: Name,
        if_exists: bool,
        cascade: bool,
    },
    RenameColumn {
        from: Name,
        to: Name,
    },
    RenameTable(Name),
    SetDefault {
        column: Name,
        default: Option<(Expr, String)>,
    },
    SetNotNull {
        column: Name,
        not_null: bool,
    },
    SetType {
        column: Name,
        sql_type: SqlType,
    },
    Redefine {
        old_name: Name,
        column: ColumnDefinition,
    },
    AddConstraint(ConstraintSpec),
    DropConstraint {
        name: Name,
        if_exists: bool,
        cascade: bool,
    },
    DropPrimaryKey(usize),
    SetSchema(Name),
    AddIdentity {
        column: Name,
        generation: crate::encoding::IdentityGeneration,
        sequence: Option<Name>,
        options: SequenceOptions,
    },
    AddDefault {
        column: Name,
        default: Expr,
        source: String,
        position: usize,
    },
    EnableConstraint(Name),
    ValidateConstraint(Name),
    RenameConstraint {
        from: Name,
        to: Name,
    },
    AlterIdentity {
        column: Name,
        generation: Option<crate::encoding::IdentityGeneration>,
        options: SequenceOptions,
    },
    DropIdentity {
        column: Name,
        if_exists: bool,
    },
    Accepted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexSpec {
    pub(crate) name: Option<Name>,
    pub(crate) unique: bool,
    pub(crate) columns: Vec<Name>,
    pub(crate) position: usize,
    pub(crate) method: crate::encoding::IndexMethod,
    pub(crate) sort_options: crate::encoding::SortOptions,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateIndex {
    pub(crate) index: IndexSpec,
    pub(crate) table: Name,
    pub(crate) if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DropIndex {
    pub(crate) names: Vec<Name>,
    pub(crate) table: Option<Name>,
    pub(crate) if_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplainStyle {
    Plan,
    QueryPlan,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Explain {
    pub(crate) style: ExplainStyle,
    pub(crate) statement: Box<Statement>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pragma {
    pub(crate) name: String,
    pub(crate) argument: Option<String>,
    pub(crate) assigned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MySqlStatement {
    ShowTables {
        full: bool,
        like: Option<String>,
    },
    ShowDatabases {
        like: Option<String>,
    },
    ShowColumns {
        table: Name,
        full: bool,
        like: Option<String>,
    },
    ShowCreateTable(Name),
    ShowCreateView(Name),
    ShowCreateTrigger(Name),
    ShowTriggers {
        like: Option<String>,
    },
    ShowIndex(Name),
    ShowVariables {
        like: Option<String>,
    },
    ShowStatus,
    ShowWarnings {
        errors_only: bool,
    },
    ShowCount {
        errors_only: bool,
    },
    Use(Name),
    Set(Vec<Assignment>),
    Accepted(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum VariableTarget {
    System(String),
    User(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AssignedValue {
    Word(String),
    Default,
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Assignment {
    pub(crate) target: VariableTarget,
    pub(crate) value: AssignedValue,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Name {
    pub(crate) text: String,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnCommit {
    PreserveRows,
    DeleteRows,
    Drop,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateTable {
    pub(crate) name: Name,
    pub(crate) temporary: Option<OnCommit>,
    pub(crate) if_not_exists: bool,
    pub(crate) columns: Vec<ColumnDefinition>,
    pub(crate) primary_key: Option<(Vec<Name>, usize)>,
    pub(crate) primary_key_name: Option<Name>,
    pub(crate) indexes: Vec<IndexSpec>,
    pub(crate) constraints: Vec<ConstraintSpec>,
    pub(crate) comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnDefinition {
    pub(crate) name: Name,
    pub(crate) sql_type: SqlType,
    pub(crate) not_null: bool,
    pub(crate) primary_key: bool,
    pub(crate) default: Option<(Expr, String)>,
    pub(crate) default_dialect: Dialect,
    pub(crate) auto_increment: bool,
    pub(crate) identity_seed: Option<crate::encoding::IdentitySeed>,
    pub(crate) generated_always: bool,
    pub(crate) unique: Option<Option<Name>>,
    pub(crate) constraints: Vec<ConstraintSpec>,
    pub(crate) comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableReference {
    pub(crate) name: Name,
    pub(crate) alias: Option<String>,
    pub(crate) derived: Option<Box<DerivedTable>>,
    pub(crate) generated: Option<Box<GeneratedTable>>,
}

impl TableReference {
    pub(crate) fn named(name: Name, alias: Option<String>) -> Self {
        Self {
            name,
            alias,
            derived: None,
            generated: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GeneratedTable {
    Values(ValuesList),
    Function(TableFunction),
    Expansion(Box<SelectListExpansion>),
    Catalog(crate::pg_catalog::CatalogRelation),
    Variable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValuesList {
    pub(crate) rows: Vec<Vec<Expr>>,
    pub(crate) column_names: Vec<Name>,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableFunction {
    pub(crate) name: String,
    pub(crate) arguments: Vec<Expr>,
    pub(crate) ordinality: bool,
    pub(crate) column_names: Vec<Name>,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectListExpansion {
    pub(crate) query: Query,
    pub(crate) items: Vec<ExpandedItem>,
    pub(crate) order: Vec<(ExpansionKey, crate::types::SortDirection)>,
    pub(crate) hidden: usize,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpansionKey {
    Ordinal { number: i64, position: usize },
    Output { item: usize, offset: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExpandedItem {
    Columns(Option<usize>),
    SetReturning {
        function: String,
        arguments: usize,
        output_name: String,
        position: usize,
        field: Option<String>,
    },
    Window {
        spec: crate::window::WindowSpec,
        keys: usize,
        output_name: String,
    },
    Computed {
        expr: Expr,
        inputs: usize,
        windows: Vec<(crate::window::WindowSpec, usize)>,
        output_name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedTable {
    pub(crate) query: Query,
    pub(crate) column_names: Vec<Name>,
    pub(crate) dialect: Dialect,
    pub(crate) lateral: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Query {
    pub(crate) with: Vec<CommonTable>,
    pub(crate) recursive: bool,
    pub(crate) body: SetExpr,
    pub(crate) order_by: Vec<OrderItem>,
    pub(crate) limit: Option<Expr>,
    pub(crate) offset: Option<Expr>,
    pub(crate) position: usize,
}

impl Query {
    pub(crate) fn of_select(select: Select, position: usize) -> Self {
        Self {
            with: Vec::new(),
            recursive: false,
            body: SetExpr::Select(Box::new(select)),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            position,
        }
    }

    pub(crate) fn plain_select(&self) -> Option<&Select> {
        match &self.body {
            SetExpr::Select(select)
                if self.with.is_empty()
                    && self.order_by.is_empty()
                    && self.limit.is_none()
                    && self.offset.is_none() =>
            {
                Some(select)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CommonTable {
    pub(crate) name: Name,
    pub(crate) column_names: Vec<Name>,
    pub(crate) query: Query,
    pub(crate) dialect: Dialect,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SetExpr {
    Select(Box<Select>),
    Operation(Box<SetOperation>),
    Query(Box<Query>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetOperator {
    Union,
    Intersect,
    Except,
}

impl SetOperator {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Union => "UNION",
            Self::Intersect => "INTERSECT",
            Self::Except => "EXCEPT",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SetOperation {
    pub(crate) operator: SetOperator,
    pub(crate) all: bool,
    pub(crate) left: SetExpr,
    pub(crate) right: SetExpr,
    pub(crate) dialect: Dialect,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Subquery {
    pub(crate) test: SubqueryTest,
    pub(crate) query: Query,
    pub(crate) state: SubqueryState,
    pub(crate) dialect: Dialect,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SubqueryTest {
    Scalar,
    Exists,
    In {
        expr: Expr,
        negated: bool,
    },
    Compare {
        expr: Expr,
        operator: BinaryOperator,
        all: bool,
    },
    Array,
}

impl SubqueryTest {
    pub(crate) fn operand(&self) -> Option<&Expr> {
        match self {
            Self::Scalar | Self::Exists | Self::Array => None,
            Self::In { expr, .. } | Self::Compare { expr, .. } => Some(expr),
        }
    }

    pub(crate) fn operand_mut(&mut self) -> Option<&mut Expr> {
        match self {
            Self::Scalar | Self::Exists | Self::Array => None,
            Self::In { expr, .. } | Self::Compare { expr, .. } => Some(expr),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SubqueryState {
    Unprepared,
    Rows(Arc<SubqueryRows>),
    Correlated {
        template: usize,
        sql_type: Option<SqlType>,
        reads: Arc<[usize]>,
        grouped: Option<Arc<[(usize, usize)]>>,
    },
}

impl PartialEq for SubqueryState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unprepared, Self::Unprepared) => true,
            (Self::Rows(left), Self::Rows(right)) => Arc::ptr_eq(left, right),
            (
                Self::Correlated {
                    template: left,
                    sql_type: left_type,
                    ..
                },
                Self::Correlated {
                    template: right,
                    sql_type: right_type,
                    ..
                },
            ) => left == right && left_type == right_type,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SubqueryRows {
    pub(crate) sql_type: Option<SqlType>,
    pub(crate) values: Vec<Value>,
    pub(crate) rows: usize,
    pub(crate) hashed: std::sync::OnceLock<Option<crate::subquery::HashedValues>>,
    pub(crate) _held: crate::memory::HeldMemory,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InsertSource {
    Values(Vec<Vec<InsertValue>>),
    Query(Box<Query>),
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
    pub(crate) on_duplicate: OnDuplicate,
    pub(crate) rollback_on_conflict: bool,
    pub(crate) overriding: Option<Overriding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overriding {
    SystemValue,
    UserValue,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OnDuplicate {
    Fail,
    Ignore,
    Replace,
    Update(Vec<(Name, Expr)>),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
    Comma,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JoinCondition {
    Always,
    On(Expr),
    Using(Vec<Name>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Join {
    pub(crate) kind: JoinKind,
    pub(crate) table: TableReference,
    pub(crate) condition: JoinCondition,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FromClause {
    pub(crate) first: TableReference,
    pub(crate) joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Select {
    pub(crate) distinct: bool,
    pub(crate) items: Vec<SelectItem>,
    pub(crate) from: Option<FromClause>,
    pub(crate) filter: Option<Expr>,
    pub(crate) group_by: Vec<Expr>,
    pub(crate) having: Option<Expr>,
    pub(crate) order_by: Vec<OrderItem>,
    pub(crate) limit: Option<Expr>,
    pub(crate) offset: Option<Expr>,
    pub(crate) source_row_limit: Option<u64>,
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
    JsonField,
    JsonFieldText,
    JsonPath,
    JsonPathText,
    JsonContains,
    JsonContainedIn,
    RegexMatch,
    RegexMatchFolded,
    RegexNoMatch,
    RegexNoMatchFolded,
    JsonHasKey,
    JsonHasAnyKey,
    JsonHasAllKeys,
    JsonDeletePath,
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
            Self::JsonField => "->",
            Self::JsonFieldText => "->>",
            Self::JsonPath => "#>",
            Self::JsonPathText => "#>>",
            Self::JsonContains => "@>",
            Self::JsonContainedIn => "<@",
            Self::RegexMatch => "~",
            Self::RegexMatchFolded => "~*",
            Self::RegexNoMatch => "!~",
            Self::RegexNoMatchFolded => "!~*",
            Self::JsonHasKey => "?",
            Self::JsonHasAnyKey => "?|",
            Self::JsonHasAllKeys => "?&",
            Self::JsonDeletePath => "#-",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    StringAgg,
    JsonAgg,
    JsonbAgg,
    JsonObjectAgg,
    JsonbObjectAgg,
    ArrayAgg,
    BoolAnd,
    BoolOr,
    Every,
    PercentileCont,
    PercentileDisc,
    Mode,
}

impl AggregateFunction {
    pub(crate) const ALL: [Self; 17] = [
        Self::Count,
        Self::Sum,
        Self::Avg,
        Self::Min,
        Self::Max,
        Self::StringAgg,
        Self::JsonAgg,
        Self::JsonbAgg,
        Self::JsonObjectAgg,
        Self::JsonbObjectAgg,
        Self::ArrayAgg,
        Self::BoolAnd,
        Self::BoolOr,
        Self::Every,
        Self::PercentileCont,
        Self::PercentileDisc,
        Self::Mode,
    ];

    pub(crate) fn ordered_set_named(name: &str) -> Option<Self> {
        Some(match name {
            "percentile_cont" => Self::PercentileCont,
            "percentile_disc" => Self::PercentileDisc,
            "mode" => Self::Mode,
            _ => return None,
        })
    }

    pub(crate) fn is_ordered_set(self) -> bool {
        matches!(self, Self::PercentileCont | Self::PercentileDisc | Self::Mode)
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "count" => Self::Count,
            "sum" => Self::Sum,
            "avg" => Self::Avg,
            "min" => Self::Min,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub(crate) fn postgresql_named(name: &str) -> Option<Self> {
        Some(match name {
            "json_agg" => Self::JsonAgg,
            "jsonb_agg" => Self::JsonbAgg,
            "json_object_agg" => Self::JsonObjectAgg,
            "jsonb_object_agg" => Self::JsonbObjectAgg,
            "array_agg" => Self::ArrayAgg,
            "bool_and" => Self::BoolAnd,
            "bool_or" => Self::BoolOr,
            "every" => Self::Every,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::Max => "max",
            Self::StringAgg => "string_agg",
            Self::JsonAgg => "json_agg",
            Self::JsonbAgg => "jsonb_agg",
            Self::JsonObjectAgg => "json_object_agg",
            Self::JsonbObjectAgg => "jsonb_object_agg",
            Self::ArrayAgg => "array_agg",
            Self::BoolAnd => "bool_and",
            Self::BoolOr => "bool_or",
            Self::Every => "every",
            Self::PercentileCont => "percentile_cont",
            Self::PercentileDisc => "percentile_disc",
            Self::Mode => "mode",
        }
    }

    pub(crate) fn keeps_nulls(self) -> bool {
        self.builds_json() || self == Self::ArrayAgg
    }

    pub(crate) fn combines_truth(self) -> bool {
        matches!(self, Self::BoolAnd | Self::BoolOr | Self::Every)
    }

    pub(crate) fn builds_json(self) -> bool {
        matches!(
            self,
            Self::JsonAgg | Self::JsonbAgg | Self::JsonObjectAgg | Self::JsonbObjectAgg
        )
    }

    pub(crate) fn builds_object(self) -> bool {
        matches!(self, Self::JsonObjectAgg | Self::JsonbObjectAgg)
    }

    pub(crate) fn builds_jsonb(self) -> bool {
        matches!(self, Self::JsonbAgg | Self::JsonbObjectAgg)
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
    SqliteFunction {
        name: &'static str,
        arguments: Vec<Expr>,
    },
    OracleFunction {
        name: &'static str,
        arguments: Vec<Expr>,
    },
    SqlServerFunction {
        function: crate::sqlserver_functions::Function,
        arguments: Vec<Expr>,
    },
    Aggregate {
        function: AggregateFunction,
        argument: Option<Box<Expr>>,
        distinct: bool,
    },
    Slot {
        index: usize,
        sql_type: Option<SqlType>,
    },
    SessionValue {
        name: &'static str,
        value: String,
    },
    Subquery(Box<Subquery>),
    OuterColumn {
        index: usize,
        sql_type: Option<SqlType>,
        depth: usize,
    },
    Constant {
        value: Value,
        sql_type: Option<SqlType>,
    },
}
