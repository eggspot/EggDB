use crate::ast::{
    AlterAction, AlterRoutine, AlterRoutineAction, AlterTriggerAction, AssignedValue,
    BinaryOperator, ConflictResolution, CopySource, CreateRoutine, CursorCommand, Delete,
    DropRoutine, Expr, ExprKind, FromClause, GeneratedTable, Insert, InsertSource, InsertValue,
    JoinCondition, Literal, MaterializedViewCommand, MergeStatement, MultitableInsert,
    MySqlStatement, Name, OnDuplicate, OutputInto, OutputTarget, Query, ReferentialAction,
    RoutineTarget, Select, SelectItem, SetExpr, SqlServerStatement, Statement, TableReference,
    UnaryOperator, Update, VariableSource,
};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult};
use crate::execute::TableCache;
use crate::row_security::RowSecurityStatement;
use crate::session::{OneStatementError, Session};
use crate::store::StoreFailure;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};
use crate::triggers::TriggerEvent;
use crate::types::SqlType;

/// A statement that can destroy data or a rule protecting it, which `eggdb mcp` runs for an AI
/// agent only after its user confirmed it: what it does, and to what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructiveStatement {
    /// What the statement does, one part per kind in the order the statement first does it:
    /// an `ALTER TABLE` that changes a column's type and drops another has two parts.
    pub parts: Vec<DestructivePart>,
    /// Whether it also drops what depends on them (`CASCADE`).
    pub cascade: bool,
}

/// One kind of destruction a [`DestructiveStatement`] does, and to what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructivePart {
    /// What it does.
    pub kind: DestructiveKind,
    /// What it does it to, as the statement names them: tables, views, indexes, sequences or
    /// schemas, or `table.column` and `table.constraint`.
    pub objects: Vec<String>,
}

impl DestructiveStatement {
    fn new(kind: DestructiveKind, objects: Vec<String>, cascade: bool) -> Self {
        Self {
            parts: vec![DestructivePart { kind, objects }],
            cascade,
        }
    }

    /// Every object the statement acts on, part by part.
    pub fn objects(&self) -> impl Iterator<Item = &str> {
        self.parts
            .iter()
            .flat_map(|part| part.objects.iter().map(String::as_str))
    }

    fn add(&mut self, kind: DestructiveKind, object: String) {
        match self.parts.iter_mut().find(|part| part.kind == kind) {
            Some(part) => {
                if !part.objects.contains(&object) {
                    part.objects.push(object);
                }
            }
            None => self.parts.push(DestructivePart {
                kind,
                objects: vec![object],
            }),
        }
    }

    pub(crate) fn absorb(&mut self, other: DestructiveStatement) {
        for part in other.parts {
            for object in part.objects {
                self.add(part.kind, object);
            }
        }
        self.cascade |= other.cascade;
    }

    pub(crate) fn combined(
        found: Option<DestructiveStatement>,
        more: Option<DestructiveStatement>,
    ) -> Option<DestructiveStatement> {
        match (found, more) {
            (Some(mut found), Some(more)) => {
                found.absorb(more);
                Some(found)
            }
            (found, more) => found.or(more),
        }
    }
}

/// What a [`DestructiveStatement`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveKind {
    /// `DROP TABLE`.
    DropTable,
    /// `DROP VIEW`.
    DropView,
    /// `DROP INDEX`.
    DropIndex,
    /// `DROP SEQUENCE`.
    DropSequence,
    /// `DROP SCHEMA`.
    DropSchema,
    /// `DROP DATABASE`, which removes a served database's file.
    DropDatabase,
    /// `DROP TYPE ... CASCADE` or `DROP DOMAIN ... CASCADE`, which drops the columns of that
    /// type.
    DropType,
    /// `ALTER TABLE ... DROP COLUMN`.
    DropColumn,
    /// `ALTER TABLE ... DROP CONSTRAINT`.
    DropConstraint,
    /// MySQL's `ALTER TABLE ... DROP PRIMARY KEY`.
    DropPrimaryKey,
    /// MySQL's and Oracle's `ALTER TABLE ... DROP PARTITION`, which removes the partition's rows.
    DropPartition,
    /// `ALTER TABLE ... ALTER COLUMN ... TYPE`, or MySQL's `MODIFY` and `CHANGE`.
    ChangeColumnType,
    /// `TRUNCATE`.
    Truncate,
    /// A `DELETE` whose `WHERE` names no column of its table, or that has none.
    DeleteWithoutFilter,
    /// An `UPDATE` whose `WHERE` names no column of its table, or that has none.
    UpdateWithoutFilter,
    /// A `DELETE` whose `WHERE` does not pin the rows it removes by a key, one that also changes
    /// other rows through a foreign key or a trigger, or a `MERGE` that deletes.
    DeleteOfManyRows,
    /// An `UPDATE` whose `WHERE` does not pin the rows it changes by a key, one that also
    /// changes other rows through a foreign key or a trigger, or a `MERGE` that updates.
    UpdateOfManyRows,
    /// An `INSERT` that overwrites the rows it collides with: `REPLACE`, `INSERT OR REPLACE`,
    /// `ON DUPLICATE KEY UPDATE` or `ON CONFLICT ... DO UPDATE`.
    InsertOverwritingRows,
    /// SQLite's `UPDATE OR REPLACE`, which deletes the rows holding a primary key or unique
    /// value its new rows take, even when its `WHERE` pins the rows it changes.
    UpdateOverwritingRows,
    /// Code that may change or drop anything: `CALL` of a procedure, a `DO` or PL/SQL block,
    /// Transact-SQL's `EXEC`, a `SELECT` of a function, or an `INSERT` whose table fires a
    /// trigger (or that the classifier cannot look up, such as a view).
    RunCode,
    /// A change of code that runs later, in triggers, defaults or other routines: `CREATE OR
    /// REPLACE`, Transact-SQL's `ALTER PROCEDURE` or `CREATE OR ALTER`, `ALTER ... RENAME` or
    /// `SET SCHEMA`, or `DROP` of an existing function, procedure or package, or a new one
    /// named like a routine that an unqualified call already finds.
    ChangeRoutine,
    /// `DROP TRIGGER`.
    DropTrigger,
    /// `ALTER TABLE ... DISABLE TRIGGER`, `DISABLE TRIGGER` or `ALTER TRIGGER ... DISABLE`.
    DisableTrigger,
    /// `DROP POLICY`, which removes a row security policy.
    DropPolicy,
    /// `ALTER TABLE ... DISABLE ROW LEVEL SECURITY`.
    DisableRowSecurity,
    /// `ALTER TABLE ... NO FORCE ROW LEVEL SECURITY`, which lets the table's owner bypass its
    /// row security again.
    NoForceRowSecurity,
}

