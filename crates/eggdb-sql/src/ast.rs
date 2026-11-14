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
        read_only: Option<bool>,
        isolation: Option<String>,
    },
    SetTransaction(TransactionCharacteristics),
    Commit,
    Rollback,
    TwoPhase(TwoPhase),
    Savepoint(SavepointCommand),
    Analyze(Vec<Name>),
    Role(crate::roles::RoleStatement),
    Database(crate::served_databases::DatabaseStatement),
    RowSecurity(crate::row_security::RowSecurityStatement),
    Listen(Name),
    Unlisten(Option<Name>),
    Notify {
        channel: Name,
        payload: Option<String>,
    },
    Set {
        name: String,
        value: Option<String>,
        scope: crate::settings::SettingScope,
    },
    Show {
        name: Option<String>,
    },
    Reset {
        name: Option<String>,
    },
    MySql(MySqlStatement),
    Pragma(Pragma),
    VacuumInto(VacuumInto),
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
    InstallExtension {
        name: &'static str,
        if_not_exists: bool,
    },
    MaterializedView(MaterializedViewCommand),
    SqlServer(SqlServerStatement),
    Merge(Box<MergeStatement>),
    CreateTableAs(Box<CreateTableAs>),
    CreateTableLike(Box<CreateTableLike>),
    MultitableInsert(Box<MultitableInsert>),
    AlterSession(Vec<(String, String)>),
    Discard(DiscardTarget),
    Prepared(PreparedCommand),
    Cursor(CursorCommand),
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
    AlterTrigger(AlterTrigger),
    CreateRoutine(Box<CreateRoutine>),
    DropRoutine(DropRoutine),
    AlterRoutine(AlterRoutine),
    Call(CallRoutine),
    Do(DoBlock),
    PlsqlBlock(Box<PlsqlBlock>),
    Synonym(SynonymCommand),
    OracleContext(OracleContextCommand),
    TextSearch(crate::text_search_catalog::TextSearchCommand),
    OracleSystemTrigger(Box<crate::oracle_system_triggers::CreateSystemTrigger>),
    ForeignData(Box<crate::foreign_tables::ForeignDataCommand>),
    CreatePackage(Box<CreatePackage>),
    DropPackage(DropPackage),
    AlterPackage(AlterPackage),
    EventTrigger(EventTriggerCommand),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlsqlBlock {
    pub(crate) code: String,
    pub(crate) program: crate::plpgsql::Program,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreatePackage {
    pub(crate) replace: bool,
    pub(crate) text: String,
    pub(crate) package: crate::plsql::PlsqlPackage,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DropPackage {
    pub(crate) name: Name,
    pub(crate) body: bool,
    pub(crate) if_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackagePart {
    Whole,
    Specification,
    Body,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterPackage {
    pub(crate) name: Name,
    pub(crate) part: PackagePart,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EventTriggerCommand {
    Create {
        name: Name,
        event: Name,
        tags: Vec<String>,
        function: String,
    },
    Drop {
        name: Name,
        if_exists: bool,
    },
    Alter {
        name: Name,
        change: EventTriggerChange,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EventTriggerChange {
    Enable(char),
    Disable,
    Rename(Name),
    Owner(Name),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MaterializedViewCommand {
    Create {
        name: Name,
        query: String,
        with_data: bool,
        if_not_exists: bool,
    },
    Refresh {
        name: Name,
        with_data: bool,
    },
    Drop {
        names: Vec<Name>,
        if_exists: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallRoutine {
    pub(crate) name: Name,
    pub(crate) arguments: Vec<Expr>,
    pub(crate) argument_names: Vec<Option<String>>,
    pub(crate) user_variables: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DoBlock {
    pub(crate) code: String,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateRoutine {
    pub(crate) name: Name,
    pub(crate) replace: bool,
    pub(crate) must_exist: bool,
    pub(crate) definition: crate::routines::RoutineDefinition,
    pub(crate) body_position: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RoutineTarget {
    pub(crate) name: Name,
    pub(crate) arguments: Option<Vec<SqlType>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DropRoutine {
    pub(crate) kind: Option<crate::routines::RoutineKind>,
    pub(crate) targets: Vec<RoutineTarget>,
    pub(crate) if_exists: bool,
    pub(crate) cascade: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterRoutine {
    pub(crate) kind: Option<crate::routines::RoutineKind>,
    pub(crate) target: RoutineTarget,
    pub(crate) action: AlterRoutineAction,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AlterRoutineAction {
    RenameTo(Name),
    SetSchema(Name),
    Settings(Vec<RoutineSettingChange>),
    Accepted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RoutineSettingChange {
    Set { name: String, value: String },
    Reset(String),
    ResetAll,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateTrigger {
    pub(crate) name: Name,
    pub(crate) table: Name,
    pub(crate) temporary: bool,
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
pub(crate) struct AlterTrigger {
    pub(crate) name: TriggerName,
    pub(crate) action: AlterTriggerAction,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AlterTriggerAction {
    Enable,
    Disable,
    Compile,
    RenameTo(Name),
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
    Query(Box<CommonTable>),
    Modifying {
        name: Name,
        column_names: Vec<Name>,
        statement: Box<Statement>,
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CursorCommand {
    Declare {
        name: String,
        scroll: Option<bool>,
        hold: bool,
        query: Box<Statement>,
    },
    Fetch {
        name: String,
        direction: crate::portals::PortalDirection,
        only_move: bool,
    },
    Close(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscardTarget {
    All,
    Plans,
    Sequences,
    Temporary,
    PreparedStatement(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeStatement {
    pub(crate) target: TableReference,
    pub(crate) source: Select,
    pub(crate) source_label: Option<String>,
    pub(crate) on: Expr,
    pub(crate) matched: Vec<MergeUpdate>,
    pub(crate) insert: Option<MergeInsert>,
    pub(crate) by_source: Vec<MergeUpdate>,
    pub(crate) output: Option<Vec<SelectItem>>,
    pub(crate) top: Option<Expr>,
}

impl MergeUpdate {
    pub(crate) fn only_deletes(&self) -> bool {
        self.assignments.is_empty() && self.delete_condition.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateTableAs {
    pub(crate) name: Name,
    pub(crate) temporary: bool,
    pub(crate) query: Statement,
    pub(crate) spelling: CreateTableAsSpelling,
    pub(crate) if_not_exists: bool,
    pub(crate) column_names: Vec<Name>,
    pub(crate) declared: Option<Box<CreateTable>>,
    pub(crate) with_data: bool,
    pub(crate) on_duplicate: OnDuplicate,
    pub(crate) on_commit: Option<OnCommit>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateTableLike {
    pub(crate) name: Name,
    pub(crate) temporary: bool,
    pub(crate) if_not_exists: bool,
    pub(crate) source: Name,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PreparedCommand {
    Prepare(PrepareStatement),
    Execute(ExecutePrepared),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PrepareStatement {
    pub(crate) name: Name,
    pub(crate) parameter_types: Vec<SqlType>,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExecutePrepared {
    pub(crate) name: Name,
    pub(crate) arguments: Vec<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateTableAsSpelling {
    SelectInto,
    CreateTableAs,
}

impl CreateTableAs {
    pub(crate) fn select_into(name: Name, temporary: bool, query: Statement) -> Self {
        Self {
            name,
            temporary,
            query,
            spelling: CreateTableAsSpelling::SelectInto,
            if_not_exists: false,
            column_names: Vec::new(),
            declared: None,
            with_data: true,
            on_duplicate: OnDuplicate::Fail,
            on_commit: None,
        }
    }

    pub(crate) fn command(&self) -> &'static str {
        match self.spelling {
            CreateTableAsSpelling::SelectInto => "SELECT INTO",
            CreateTableAsSpelling::CreateTableAs => "CREATE TABLE AS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultitableInsertKind {
    All,
    First,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MultitableInsert {
    pub(crate) kind: MultitableInsertKind,
    pub(crate) source: Query,
    pub(crate) computed: usize,
    pub(crate) branches: Vec<InsertBranch>,
    pub(crate) otherwise: Vec<InsertTarget>,
}

impl MultitableInsert {
    pub(crate) fn targets(&self) -> impl Iterator<Item = &InsertTarget> {
        self.branches
            .iter()
            .flat_map(|branch| &branch.targets)
            .chain(&self.otherwise)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InsertBranch {
    pub(crate) condition: Option<usize>,
    pub(crate) targets: Vec<InsertTarget>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InsertTarget {
    pub(crate) table: TableReference,
    pub(crate) columns: Option<Vec<Name>>,
    pub(crate) values: Option<Vec<TargetValue>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetValue {
    Computed(usize),
    Default,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutputInto {
    pub(crate) statement: Statement,
    pub(crate) destination: OutputDestination,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutputDestination {
    pub(crate) target: OutputTarget,
    pub(crate) columns: Option<Vec<Name>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutputTarget {
    Variable(Name),
    Table(TableReference),
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
    TopRows(Box<TopRows>),
    InsertExecute(Box<InsertExecute>),
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
        rules: crate::table_variables::TableVariableRules,
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
    Showplan {
        kind: ShowplanKind,
        on: bool,
    },
    XactAbort(bool),
    InsertBulk(crate::bulk_load::InsertBulk),
    SecurityPolicy(Box<crate::sqlserver_security::SecurityPolicyCommand>),
    DdlTrigger(Box<crate::sqlserver_ddl_triggers::DdlTriggerCommand>),
    PartitionScheme(Box<crate::sqlserver_partitions::PartitionSchemeCommand>),
    Declare(Vec<VariableDeclaration>),
    SetVariable {
        variable: Name,
        value: Expr,
    },
    SelectAssign {
        targets: Vec<AssignmentTarget>,
        query: Box<Statement>,
        per_row: bool,
    },
    Condition(Expr),
    Print(Expr),
    RaiseError(RaiseError),
    Throw(Option<ThrowArguments>),
    ExecuteDynamic(DynamicSql),
    SaveTransaction(Name),
    RollbackTo(Name),
    Return(Expr),
    DeclareCursor(CursorDeclaration),
    OpenCursor(CursorName),
    FetchCursor {
        cursor: CursorName,
        orientation: FetchOrientation,
        into: Vec<Name>,
    },
    CloseCursor(CursorName),
    DeallocateCursor(CursorName),
    WaitFor {
        until_time: bool,
        time: Expr,
    },
    OutputInto(Box<OutputInto>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CursorName {
    pub(crate) name: Name,
    pub(crate) global: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CursorDeclaration {
    pub(crate) name: Name,
    pub(crate) global: bool,
    pub(crate) scroll: bool,
    pub(crate) dynamic: bool,
    pub(crate) query: Box<Statement>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FetchOrientation {
    Next,
    Prior,
    First,
    Last,
    Absolute(Expr),
    Relative(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VariableDeclaration {
    pub(crate) name: Name,
    pub(crate) declared: crate::sqlserver_types::SqlServerType,
    pub(crate) initial: Option<Expr>,
}

/// Transact-SQL's `INSERT [INTO] table [(columns)] EXEC procedure ...`: the rows of every result
/// set the procedure returns, inserted into a table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InsertExecute {
    pub(crate) table: Name,
    pub(crate) columns: Option<Vec<Name>>,
    pub(crate) call: ProcedureCall,
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
    pub(crate) logged: bool,
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
    pub(crate) routine: String,
    pub(crate) arguments: Vec<ProcedureArgument>,
    pub(crate) status_variable: Option<Name>,
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
    pub(crate) sqlite_text: Option<String>,
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
    pub(crate) or_replace: bool,
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
    SystemVersioning(Option<SystemVersioning>),
    SystemPeriod {
        start: String,
        end: String,
    },
    SystemVersioningOn {
        history: Option<Name>,
        retention: Option<String>,
    },
    AddColumn {
        column: ColumnDefinition,
        if_not_exists: bool,
        sqlite_text: Option<String>,
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
        using: Option<Expr>,
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
    SetExpression {
        column: Name,
        generation: GenerationSpec,
    },
    DropExpression {
        column: Name,
        if_exists: bool,
    },
    AttachPartition {
        partition: Name,
        bound: PartitionBoundSpec,
    },
    DetachPartition(Name),
    AddPartitions {
        partitions: Vec<NamedPartition>,
        hash_count: Option<u32>,
        position: usize,
    },
    DropPartitions(Vec<Name>),
    TruncatePartitions(Vec<Name>),
    DropSubpartitions(Vec<Name>),
    TruncateSubpartitions(Vec<Name>),
    MaintainPartitions(Vec<Name>),
    ReorganizePartitions {
        from: Vec<Name>,
        into: Vec<NamedPartition>,
        position: usize,
    },
    SplitPartition {
        partition: Name,
        point: SplitPoint,
        into: [Name; 2],
    },
    ExchangePartition {
        partition: Name,
        table: Name,
        validate: bool,
    },
    CoalescePartitions {
        count: u32,
        position: usize,
    },
    MergePartitions {
        from: Vec<Name>,
        into: Name,
    },
    MergePartitionSpan {
        first: Name,
        last: Name,
        into: Name,
    },
    ModifyPartitionValues {
        partition: Name,
        values: Vec<PartitionBoundValue>,
        adding: bool,
    },
    RowMovement(bool),
    AttachIndex(Name),
    MoveColumn {
        column: Name,
        placement: ColumnPlacement,
    },
    AddIndex(IndexSpec),
    RenameIndex {
        from: Name,
        to: Name,
    },
    Accepted,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SplitPoint {
    At(Vec<PartitionBoundValue>),
    Values(Vec<PartitionBoundValue>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ColumnPlacement {
    First,
    After(Name),
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
    pub(crate) only: bool,
    pub(crate) if_not_exists: bool,
    pub(crate) online: bool,
    pub(crate) sqlite_text: Option<String>,
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
    Tree,
    Json,
    PostgresJson,
    PostgresYaml,
    PostgresXml,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Explain {
    pub(crate) style: ExplainStyle,
    pub(crate) analyze: bool,
    pub(crate) buffers: bool,
    pub(crate) statement: Box<Statement>,
    pub(crate) plan_table: Option<PlanTableTarget>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PlanTableTarget {
    pub(crate) statement_id: Option<String>,
    pub(crate) table: Option<Name>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShowplanKind {
    Text,
    All,
    StatisticsProfile,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VacuumInto {
    pub(crate) path: String,
    pub(crate) position: usize,
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
    ShowCreateRoutine {
        kind: crate::routines::RoutineKind,
        name: Name,
    },
    ShowRoutineStatus {
        kind: crate::routines::RoutineKind,
        like: Option<String>,
    },
    CreateEvent(Box<EventDefinition>),
    LoadData(Box<LoadData>),
    AlterEvent(Box<AlterEvent>),
    DropEvent {
        name: Name,
        if_exists: bool,
    },
    ShowEvents {
        like: Option<String>,
    },
    ShowCreateEvent(Name),
    ShowCreateSequence(Name),
    ShowTriggers {
        like: Option<String>,
    },
    ShowIndex(Name),
    ShowVariables {
        like: Option<String>,
    },
    ShowStatus,
    ShowServerFacts {
        view: crate::mysql_catalog::MySqlRelation,
        like: Option<String>,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadDuplicates {
    Error,
    Replace,
    Ignore,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoadTarget {
    Column(String),
    Variable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoadData {
    pub(crate) local: bool,
    pub(crate) file: String,
    pub(crate) duplicates: LoadDuplicates,
    pub(crate) table: Name,
    pub(crate) table_text: String,
    pub(crate) partitions: Option<String>,
    pub(crate) fields_terminated: Vec<u8>,
    pub(crate) enclosed: Option<u8>,
    pub(crate) optionally_enclosed: bool,
    pub(crate) escaped: Option<u8>,
    pub(crate) lines_starting: Vec<u8>,
    pub(crate) lines_terminated: Vec<u8>,
    pub(crate) ignore_lines: u64,
    pub(crate) targets: Option<Vec<LoadTarget>>,
    pub(crate) assignments: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EventSchedule {
    At(Expr),
    Every {
        amount: Expr,
        unit: String,
        starts: Option<Expr>,
        ends: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventDefinition {
    pub(crate) name: Name,
    pub(crate) if_not_exists: bool,
    pub(crate) schedule: EventSchedule,
    pub(crate) preserve: bool,
    pub(crate) enabled: bool,
    pub(crate) comment: Option<String>,
    pub(crate) body: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlterEvent {
    pub(crate) name: Name,
    pub(crate) schedule: Option<EventSchedule>,
    pub(crate) preserve: Option<bool>,
    pub(crate) enabled: Option<bool>,
    pub(crate) rename: Option<Name>,
    pub(crate) comment: Option<String>,
    pub(crate) body: Option<String>,
}

/// Oracle's `CREATE [OR REPLACE] CONTEXT namespace USING [schema.]package` and `DROP CONTEXT`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OracleContextCommand {
    Create {
        namespace: String,
        package: String,
        replace: bool,
    },
    Drop {
        namespace: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SynonymCommand {
    Create {
        name: Name,
        public: bool,
        replace: bool,
        target: Name,
    },
    Drop {
        name: Name,
        public: bool,
        force: bool,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharacteristicsTarget {
    CurrentTransaction,
    NextTransaction,
    Session,
    Global,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TransactionCharacteristics {
    pub(crate) target: CharacteristicsTarget,
    pub(crate) read_only: Option<bool>,
    pub(crate) isolation: Option<String>,
    pub(crate) snapshot: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SavepointCommand {
    Set(Name),
    RollbackTo(Name),
    Release(Name),
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
    pub(crate) partition_by: Option<PartitionBy>,
    pub(crate) partition_of: Option<PartitionOf>,
    pub(crate) sqlite_text: Option<String>,
    pub(crate) system_versioning: Option<SystemVersioning>,
    pub(crate) rowversion_column: Option<String>,
    pub(crate) lsm_storage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartitionStrategy {
    Range,
    List,
    Hash,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PartitionBoundValue {
    Value(Expr),
    MinValue,
    MaxValue,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PartitionBoundSpec {
    Range {
        from: Vec<PartitionBoundValue>,
        to: Vec<PartitionBoundValue>,
    },
    List(Vec<PartitionBoundValue>),
    Hash {
        modulus: u32,
        remainder: u32,
    },
    Default,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartitionBy {
    pub(crate) strategy: PartitionStrategy,
    pub(crate) key: Vec<PartitionKeyPart>,
    pub(crate) partitions: Vec<NamedPartition>,
    pub(crate) hash_partitions: Option<u32>,
    pub(crate) mysql_integer_key: bool,
    pub(crate) subpartition_by: Option<Box<SubpartitionBy>>,
    pub(crate) row_movement: Option<bool>,
    pub(crate) interval: Option<String>,
    pub(crate) position: usize,
    pub(crate) scheme: Option<Name>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubpartitionBy {
    pub(crate) strategy: PartitionStrategy,
    pub(crate) key: Vec<PartitionKeyPart>,
    pub(crate) count: Option<u32>,
    pub(crate) template: Vec<NamedPartition>,
    pub(crate) mysql_integer_key: bool,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PartitionKeyPart {
    Column(Name),
    Expression {
        expr: Expr,
        text: String,
        position: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NamedPartition {
    pub(crate) name: Name,
    pub(crate) bound: NamedPartitionBound,
    pub(crate) subpartitions: Vec<NamedPartition>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NamedPartitionBound {
    LessThan(Vec<PartitionBoundValue>),
    In(Vec<PartitionBoundValue>),
    Hash,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartitionOf {
    pub(crate) parent: Name,
    pub(crate) bound: PartitionBoundSpec,
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
    pub(crate) serial: bool,
    pub(crate) identity_seed: Option<crate::encoding::IdentitySeed>,
    pub(crate) generated_always: bool,
    pub(crate) on_update_current_timestamp: bool,
    pub(crate) unique: Option<Option<Name>>,
    pub(crate) constraints: Vec<ConstraintSpec>,
    pub(crate) comment: Option<String>,
    pub(crate) generated: Option<GenerationSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GenerationSpec {
    pub(crate) expr: Expr,
    pub(crate) text: String,
    pub(crate) stored: bool,
    pub(crate) type_given: bool,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableReference {
    pub(crate) name: Name,
    pub(crate) alias: Option<String>,
    pub(crate) derived: Option<Box<DerivedTable>>,
    pub(crate) generated: Option<Box<GeneratedTable>>,
    pub(crate) partition_selection: Option<Box<PartitionSelection>>,
    pub(crate) system_time: Option<Box<SystemTime>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SystemVersioning {
    pub(crate) start: String,
    pub(crate) end: String,
    pub(crate) history: Option<Name>,
    pub(crate) on: bool,
    pub(crate) retention: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SystemTime {
    AsOf(Expr),
    FromTo(Expr, Expr),
    Between(Expr, Expr),
    ContainedIn(Expr, Expr),
    All,
}

impl TableReference {
    pub(crate) fn named(name: Name, alias: Option<String>) -> Self {
        Self {
            name,
            alias,
            derived: None,
            generated: None,
            partition_selection: None,
            system_time: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PartitionSelection {
    Named(Vec<Name>),
    HoldingKey { values: Vec<Expr>, position: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GeneratedTable {
    Values(ValuesList),
    Function(TableFunction),
    Expansion(Box<SelectListExpansion>),
    Catalog(crate::pg_catalog::CatalogRelation),
    Variable(String),
    Sequence(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValuesList {
    pub(crate) rows: Vec<Vec<Expr>>,
    pub(crate) column_names: Vec<Name>,
    pub(crate) position: usize,
}

pub(crate) const ORACLE_COLLECTION_TABLE: &str = "table";

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
    pub(crate) hierarchy: Option<Box<Hierarchy>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hierarchy {
    pub(crate) inputs: usize,
    pub(crate) start: Option<usize>,
    pub(crate) connect: Expr,
    pub(crate) prior_inputs: Vec<usize>,
    pub(crate) no_cycle: bool,
    pub(crate) siblings: Vec<(usize, crate::types::SortDirection)>,
    pub(crate) outputs: Vec<HierarchyOutput>,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HierarchyOutput {
    Path { input: usize, separator: String },
    Root(usize),
    Prior(usize),
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
    pub(crate) search: Option<SearchClause>,
    pub(crate) cycle: Option<CycleClause>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchOrder {
    DepthFirst,
    BreadthFirst,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SearchClause {
    pub(crate) order: SearchOrder,
    pub(crate) columns: Vec<Name>,
    pub(crate) sequence_column: Name,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CycleClause {
    pub(crate) columns: Vec<Name>,
    pub(crate) mark_column: Name,
    pub(crate) mark_values: Option<CycleMarkValues>,
    pub(crate) path_column: Name,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CycleMarkValues {
    pub(crate) cycle: Expr,
    pub(crate) default: Expr,
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
    RowCompare {
        expr: Expr,
        operator: BinaryOperator,
    },
}

impl SubqueryTest {
    pub(crate) fn operand(&self) -> Option<&Expr> {
        match self {
            Self::Scalar | Self::Exists | Self::Array => None,
            Self::In { expr, .. } | Self::Compare { expr, .. } | Self::RowCompare { expr, .. } => {
                Some(expr)
            }
        }
    }

    pub(crate) fn operand_mut(&mut self) -> Option<&mut Expr> {
        match self {
            Self::Scalar | Self::Exists | Self::Array => None,
            Self::In { expr, .. } | Self::Compare { expr, .. } | Self::RowCompare { expr, .. } => {
                Some(expr)
            }
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
    pub(crate) conflict_target: Option<ConflictTarget>,
    pub(crate) rollback_on_conflict: bool,
    pub(crate) keeps_changes_on_conflict: bool,
    pub(crate) overriding: Option<Overriding>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConflictTarget {
    Inferred {
        columns: Vec<Name>,
        expressions: Vec<Expr>,
        predicate: Option<Expr>,
    },
    Constraint(Name),
}

impl ConflictTarget {
    pub(crate) fn columns(&self) -> &[Name] {
        match self {
            Self::Inferred { columns, .. } => columns,
            Self::Constraint(_) => &[],
        }
    }

    pub(crate) fn expressions_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            Self::Inferred {
                expressions,
                predicate,
                ..
            } => expressions.iter_mut().chain(predicate.iter_mut()).collect(),
            Self::Constraint(_) => Vec::new(),
        }
    }
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
    Update(Vec<(Name, Expr)>, Option<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictResolution {
    Rollback,
    Abort,
    Fail,
    Ignore,
    Replace,
}

impl ConflictResolution {
    pub(crate) fn into_insert_fields(self) -> (OnDuplicate, bool, bool) {
        match self {
            Self::Rollback => (OnDuplicate::Fail, true, false),
            Self::Abort => (OnDuplicate::Fail, false, false),
            Self::Fail => (OnDuplicate::Fail, false, true),
            Self::Ignore => (OnDuplicate::Ignore, false, false),
            Self::Replace => (OnDuplicate::Replace, false, false),
        }
    }

    pub(crate) fn resolves_row_by_row(self) -> bool {
        matches!(self, Self::Fail | Self::Ignore | Self::Replace)
    }
}

/// Transact-SQL's `INSERT`, `UPDATE` or `DELETE` with `TOP (n) [PERCENT]`: the statement, run on
/// at most `count` (or that percentage) of the rows it matches or inserts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TopRows {
    pub(crate) count: Expr,
    pub(crate) statement: Statement,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Update {
    pub(crate) table: TableReference,
    pub(crate) assignments: Vec<(Name, InsertValue)>,
    pub(crate) filter: Option<Expr>,
    pub(crate) returning: Option<Vec<SelectItem>>,
    pub(crate) from: Option<JoinedTables>,
    pub(crate) conflict_resolution: ConflictResolution,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Delete {
    pub(crate) table: TableReference,
    pub(crate) filter: Option<Expr>,
    pub(crate) returning: Option<Vec<SelectItem>>,
    pub(crate) using: Option<JoinedTables>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JoinedTables {
    pub(crate) tables: FromClause,
    pub(crate) target_at: Option<usize>,
    pub(crate) more_targets_at: Vec<usize>,
    pub(crate) assigned_at: Vec<Option<usize>>,
    pub(crate) writes_any_table: bool,
}

impl JoinedTables {
    pub(crate) fn beside_target(tables: FromClause) -> Self {
        Self {
            tables,
            target_at: None,
            more_targets_at: Vec::new(),
            assigned_at: Vec::new(),
            writes_any_table: false,
        }
    }

    pub(crate) fn references(&self) -> impl Iterator<Item = &TableReference> {
        std::iter::once(&self.tables.first).chain(self.tables.joins.iter().map(|join| &join.table))
    }
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
    Natural,
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
    pub(crate) grouping_sets: Option<GroupingSets>,
    pub(crate) distinct_on: Vec<Expr>,
    pub(crate) having: Option<Expr>,
    pub(crate) order_by: Vec<OrderItem>,
    pub(crate) limit: Option<Expr>,
    pub(crate) offset: Option<Expr>,
    pub(crate) source_row_limit: Option<u64>,
    pub(crate) locking: Option<RowLocking>,
}

/// A two-phase commit statement (D1127): PostgreSQL's `PREPARE TRANSACTION`, `COMMIT PREPARED`
/// and `ROLLBACK PREPARED`, and MySQL's `XA`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TwoPhase {
    Prepare(String),
    CommitPrepared(String),
    RollbackPrepared(String),
    XaStart(String),
    XaEnd(String),
    XaCommitOnePhase(String),
    XaRollback(String),
    Recover,
}

impl TwoPhase {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::Prepare(_) => "PREPARE TRANSACTION",
            Self::CommitPrepared(_) | Self::XaCommitOnePhase(_) => "COMMIT PREPARED",
            Self::RollbackPrepared(_) | Self::XaRollback(_) => "ROLLBACK PREPARED",
            Self::XaStart(_) => "XA START",
            Self::XaEnd(_) => "XA END",
            Self::Recover => "XA RECOVER",
        }
    }
}

/// A query's `FOR UPDATE` or `FOR SHARE` clause (D1126): the rows it returns are locked until
/// the transaction ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowLocking {
    pub(crate) exclusive: bool,
    pub(crate) wait: RowLockWait,
}

/// What a row locking clause does with a row another transaction holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowLockWait {
    Wait,
    NoWait,
    SkipLocked,
}

impl Select {
    /// Whether the query keeps one row of each set of duplicates, by `DISTINCT` or `DISTINCT ON`.
    pub(crate) fn removes_duplicates(&self) -> bool {
        self.distinct || !self.distinct_on.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupingSets {
    pub(crate) sets: Vec<Vec<usize>>,
    pub(crate) distinct: bool,
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
    JsonPathExists,
    TextSearchMatch,
    VectorL2Distance,
    VectorCosineDistance,
    VectorNegativeInnerProduct,
    VectorL1Distance,
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
            Self::JsonPathExists => "@?",
            Self::TextSearchMatch => "@@",
            Self::VectorL2Distance => "<->",
            Self::VectorCosineDistance => "<=>",
            Self::VectorNegativeInnerProduct => "<#>",
            Self::VectorL1Distance => "<+>",
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
    ApproxPercentileCont,
    ApproxCountDistinct,
    Stddev,
    StddevSamp,
    StddevPop,
    Variance,
    VarSamp,
    VarPop,
    OracleStddev,
    OracleVariance,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    RegrCount,
    RegrSxx,
    RegrSyy,
    RegrSxy,
    RegrAvgx,
    RegrAvgy,
    RegrR2,
    RegrSlope,
    RegrIntercept,
    CovarPop,
    CovarSamp,
    Corr,
    CorrS,
    CorrK,
}

impl AggregateFunction {
    pub(crate) const ALL: [Self; 45] = [
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
        Self::ApproxPercentileCont,
        Self::ApproxCountDistinct,
        Self::Stddev,
        Self::StddevSamp,
        Self::StddevPop,
        Self::Variance,
        Self::VarSamp,
        Self::VarPop,
        Self::OracleStddev,
        Self::OracleVariance,
        Self::Rank,
        Self::DenseRank,
        Self::PercentRank,
        Self::CumeDist,
        Self::RegrCount,
        Self::RegrSxx,
        Self::RegrSyy,
        Self::RegrSxy,
        Self::RegrAvgx,
        Self::RegrAvgy,
        Self::RegrR2,
        Self::RegrSlope,
        Self::RegrIntercept,
        Self::CovarPop,
        Self::CovarSamp,
        Self::Corr,
        Self::CorrS,
        Self::CorrK,
    ];

    pub(crate) fn bivariate_named(name: &str) -> Option<Self> {
        Some(match name {
            "regr_count" => Self::RegrCount,
            "regr_sxx" => Self::RegrSxx,
            "regr_syy" => Self::RegrSyy,
            "regr_sxy" => Self::RegrSxy,
            "regr_avgx" => Self::RegrAvgx,
            "regr_avgy" => Self::RegrAvgy,
            "regr_r2" => Self::RegrR2,
            "regr_slope" => Self::RegrSlope,
            "regr_intercept" => Self::RegrIntercept,
            "covar_pop" => Self::CovarPop,
            "covar_samp" => Self::CovarSamp,
            "corr" => Self::Corr,
            "corr_s" => Self::CorrS,
            "corr_k" => Self::CorrK,
            _ => return None,
        })
    }

    pub(crate) fn is_bivariate(self) -> bool {
        matches!(
            self,
            Self::RegrCount
                | Self::RegrSxx
                | Self::RegrSyy
                | Self::RegrSxy
                | Self::RegrAvgx
                | Self::RegrAvgy
                | Self::RegrR2
                | Self::RegrSlope
                | Self::RegrIntercept
                | Self::CovarPop
                | Self::CovarSamp
                | Self::Corr
                | Self::CorrS
                | Self::CorrK
        )
    }

    pub(crate) fn hypothetical_named(name: &str) -> Option<Self> {
        Some(match name {
            "rank" => Self::Rank,
            "dense_rank" => Self::DenseRank,
            "percent_rank" => Self::PercentRank,
            "cume_dist" => Self::CumeDist,
            _ => return None,
        })
    }

    pub(crate) fn is_hypothetical(self) -> bool {
        matches!(
            self,
            Self::Rank | Self::DenseRank | Self::PercentRank | Self::CumeDist
        )
    }

    pub(crate) fn is_statistical(self) -> bool {
        matches!(
            self,
            Self::Stddev
                | Self::StddevSamp
                | Self::StddevPop
                | Self::Variance
                | Self::VarSamp
                | Self::VarPop
                | Self::OracleStddev
                | Self::OracleVariance
        )
    }

    pub(crate) fn ordered_set_named(name: &str) -> Option<Self> {
        Some(match name {
            "percentile_cont" => Self::PercentileCont,
            "approx_percentile_cont" | "approx_percentile" => Self::ApproxPercentileCont,
            "percentile_disc" | "approx_percentile_disc" => Self::PercentileDisc,
            "mode" => Self::Mode,
            _ => return None,
        })
    }

    pub(crate) fn is_ordered_set(self) -> bool {
        matches!(
            self,
            Self::PercentileCont | Self::PercentileDisc | Self::Mode | Self::ApproxPercentileCont
        )
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "count" => Self::Count,
            "sum" => Self::Sum,
            "avg" => Self::Avg,
            "min" => Self::Min,
            "max" => Self::Max,
            "approx_count_distinct" => Self::ApproxCountDistinct,
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
            Self::ApproxCountDistinct => "approx_count_distinct",
            Self::PercentileCont => "percentile_cont",
            Self::PercentileDisc => "percentile_disc",
            Self::Mode => "mode",
            Self::ApproxPercentileCont => "approx_percentile_cont",
            Self::Stddev => "stddev",
            Self::StddevSamp => "stddev_samp",
            Self::StddevPop => "stddev_pop",
            Self::Variance => "variance",
            Self::VarSamp => "var_samp",
            Self::VarPop => "var_pop",
            Self::OracleStddev => "oracle_stddev",
            Self::OracleVariance => "oracle_variance",
            Self::Rank => "rank",
            Self::DenseRank => "dense_rank",
            Self::PercentRank => "percent_rank",
            Self::CumeDist => "cume_dist",
            Self::RegrCount => "regr_count",
            Self::RegrSxx => "regr_sxx",
            Self::RegrSyy => "regr_syy",
            Self::RegrSxy => "regr_sxy",
            Self::RegrAvgx => "regr_avgx",
            Self::RegrAvgy => "regr_avgy",
            Self::RegrR2 => "regr_r2",
            Self::RegrSlope => "regr_slope",
            Self::RegrIntercept => "regr_intercept",
            Self::CovarPop => "covar_pop",
            Self::CovarSamp => "covar_samp",
            Self::Corr => "corr",
            Self::CorrS => "corr_s",
            Self::CorrK => "corr_k",
        }
    }

    pub(crate) fn keeps_nulls(self) -> bool {
        self.builds_json() || self == Self::ArrayAgg
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
    CurrentSetting {
        name: String,
        missing_ok: bool,
        parsed: Option<String>,
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