impl DestructiveKind {
    /// How a person reads it, such as `DROP TABLE` or `DELETE of the whole table`.
    pub fn label(self) -> &'static str {
        match self {
            Self::DropTable => "DROP TABLE",
            Self::DropView => "DROP VIEW",
            Self::DropIndex => "DROP INDEX",
            Self::DropSequence => "DROP SEQUENCE",
            Self::DropSchema => "DROP SCHEMA",
            Self::DropDatabase => "DROP DATABASE",
            Self::DropType => "DROP TYPE ... CASCADE",
            Self::DropColumn => "ALTER TABLE ... DROP COLUMN",
            Self::DropConstraint => "ALTER TABLE ... DROP of a constraint, key or index",
            Self::DropPrimaryKey => "ALTER TABLE ... DROP PRIMARY KEY",
            Self::DropPartition => "ALTER TABLE ... DROP PARTITION",
            Self::ChangeColumnType => "a change of a column's type",
            Self::Truncate => "TRUNCATE",
            Self::DeleteWithoutFilter => "DELETE of the whole table",
            Self::UpdateWithoutFilter => "UPDATE of the whole table",
            Self::DeleteOfManyRows => "DELETE that can remove many rows",
            Self::UpdateOfManyRows => "UPDATE that can change many rows",
            Self::InsertOverwritingRows => {
                "INSERT that overwrites existing rows (REPLACE or an upsert)"
            }
            Self::UpdateOverwritingRows => {
                "UPDATE OR REPLACE that deletes the rows it collides with"
            }
            Self::RunCode => {
                "code that may change or drop data (CALL, DO, EXEC, a function or a trigger)"
            }
            Self::ChangeRoutine => {
                "a replace, rename or drop of an existing function, procedure or package (code that may run later)"
            }
            Self::DropTrigger => "DROP TRIGGER",
            Self::DisableTrigger => "DISABLE TRIGGER",
            Self::DropPolicy => "DROP POLICY",
            Self::DisableRowSecurity => "DISABLE ROW LEVEL SECURITY",
            Self::NoForceRowSecurity => "NO FORCE ROW LEVEL SECURITY",
        }
    }
}

/// Parses `text` as [`Session::run_one_statement`] does, in `dialect`, without a database,
/// and says whether its one statement is destructive. It refuses what `run_one_statement`
/// refuses (no statement, a second one, transaction control, `COPY`) and fails on a parse
/// error, both before anything could run. With no database it knows no table's keys or
/// triggers, so every `DELETE`, `UPDATE` and `INSERT` is destructive here;
/// [`Session::destructive_statement`] on a session over the database reads them.
pub fn destructive_statement(
    text: &str,
    dialect: Dialect,
) -> Result<Option<DestructiveStatement>, OneStatementError> {
    Session::with_dialect(&NoDatabase, dialect).destructive_statement(text)
}

pub(crate) fn classify(
    statement: &Statement,
    keys: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    match statement {
        Statement::DropTable { names, cascade, .. } => {
            dropped(DestructiveKind::DropTable, names, *cascade)
        }
        Statement::DropView { names, cascade, .. } => {
            dropped(DestructiveKind::DropView, names, *cascade)
        }
        Statement::DropSequence { names, cascade, .. } => {
            dropped(DestructiveKind::DropSequence, names, *cascade)
        }
        Statement::DropSchema { names, cascade, .. } => {
            dropped(DestructiveKind::DropSchema, names, *cascade)
        }
        Statement::DropIndex(drop) => dropped(DestructiveKind::DropIndex, &drop.names, false),
        Statement::DropType {
            names,
            cascade: true,
            ..
        } => dropped(DestructiveKind::DropType, names, true),
        Statement::Truncate(table) => Some(DestructiveStatement::new(
            DestructiveKind::Truncate,
            vec![shown(&table.name)],
            false,
        )),
        Statement::Delete(delete) => deleted(delete, keys),
        Statement::Update(update) => updated(update, keys),
        Statement::Insert(insert) => inserted(insert, keys),
        Statement::AlterTable(alter) => altered(&alter.name, &alter.actions),
        Statement::MaterializedView(crate::ast::MaterializedViewCommand::Drop {
            names, ..
        }) => dropped(DestructiveKind::DropView, names, false),
        Statement::MaterializedView(MaterializedViewCommand::Create { query, .. }) => {
            match keys.parsed(query) {
                Some(parsed) => statement_running_code(&parsed, keys),
                None => run_code(vec!["MATERIALIZED VIEW".to_owned()]),
            }
        }
        Statement::MaterializedView(MaterializedViewCommand::Refresh { name, .. }) => {
            let parsed = keys
                .materialized_view_query(&name.text)
                .and_then(|query| keys.parsed(&query));
            match parsed {
                Some(parsed) => statement_running_code(&parsed, keys),
                None => run_code(vec![shown(name)]),
            }
        }
        Statement::Explain(explain) if explain.analyze => classify(&explain.statement, keys),
        Statement::Cursor(CursorCommand::Declare { query, .. }) => classify(query, keys),
        Statement::Copy(copy) => match &copy.source {
            CopySource::Query(query) => {
                let mut functions = CalledFunctions::default();
                query_user_functions(query, keys, &mut functions);
                running(functions.user)
            }
            CopySource::Table { .. } => None,
        },
        Statement::MySql(MySqlStatement::Set(assignments)) => {
            let mut functions = CalledFunctions::default();
            for assignment in assignments {
                if let AssignedValue::Expr(expr) = &assignment.value {
                    user_functions(expr, keys, &mut functions);
                }
            }
            running(functions.user)
        }
        Statement::DropRoutine(drop) => routine_dropped(drop, keys),
        Statement::CreateRoutine(create) => routine_created(create, keys),
        Statement::AlterRoutine(alter) => routine_altered(alter, keys),
        Statement::CreatePackage(create) => {
            package_changed(&create.package.name, create.package.body, keys)
        }
        Statement::DropPackage(drop) => package_changed(&drop.name, false, keys),
        Statement::InstallExtension { .. } => None,
        Statement::Database(crate::served_databases::DatabaseStatement::Drop { names, .. }) => {
            dropped(DestructiveKind::DropDatabase, names, false)
        }
        Statement::Database(crate::served_databases::DatabaseStatement::Create { .. }) => None,
        Statement::Select(select) => selected(select),
        Statement::Query(query) => queried(query),
        Statement::SqlServer(statement) => sqlserver(statement, keys),
        Statement::DropTrigger(drop) => Some(DestructiveStatement::new(
            DestructiveKind::DropTrigger,
            drop.names
                .iter()
                .map(|trigger| match &drop.table {
                    Some(table) => format!("{}.{}", shown(table), shown(&trigger.name)),
                    None => shown(&trigger.name),
                })
                .collect(),
            false,
        )),
        Statement::EnableTrigger(switch) if !switch.enable => {
            let table = shown(&switch.table);
            Some(DestructiveStatement::new(
                DestructiveKind::DisableTrigger,
                match &switch.names {
                    Some(names) => names
                        .iter()
                        .map(|name| format!("{table}.{}", shown(name)))
                        .collect(),
                    None => vec![table],
                },
                false,
            ))
        }
        Statement::AlterTrigger(alter) if alter.action == AlterTriggerAction::Disable => {
            Some(DestructiveStatement::new(
                DestructiveKind::DisableTrigger,
                vec![shown(&alter.name.name)],
                false,
            ))
        }
        Statement::RowSecurity(RowSecurityStatement::DropPolicy { table, name, .. }) => {
            Some(DestructiveStatement::new(
                DestructiveKind::DropPolicy,
                vec![format!("{}.{}", shown(table), shown(name))],
                false,
            ))
        }
        Statement::RowSecurity(RowSecurityStatement::Switch {
            table,
            enabled: false,
        }) => Some(DestructiveStatement::new(
            DestructiveKind::DisableRowSecurity,
            vec![shown(table)],
            false,
        )),
        Statement::RowSecurity(RowSecurityStatement::Force {
            table,
            forced: false,
        }) => Some(DestructiveStatement::new(
            DestructiveKind::NoForceRowSecurity,
            vec![shown(table)],
            false,
        )),
        Statement::CreateTable(_)
        | Statement::CreateTableLike(_)
        | Statement::Prepared(crate::ast::PreparedCommand::Prepare(_))
        | Statement::Begin { .. }
        | Statement::SetTransaction(_)
        | Statement::Commit
        | Statement::Rollback
        | Statement::Savepoint(_)
        | Statement::Analyze(_)
        | Statement::Role(_)
        | Statement::RowSecurity(
            RowSecurityStatement::CreatePolicy { .. }
            | RowSecurityStatement::Switch { enabled: true, .. }
            | RowSecurityStatement::Force { forced: true, .. },
        )
        | Statement::Listen(_)
        | Statement::Unlisten(_)
        | Statement::Notify { .. }
        | Statement::Set { .. }
        | Statement::Show { .. }
        | Statement::Reset { .. }
        | Statement::MySql(_)
        | Statement::Pragma(_)
        | Statement::VacuumInto(_)
        | Statement::CreateIndex(_)
        | Statement::Explain(_)
        | Statement::CreateSequence(_)
        | Statement::AlterSequence(_)
        | Statement::CreateSchema { .. }
        | Statement::RenameSchema { .. }
        | Statement::CreateView(_)
        | Statement::AlterView(_)
        | Statement::Comment(_)
        | Statement::Ignored { .. }
        | Statement::AlterSession(_)
        | Statement::Discard(_)
        | Statement::Cursor(_)
        | Statement::CreateType(_)
        | Statement::CreateDomain(_)
        | Statement::DropType { cascade: false, .. }
        | Statement::AlterType(_)
        | Statement::AlterDomain(_)
        | Statement::SetConstraints { .. }
        | Statement::CreateTrigger(_)
        | Statement::EnableTrigger(_)
        | Statement::AlterTrigger(_)
        | Statement::AlterPackage(_) => None,
        Statement::Call(call) => Some(DestructiveStatement::new(
            DestructiveKind::RunCode,
            vec![shown(&call.name)],
            false,
        )),
        Statement::Do(_) => Some(DestructiveStatement::new(
            DestructiveKind::RunCode,
            vec!["DO".to_owned()],
            false,
        )),
        Statement::Prepared(crate::ast::PreparedCommand::Execute(execute)) => {
            run_code(vec![execute.name.text.clone()])
        }
        Statement::PlsqlBlock(_) => Some(DestructiveStatement::new(
            DestructiveKind::RunCode,
            vec![crate::plsql::ANONYMOUS_BLOCK_NAME.to_owned()],
            false,
        )),
        Statement::With(with) => crate::with_statement::classify(with, keys),
        Statement::Merge(merge) => merged(merge, keys),
        Statement::CreateTableAs(create) => classify(&create.query, keys),
        Statement::MultitableInsert(insert) => multitable_inserted(insert, keys),
    }
}

fn run_code(objects: Vec<String>) -> Option<DestructiveStatement> {
    Some(DestructiveStatement::new(
        DestructiveKind::RunCode,
        objects,
        false,
    ))
}

fn sqlserver(
    statement: &SqlServerStatement,
    keys: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    match statement {
        SqlServerStatement::Execute(call)
        | SqlServerStatement::InsertVariable {
            source: VariableSource::Execute(call),
            ..
        } => run_code(vec![shown(&call.procedure)]),
        SqlServerStatement::ExecuteDynamic(_) => run_code(vec!["EXEC".to_owned()]),
        SqlServerStatement::InsertVariable {
            source: VariableSource::Query(query),
            ..
        }
        | SqlServerStatement::SelectAssign { query, .. } => classify(query, keys),
        SqlServerStatement::DeclareCursor(cursor) => classify(&cursor.query, keys),
        SqlServerStatement::OutputInto(output) => output_into(output, keys),
        SqlServerStatement::TopRows(top) => classify(&top.statement, keys),
        SqlServerStatement::InsertBulk(bulk) => insert_runs_code(&bulk.table, keys),
        SqlServerStatement::Use(_)
        | SqlServerStatement::NoCount(_)
        | SqlServerStatement::ImplicitTransactions(_)
        | SqlServerStatement::IdentityInsert { .. }
        | SqlServerStatement::DeclareTable { .. }
        | SqlServerStatement::InsertVariable {
            source: VariableSource::Values(_),
            ..
        }
        | SqlServerStatement::UpdateVariable { .. }
        | SqlServerStatement::DeleteVariable { .. }
        | SqlServerStatement::FormatOnly(_)
        | SqlServerStatement::Showplan { .. }
        | SqlServerStatement::XactAbort(_)
        | SqlServerStatement::Declare(_)
        | SqlServerStatement::SetVariable { .. }
        | SqlServerStatement::Condition(_)
        | SqlServerStatement::Print(_)
        | SqlServerStatement::RaiseError(_)
        | SqlServerStatement::Throw(_)
        | SqlServerStatement::SaveTransaction(_)
        | SqlServerStatement::RollbackTo(_)
        | SqlServerStatement::Return(_)
        | SqlServerStatement::OpenCursor(_)
        | SqlServerStatement::FetchCursor { .. }
        | SqlServerStatement::CloseCursor(_)
        | SqlServerStatement::DeallocateCursor(_)
        | SqlServerStatement::WaitFor { .. } => None,
    }
}

fn selected(select: &Select) -> Option<DestructiveStatement> {
    let mut functions = Vec::new();
    select_running_code(select, &mut functions);
    running(functions)
}

fn queried(query: &Query) -> Option<DestructiveStatement> {
    let mut functions = Vec::new();
    query_running_code(query, &mut functions);
    running(functions)
}

pub(crate) fn query_runs_code(query: &Query) -> Option<DestructiveStatement> {
    queried(query)
}

fn running(functions: Vec<String>) -> Option<DestructiveStatement> {
    let mut found: Option<DestructiveStatement> = None;
    for function in functions {
        match &mut found {
            Some(statement) => statement.add(DestructiveKind::RunCode, function),
            None => found = run_code(vec![function]),
        }
    }
    found
}

fn query_running_code(query: &Query, functions: &mut Vec<String>) {
    for common in &query.with {
        query_running_code(&common.query, functions);
    }
    body_running_code(&query.body, functions);
}

fn body_running_code(body: &SetExpr, functions: &mut Vec<String>) {
    match body {
        SetExpr::Select(select) => select_running_code(select, functions),
        SetExpr::Operation(operation) => {
            body_running_code(&operation.left, functions);
            body_running_code(&operation.right, functions);
        }
        SetExpr::Query(query) => query_running_code(query, functions),
    }
}

fn select_running_code(select: &Select, functions: &mut Vec<String>) {
    if !crate::routines::select_may_write(select) {
        return;
    }
    let before = functions.len();
    if let Some(from) = &select.from
        && let Some(generated) = &from.first.generated
        && let GeneratedTable::Function(function) = generated.as_ref()
    {
        functions.push(shown_text(&function.name));
    } else {
        for expr in crate::subquery::select_expressions(select) {
            function_names(expr, functions);
        }
    }
    if functions.len() == before {
        functions.push("SELECT".to_owned());
    }
}

fn function_names(expr: &Expr, functions: &mut Vec<String>) {
    if let ExprKind::Function { name, .. } = &expr.kind
        && !name.is_empty()
        && !name.starts_with('*')
    {
        functions.push(shown_text(name));
    }
    for child in crate::eval::children(expr) {
        function_names(child, functions);
    }
}

fn shown_text(name: &str) -> String {
    name.replace(crate::schemas::QUALIFIER, ".")
}

fn deleted(delete: &Delete, keys: &mut CatalogLookup<'_>) -> Option<DestructiveStatement> {
    let kind = match row_reach(delete.filter.as_ref(), &delete.table, keys) {
        RowReach::WholeTable => Some(DestructiveKind::DeleteWithoutFilter),
        RowReach::Pinned(table) if !table.delete_reaches_further && delete.using.is_none() => None,
        RowReach::Pinned(_) | RowReach::Many => Some(DestructiveKind::DeleteOfManyRows),
    };
    let mut functions = CalledFunctions::default();
    optional_user_functions(delete.filter.as_ref(), keys, &mut functions);
    returning_user_functions(delete.returning.as_deref(), keys, &mut functions);
    let mut names = vec![shown(&delete.table.name)];
    if let Some(joined) = &delete.using {
        from_user_functions(&joined.tables, keys, &mut functions);
        for &at in &joined.more_targets_at {
            if let Some(reference) = joined.references().nth(at) {
                names.push(shown(&reference.name));
            }
        }
    }
    DestructiveStatement::combined(
        kind.map(|kind| DestructiveStatement::new(kind, names, false)),
        running(functions.user),
    )
}

fn updated(update: &Update, keys: &mut CatalogLookup<'_>) -> Option<DestructiveStatement> {
    let kind = match row_reach(update.filter.as_ref(), &update.table, keys) {
        RowReach::WholeTable => Some(DestructiveKind::UpdateWithoutFilter),
        RowReach::Pinned(table)
            if !table.update_reaches_further_with(update) && update.from.is_none() =>
        {
            None
        }
        RowReach::Pinned(_) | RowReach::Many => Some(DestructiveKind::UpdateOfManyRows),
    };
    let mut functions = CalledFunctions::default();
    for (_, value) in &update.assignments {
        if let InsertValue::Expr(expr) = value {
            user_functions(expr, keys, &mut functions);
        }
    }
    optional_user_functions(update.filter.as_ref(), keys, &mut functions);
    returning_user_functions(update.returning.as_deref(), keys, &mut functions);
    if let Some(joined) = &update.from {
        from_user_functions(&joined.tables, keys, &mut functions);
    }
    let mut found = kind.map(|kind| DestructiveStatement::new(kind, updated_tables(update), false));
    if update.conflict_resolution == ConflictResolution::Replace {
        found = DestructiveStatement::combined(
            found,
            Some(DestructiveStatement::new(
                DestructiveKind::UpdateOverwritingRows,
                vec![shown(&update.table.name)],
                false,
            )),
        );
    }
    DestructiveStatement::combined(found, running(functions.user))
}

fn updated_tables(update: &Update) -> Vec<String> {
    let mut names = vec![shown(&update.table.name)];
    let Some(joined) = update
        .from
        .as_ref()
        .filter(|joined| joined.writes_any_table)
    else {
        return names;
    };
    let unqualified = (0..update.assignments.len())
        .any(|at| joined.assigned_at.get(at).copied().flatten().is_none());
    for (at, reference) in joined.references().enumerate() {
        let written = unqualified || joined.assigned_at.contains(&Some(at));
        if written && reference.derived.is_none() && reference.generated.is_none() {
            push_new(&mut names, shown(&reference.name));
        }
    }
    names
}

fn inserted(insert: &Insert, keys: &mut CatalogLookup<'_>) -> Option<DestructiveStatement> {
    let found = match insert.on_duplicate {
        OnDuplicate::Replace | OnDuplicate::Update(..) => Some(DestructiveStatement::new(
            DestructiveKind::InsertOverwritingRows,
            vec![shown(&insert.table.name)],
            false,
        )),
        OnDuplicate::Fail | OnDuplicate::Ignore => insert_runs_code(&insert.table.name, keys),
    };
    let mut functions = CalledFunctions::default();
    match &insert.source {
        InsertSource::Values(rows) => {
            for value in rows.iter().flatten() {
                if let InsertValue::Expr(expr) = value {
                    user_functions(expr, keys, &mut functions);
                }
            }
        }
        InsertSource::Query(query) => query_user_functions(query, keys, &mut functions),
        InsertSource::DefaultValues => {}
    }
    if let OnDuplicate::Update(assignments, filter) = &insert.on_duplicate {
        for (_, expr) in assignments {
            user_functions(expr, keys, &mut functions);
        }
        if let Some(filter) = filter {
            user_functions(filter, keys, &mut functions);
        }
    }
    returning_user_functions(insert.returning.as_deref(), keys, &mut functions);
    DestructiveStatement::combined(found, running(functions.user))
}

fn output_into(output: &OutputInto, keys: &mut CatalogLookup<'_>) -> Option<DestructiveStatement> {
    let found = classify(&output.statement, keys);
    match &output.destination.target {
        OutputTarget::Table(table) => {
            DestructiveStatement::combined(found, insert_runs_code(&table.name, keys))
        }
        OutputTarget::Variable(_) => found,
    }
}

fn multitable_inserted(
    insert: &MultitableInsert,
    keys: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    let mut found = None;
    for target in insert.targets() {
        found = DestructiveStatement::combined(found, insert_runs_code(&target.table.name, keys));
    }
    let mut functions = CalledFunctions::default();
    query_user_functions(&insert.source, keys, &mut functions);
    DestructiveStatement::combined(found, running(functions.user))
}

fn optional_user_functions(
    expr: Option<&Expr>,
    catalog: &mut CatalogLookup<'_>,
    functions: &mut CalledFunctions,
) {
    if let Some(expr) = expr {
        user_functions(expr, catalog, functions);
    }
}

fn returning_user_functions(
    returning: Option<&[SelectItem]>,
    catalog: &mut CatalogLookup<'_>,
    functions: &mut CalledFunctions,
) {
    for item in returning.unwrap_or_default() {
        if let SelectItem::Expr { expr, .. } = item {
            user_functions(expr, catalog, functions);
        }
    }
}

fn statement_running_code(
    statement: &Statement,
    catalog: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    let mut functions = CalledFunctions::default();
    match statement {
        Statement::Query(query) => query_user_functions(query, catalog, &mut functions),
        Statement::Select(select) => select_user_functions(select, catalog, &mut functions),
        other => return classify(other, catalog),
    }
    running(functions.user)
}

fn query_user_functions(
    query: &Query,
    catalog: &mut CatalogLookup<'_>,
    functions: &mut CalledFunctions,
) {
    for common in &query.with {
        query_user_functions(&common.query, catalog, functions);
    }
    body_user_functions(&query.body, catalog, functions);
}

fn body_user_functions(
    body: &SetExpr,
    catalog: &mut CatalogLookup<'_>,
    functions: &mut CalledFunctions,
) {
    match body {
        SetExpr::Select(select) => select_user_functions(select, catalog, functions),
        SetExpr::Operation(operation) => {
            body_user_functions(&operation.left, catalog, functions);
            body_user_functions(&operation.right, catalog, functions);
        }
        SetExpr::Query(query) => query_user_functions(query, catalog, functions),
    }
}

fn select_user_functions(
    select: &Select,
    catalog: &mut CatalogLookup<'_>,
    functions: &mut CalledFunctions,
) {
    for expr in crate::subquery::select_expressions(select) {
        user_functions(expr, catalog, functions);
    }
    if let Some(from) = &select.from {
        listed_user_functions(from, catalog, functions);
    }
}

fn from_user_functions(
    from: &FromClause,
    catalog: &mut CatalogLookup<'_>,
    functions: &mut CalledFunctions,
) {
    for table in std::iter::once(&from.first).chain(from.joins.iter().map(|join| &join.table)) {
        if let Some(generated) = &table.generated {
            for expr in crate::set_returning::expressions(generated) {
                user_functions(expr, catalog, functions);
            }
        }
    }
    for join in &from.joins {
        if let JoinCondition::On(expr) = &join.condition {
            user_functions(expr, catalog, functions);
        }
    }
    listed_user_functions(from, catalog, functions);
}

fn listed_user_functions(
    from: &FromClause,
    catalog: &mut CatalogLookup<'_>,
    functions: &mut CalledFunctions,
) {
    for table in std::iter::once(&from.first).chain(from.joins.iter().map(|join| &join.table)) {
        if let Some(derived) = &table.derived {
            query_user_functions(&derived.query, catalog, functions);
        }
        if let Some(generated) = &table.generated
            && let GeneratedTable::Function(function) = generated.as_ref()
        {
            functions.look_up(&function.name, catalog);
        }
    }
}

fn user_functions(expr: &Expr, catalog: &mut CatalogLookup<'_>, functions: &mut CalledFunctions) {
    match &expr.kind {
        ExprKind::Function { name, arguments } if name == crate::routines::ROUTINE_CALL => {
            if let Some(Expr {
                kind: ExprKind::Literal(Literal::String(called)),
                ..
            }) = arguments.first()
            {
                push_new(&mut functions.user, shown_text(called));
            }
        }
        ExprKind::Function { name, .. } if !name.is_empty() && !name.starts_with('*') => {
            functions.look_up(name, catalog);
        }
        ExprKind::Subquery(subquery) => query_user_functions(&subquery.query, catalog, functions),
        _ => {}
    }
    for child in crate::eval::children(expr) {
        user_functions(child, catalog, functions);
    }
}

#[derive(Default)]
struct CalledFunctions {
    user: Vec<String>,
    looked_up: Vec<String>,
}

impl CalledFunctions {
    fn look_up(&mut self, name: &str, catalog: &mut CatalogLookup<'_>) {
        if self.looked_up.iter().any(|seen| seen == name) {
            return;
        }
        self.looked_up.push(name.to_owned());
        if catalog.is_user_routine(name) {
            push_new(&mut self.user, shown_text(name));
        }
    }
}

fn push_new(functions: &mut Vec<String>, name: String) {
    if !functions.contains(&name) {
        functions.push(name);
    }
}

fn routine_drop_cascading(
    drop: &DropRoutine,
    catalog: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    let mut triggers = Vec::new();
    for target in &drop.targets {
        match catalog.triggers_calling(drop.kind, target) {
            Some(found) => triggers.extend(found),
            None => triggers.push(shown(&target.name)),
        }
    }
    (!triggers.is_empty())
        .then(|| DestructiveStatement::new(DestructiveKind::DropTrigger, triggers, true))
}

fn routine_created(
    create: &CreateRoutine,
    catalog: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    let written = create.name.text.as_str();
    let base = crate::schemas::base_name(crate::schemas::normalized(written));
    let mut names = Vec::new();
    let mut replaces = false;
    let mut looked_up = vec![written];
    if base != written {
        looked_up.push(base);
    }
    for looked_up in looked_up {
        match catalog.existing_routine(looked_up) {
            ExistingObject::Found(internal) => {
                replaces = true;
                push_new(&mut names, internal);
            }
            ExistingObject::Unknown => replaces = true,
            ExistingObject::Missing => {}
        }
    }
    match catalog.creation_name(&create.name) {
        Some(created) => push_new(&mut names, created),
        None => replaces = true,
    }
    if written
        .rsplit(['.', crate::schemas::QUALIFIER])
        .next()
        .is_some_and(crate::eval::is_builtin_function)
    {
        replaces = true;
    }
    routine_changed(shown(&create.name), &names, replaces, catalog)
}

fn routine_altered(
    alter: &AlterRoutine,
    catalog: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    if matches!(
        alter.action,
        AlterRoutineAction::Accepted | AlterRoutineAction::Settings(_)
    ) {
        return None;
    }
    let old = match catalog.existing_routine(&alter.target.name.text) {
        ExistingObject::Missing => return None,
        ExistingObject::Unknown => return changed_routines(vec![shown(&alter.target.name)]),
        ExistingObject::Found(internal) => internal,
    };
    let new = match &alter.action {
        AlterRoutineAction::RenameTo(to) => match old.split_once(crate::schemas::QUALIFIER) {
            Some((schema, _)) => crate::schemas::qualified(schema, &to.text),
            None => to.text.clone(),
        },
        AlterRoutineAction::SetSchema(schema) => {
            crate::schemas::qualified(&schema.text, crate::schemas::base_name(&old))
        }
        AlterRoutineAction::Settings(_) | AlterRoutineAction::Accepted => return None,
    };
    routine_changed(shown(&alter.target.name), &[old, new], true, catalog)
}

fn routine_changed(
    shown_name: String,
    internal_names: &[String],
    replaces: bool,
    catalog: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    let mut triggers = Vec::new();
    for name in internal_names {
        match catalog.triggers_naming(name) {
            Some(found) => {
                for trigger in found {
                    push_new(&mut triggers, trigger);
                }
            }
            None => return changed_routines(vec![shown_name]),
        }
    }
    if !triggers.is_empty() {
        return Some(DestructiveStatement::new(
            DestructiveKind::DisableTrigger,
            triggers,
            false,
        ));
    }
    replaces
        .then(|| DestructiveStatement::new(DestructiveKind::ChangeRoutine, vec![shown_name], false))
}

fn routine_dropped(
    drop: &DropRoutine,
    catalog: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    let cascaded = if drop.cascade {
        routine_drop_cascading(drop, catalog)
    } else {
        None
    };
    let existing = drop
        .targets
        .iter()
        .filter(|target| catalog.existing_routine(&target.name.text) != ExistingObject::Missing)
        .map(|target| shown(&target.name))
        .collect();
    DestructiveStatement::combined(cascaded, changed_routines(existing))
}

fn package_changed(
    name: &Name,
    body: bool,
    catalog: &mut CatalogLookup<'_>,
) -> Option<DestructiveStatement> {
    match catalog.existing_package(&name.text, body) {
        ExistingObject::Missing => None,
        ExistingObject::Found(_) | ExistingObject::Unknown => changed_routines(vec![shown(name)]),
    }
}

fn changed_routines(names: Vec<String>) -> Option<DestructiveStatement> {
    (!names.is_empty())
        .then(|| DestructiveStatement::new(DestructiveKind::ChangeRoutine, names, false))
}

fn merged(merge: &MergeStatement, keys: &mut CatalogLookup<'_>) -> Option<DestructiveStatement> {
    let deletes = merge
        .matched
        .iter()
        .chain(&merge.by_source)
        .any(|clause| clause.delete_condition.is_some());
    let found = match merge.matched.first().or(merge.by_source.first()) {
        Some(_) => {
            let kind = if deletes {
                DestructiveKind::DeleteOfManyRows
            } else {
                DestructiveKind::UpdateOfManyRows
            };
            Some(DestructiveStatement::new(
                kind,
                vec![shown(&merge.target.name)],
                false,
            ))
        }
        None if merge.insert.is_some() => insert_runs_code(&merge.target.name, keys),
        None => None,
    };
    let mut functions = CalledFunctions::default();
    select_user_functions(&merge.source, keys, &mut functions);
    user_functions(&merge.on, keys, &mut functions);
    for update in &merge.matched {
        for (_, expr) in &update.assignments {
            user_functions(expr, keys, &mut functions);
        }
        optional_user_functions(update.condition.as_ref(), keys, &mut functions);
        optional_user_functions(update.delete_condition.as_ref(), keys, &mut functions);
    }
    if let Some(insert) = &merge.insert {
        for expr in &insert.values {
            user_functions(expr, keys, &mut functions);
        }
        optional_user_functions(insert.condition.as_ref(), keys, &mut functions);
    }
    for clause in &merge.by_source {
        for (_, expr) in &clause.assignments {
            user_functions(expr, keys, &mut functions);
        }
        optional_user_functions(clause.condition.as_ref(), keys, &mut functions);
    }
    returning_user_functions(merge.output.as_deref(), keys, &mut functions);
    DestructiveStatement::combined(found, running(functions.user))
}

fn insert_runs_code(table: &Name, keys: &mut CatalogLookup<'_>) -> Option<DestructiveStatement> {
    match keys.table_keys(&table.text) {
        Some(found) if !found.insert_reaches_further => None,
        _ => run_code(vec![shown(table)]),
    }
}

fn dropped(kind: DestructiveKind, names: &[Name], cascade: bool) -> Option<DestructiveStatement> {
    Some(DestructiveStatement::new(
        kind,
        names.iter().map(shown).collect(),
        cascade,
    ))
}

fn altered(table: &Name, actions: &[AlterAction]) -> Option<DestructiveStatement> {
    let table = shown(table);
    let mut found: Option<DestructiveStatement> = None;
    for action in actions {
        let (kind, object, cascade) = match action {
            AlterAction::DropColumn { name, cascade, .. } => (
                DestructiveKind::DropColumn,
                format!("{table}.{}", shown(name)),
                *cascade,
            ),
            AlterAction::DropConstraint { name, cascade, .. } => (
                DestructiveKind::DropConstraint,
                format!("{table}.{}", shown(name)),
                *cascade,
            ),
            AlterAction::DropPrimaryKey(_) => {
                (DestructiveKind::DropPrimaryKey, table.clone(), false)
            }
            AlterAction::DropPartitions(partitions) => (
                DestructiveKind::DropPartition,
                partitions
                    .iter()
                    .map(|partition| format!("{table}.{}", shown(partition)))
                    .collect::<Vec<_>>()
                    .join(", "),
                false,
            ),
            AlterAction::TruncatePartitions(partitions) => (
                DestructiveKind::Truncate,
                partitions
                    .iter()
                    .map(|partition| format!("{table}.{}", shown(partition)))
                    .collect::<Vec<_>>()
                    .join(", "),
                false,
            ),
            AlterAction::SetType { column, .. } => (
                DestructiveKind::ChangeColumnType,
                format!("{table}.{}", shown(column)),
                false,
            ),
            AlterAction::Redefine { old_name, .. } => (
                DestructiveKind::ChangeColumnType,
                format!("{table}.{}", shown(old_name)),
                false,
            ),
            AlterAction::AddColumn { .. }
            | AlterAction::RenameColumn { .. }
            | AlterAction::RenameTable(_)
            | AlterAction::SetDefault { .. }
            | AlterAction::SetNotNull { .. }
            | AlterAction::AddConstraint(_)
            | AlterAction::SetSchema(_)
            | AlterAction::AddIdentity { .. }
            | AlterAction::AddDefault { .. }
            | AlterAction::EnableConstraint(_)
            | AlterAction::ValidateConstraint(_)
            | AlterAction::RenameConstraint { .. }
            | AlterAction::AlterIdentity { .. }
            | AlterAction::DropIdentity { .. }
            | AlterAction::SetExpression { .. }
            | AlterAction::DropExpression { .. }
            | AlterAction::AttachPartition { .. }
            | AlterAction::DetachPartition(_)
            | AlterAction::AddPartitions { .. }
            | AlterAction::ReorganizePartitions { .. }
            | AlterAction::SplitPartition { .. }
            | AlterAction::ExchangePartition { .. }
            | AlterAction::CoalescePartitions { .. }
            | AlterAction::MaintainPartitions(_)
            | AlterAction::MergePartitions { .. }
            | AlterAction::MergePartitionSpan { .. }
            | AlterAction::ModifyPartitionValues { .. }
            | AlterAction::RowMovement(_)
            | AlterAction::AttachIndex(_)
            | AlterAction::MoveColumn { .. }
            | AlterAction::AddIndex(_)
            | AlterAction::RenameIndex { .. }
            | AlterAction::Accepted => continue,
        };
        match &mut found {
            Some(statement) => {
                statement.add(kind, object);
                statement.cascade |= cascade;
            }
            None => found = Some(DestructiveStatement::new(kind, vec![object], cascade)),
        }
    }
    found
}

fn shown(name: &Name) -> String {
    shown_text(&name.text)
}

fn qualifies_target(qualifier: &str, table: &TableReference) -> bool {
    table
        .alias
        .as_deref()
        .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
        || qualifier.eq_ignore_ascii_case(crate::schemas::base_name(&table.name.text))
        || qualifier.eq_ignore_ascii_case(&shown(&table.name))
}

enum RowReach {
    WholeTable,
    Many,
    Pinned(TableKeys),
}

fn row_reach(
    filter: Option<&Expr>,
    table: &TableReference,
    keys: &mut CatalogLookup<'_>,
) -> RowReach {
    let names_target = |qualifier: Option<&str>| {
        qualifier.is_none_or(|qualifier| qualifies_target(qualifier, table))
    };
    let Some(filter) = filter.filter(|filter| names_column(filter, &names_target)) else {
        return RowReach::WholeTable;
    };
    match keys.table_keys(&table.name.text) {
        Some(found) if found.pins_rows(filter, table) => RowReach::Pinned(found),
        _ => RowReach::Many,
    }
}

const MOST_PINNED_ROWS: usize = 100;

/// What the classifier reads from the database a statement would run against.
pub(crate) trait ClassifierCatalog {
    /// Finds what the classifier needs to know of a table by its internal name, or `None` when
    /// it cannot tell (no database, a view, or a read that failed). A relation that does not
    /// exist has no keys and reaches nothing: a statement naming it fails before it changes
    /// anything.
    fn table_keys(&mut self, table: &str) -> Option<TableKeys>;

    /// Whether a function call written as `name` can call a routine a user created, which can
    /// write; `true` when it cannot tell.
    fn is_user_routine(&mut self, name: &str) -> bool;

    /// The triggers, as `table.trigger`, whose action calls the routine `target` names, or
    /// `None` when it cannot tell.
    fn triggers_calling(
        &mut self,
        kind: Option<crate::routines::RoutineKind>,
        target: &RoutineTarget,
    ) -> Option<Vec<String>>;

    /// The internal name of the routine a call written as `name` finds.
    fn existing_routine(&mut self, name: &str) -> ExistingObject;

    /// The internal name of the package `name` finds; with `body`, only one that has a body.
    fn existing_package(&mut self, name: &str, body: bool) -> ExistingObject;

    /// The internal name `CREATE` gives a routine written as `name`, or `None` when it cannot
    /// tell.
    fn creation_name(&mut self, name: &Name) -> Option<String>;

    /// The triggers, as `table.trigger`, whose action names the routine `internal`, whether
    /// or not it exists, or `None` when it cannot tell.
    fn triggers_naming(&mut self, internal: &str) -> Option<Vec<String>>;

    /// The query a materialized view stores, or `None` when it cannot tell.
    fn materialized_view_query(&mut self, name: &str) -> Option<String>;

    /// The one statement `text` holds, parsed as the session parses it, or `None` when it
    /// does not parse to one.
    fn parsed(&mut self, text: &str) -> Option<Statement>;
}

/// Whether a routine or package a statement names exists, as the classifier reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExistingObject {
    /// It does not exist.
    Missing,
    /// It exists, under this internal name.
    Found(String),
    /// The classifier cannot tell.
    Unknown,
}

pub(crate) type CatalogLookup<'a> = dyn ClassifierCatalog + 'a;

/// A table's keys and what else a change of its rows reaches, as the classifier needs them to
/// tell whether a `DELETE` or `UPDATE` changes only a few rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableKeys {
    columns: Vec<KeyedColumn>,
    keys: Vec<Vec<usize>>,
    delete_reaches_further: bool,
    update_reaches_further: bool,
    insert_reaches_further: bool,
    columns_cascading_on_update: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyedColumn {
    name: String,
    character: bool,
}

/// Reads the keys of table `name`: its primary key and its unique indexes, whether a `DELETE`,
/// `UPDATE` or `INSERT` of its rows fires a trigger, and which foreign keys of other tables
/// change their rows when its rows are deleted or their referenced columns updated. A
/// partitioned table's rows live in its partitions, so the triggers and foreign keys of every
/// partition, at any depth, count as the table's own. A relation that does not exist gives
/// keys of no columns that reach nothing; a view is an error.
pub(crate) fn table_keys(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<Option<TableKeys>> {
    let Some(table) = tables.table(reader, name)? else {
        return Ok(Some(TableKeys {
            columns: Vec::new(),
            keys: Vec::new(),
            delete_reaches_further: false,
            update_reaches_further: false,
            insert_reaches_further: false,
            columns_cascading_on_update: Vec::new(),
        }));
    };
    let mut keys = Vec::new();
    if !table.primary_key.is_empty() {
        keys.push(table.primary_key.clone());
    }
    keys.extend(
        table
            .indexes
            .iter()
            .filter(|index| index.unique && !index.columns.is_empty())
            .map(|index| index.columns.clone()),
    );
    let mut found = TableKeys {
        columns: table
            .columns
            .iter()
            .map(|column| KeyedColumn {
                name: column.name.clone(),
                character: column.sql_type.is_character(),
            })
            .collect(),
        keys,
        delete_reaches_further: false,
        update_reaches_further: false,
        insert_reaches_further: false,
        columns_cascading_on_update: Vec::new(),
    };
    found.add_reach_of(reader, tables, &table, &table.name)?;
    let partitions = crate::partitions::catalog(reader, tables)?.descendants_of(&table.name);
    for partition in partitions {
        found.add_reach_of(reader, tables, &table, &partition)?;
        for moved_row_event in [TriggerEvent::Insert, TriggerEvent::Delete] {
            found.update_reaches_further |=
                crate::triggers::fire_on(reader, tables, &partition, moved_row_event)?;
        }
    }
    Ok(Some(found))
}

impl TableKeys {
    fn reach_everything(&mut self) {
        self.delete_reaches_further = true;
        self.update_reaches_further = true;
        self.insert_reaches_further = true;
    }

    fn add_reach_of(
        &mut self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        root: &TableDefinition,
        relation: &str,
    ) -> SqlResult<()> {
        if tables.table(reader, relation)?.is_none() {
            self.reach_everything();
            return Ok(());
        }
        let fires = |tables: &mut TableCache, event: TriggerEvent| {
            crate::triggers::fire_on(reader, tables, relation, event)
        };
        self.delete_reaches_further |= fires(tables, TriggerEvent::Delete)?;
        self.update_reaches_further |= fires(tables, TriggerEvent::Update)?;
        self.insert_reaches_further |= fires(tables, TriggerEvent::Insert)?;
        for child_name in crate::constraints::referencing_tables(reader, relation)? {
            let Some(child) = tables.table(reader, &child_name)? else {
                self.delete_reaches_further = true;
                self.update_reaches_further = true;
                continue;
            };
            for key in child
                .foreign_keys
                .iter()
                .filter(|key| key.parent == relation)
            {
                self.delete_reaches_further |= changes_child_rows(key.on_delete);
                if !changes_child_rows(key.on_update) {
                    continue;
                }
                for column in &key.parent_columns {
                    match root.column_index(column) {
                        Some(index) => self.columns_cascading_on_update.push(index),
                        None => self.update_reaches_further = true,
                    }
                }
            }
        }
        Ok(())
    }
}

fn changes_child_rows(action: ReferentialAction) -> bool {
    match action {
        ReferentialAction::Cascade | ReferentialAction::SetNull | ReferentialAction::SetDefault => {
            true
        }
        ReferentialAction::NoAction | ReferentialAction::Restrict => false,
    }
}

impl TableKeys {
    fn pins_rows(&self, filter: &Expr, table: &TableReference) -> bool {
        let mut conjuncts = Vec::new();
        top_level_conjuncts(filter, &mut conjuncts);
        let mut values_per_column: Vec<Option<usize>> = vec![None; self.columns.len()];
        for conjunct in conjuncts {
            let Some((column, values)) = self.pinned_column(conjunct, table) else {
                continue;
            };
            if let Some(known) = values_per_column.get_mut(column) {
                *known = Some(known.map_or(values, |earlier| earlier.min(values)));
            }
        }
        self.keys.iter().any(|key| {
            !key.is_empty()
                && key
                    .iter()
                    .try_fold(1usize, |rows, column| {
                        let values = values_per_column.get(*column).copied().flatten()?;
                        rows.checked_mul(values)
                            .filter(|rows| *rows <= MOST_PINNED_ROWS)
                    })
                    .is_some()
        })
    }

    fn pinned_column(&self, conjunct: &Expr, table: &TableReference) -> Option<(usize, usize)> {
        match &conjunct.kind {
            ExprKind::Binary(BinaryOperator::Equal, left, right) => {
                let (column, value) = match self.target_column(left, table) {
                    Some(column) => (column, right),
                    None => (self.target_column(right, table)?, left),
                };
                self.pins_to(column, value).then_some((column, 1))
            }
            ExprKind::InList {
                expr,
                list,
                negated: false,
            } => {
                let column = self.target_column(expr, table)?;
                (list.len() <= MOST_PINNED_ROWS
                    && list.iter().all(|value| self.pins_to(column, value)))
                .then_some((column, list.len()))
            }
            _ => None,
        }
    }

    fn target_column(&self, expr: &Expr, table: &TableReference) -> Option<usize> {
        let ExprKind::Column { qualifier, name } = &expr.kind else {
            return None;
        };
        if qualifier
            .as_deref()
            .is_some_and(|qualifier| !qualifies_target(qualifier, table))
        {
            return None;
        }
        self.column_named(name)
    }

    fn column_named(&self, name: &str) -> Option<usize> {
        if let Some(exact) = self.columns.iter().position(|column| column.name == name) {
            return Some(exact);
        }
        let mut alike = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name.eq_ignore_ascii_case(name))
            .map(|(index, _)| index);
        let first = alike.next()?;
        alike.next().is_none().then_some(first)
    }

    fn pins_to(&self, column: usize, value: &Expr) -> bool {
        let character = self
            .columns
            .get(column)
            .is_none_or(|column| column.character);
        is_one_value(value, character)
    }

    fn update_reaches_further_with(&self, update: &Update) -> bool {
        self.update_reaches_further
            || update.assignments.iter().any(|(name, _)| {
                self.column_named(&name.text)
                    .is_none_or(|column| self.columns_cascading_on_update.contains(&column))
            })
    }
}

fn is_one_value(expr: &Expr, compared_with_characters: bool) -> bool {
    match &expr.kind {
        ExprKind::Literal(Literal::String(_) | Literal::Null) => true,
        ExprKind::Literal(Literal::Integer(_) | Literal::Decimal(_) | Literal::Boolean(_)) => {
            !compared_with_characters
        }
        ExprKind::Constant { sql_type, .. } => {
            !compared_with_characters || sql_type.is_some_and(SqlType::is_character)
        }
        ExprKind::Unary(UnaryOperator::Minus | UnaryOperator::Plus, operand) => {
            !compared_with_characters && is_one_value(operand, false)
        }
        ExprKind::Cast { expr, sql_type } => {
            (!compared_with_characters || sql_type.is_character()) && is_one_value(expr, false)
        }
        _ => false,
    }
}

fn top_level_conjuncts<'a>(expr: &'a Expr, found: &mut Vec<&'a Expr>) {
    match &expr.kind {
        ExprKind::And(parts) => {
            for part in parts {
                top_level_conjuncts(part, found);
            }
        }
        _ => found.push(expr),
    }
}

fn names_column(expr: &Expr, names_target: &dyn Fn(Option<&str>) -> bool) -> bool {
    if let ExprKind::Column { qualifier, .. } = &expr.kind
        && names_target(qualifier.as_deref())
    {
        return true;
    }
    crate::eval::children(expr)
        .into_iter()
        .any(|child| names_column(child, names_target))
}

struct NoDatabase;

enum NoAccess {}

impl KeyValueRead for NoAccess {
    fn get(&self, _key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        match *self {}
    }

    fn scan(
        &self,
        _start: &[u8],
        _end: &[u8],
        _direction: ScanDirection,
        _visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        match *self {}
    }
}

impl KeyValueWrite for NoAccess {
    fn put(&mut self, _key: &[u8], _value: &[u8]) -> SqlResult<()> {
        match *self {}
    }

    fn delete(&mut self, _key: &[u8]) -> SqlResult<bool> {
        match *self {}
    }

    fn commit(self) -> SqlResult<()> {
        match self {}
    }

    fn rollback(self) -> SqlResult<()> {
        match self {}
    }
}

impl KeyValueStore for NoDatabase {
    type Read<'s> = NoAccess;
    type Write<'s> = NoAccess;

    fn begin_read(&self) -> SqlResult<NoAccess> {
        Err(no_database())
    }

    fn begin_write(&self) -> SqlResult<NoAccess> {
        Err(no_database())
    }
}

fn no_database() -> SqlError {
    SqlError::store(
        StoreFailure::Other,
        "a statement is only parsed here, with no database to read",
    )
}
