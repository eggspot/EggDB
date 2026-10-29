use crate::ast::{AlterAction, Expr, ExprKind, Name, Statement, TableReference};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};
use crate::session::{OneStatementError, Session};
use crate::store::StoreFailure;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};

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
            Some(part) => part.objects.push(object),
            None => self.parts.push(DestructivePart {
                kind,
                objects: vec![object],
            }),
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
    /// `ALTER TABLE ... DROP COLUMN`.
    DropColumn,
    /// `ALTER TABLE ... DROP CONSTRAINT`.
    DropConstraint,
    /// MySQL's `ALTER TABLE ... DROP PRIMARY KEY`.
    DropPrimaryKey,
    /// `ALTER TABLE ... ALTER COLUMN ... TYPE`, or MySQL's `MODIFY` and `CHANGE`.
    ChangeColumnType,
    /// `TRUNCATE`.
    Truncate,
    /// A `DELETE` whose `WHERE` names no column of its table, or that has none.
    DeleteWithoutFilter,
    /// An `UPDATE` whose `WHERE` names no column of its table, or that has none.
    UpdateWithoutFilter,
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
            Self::DropColumn => "ALTER TABLE ... DROP COLUMN",
            Self::DropConstraint => "ALTER TABLE ... DROP of a constraint, key or index",
            Self::DropPrimaryKey => "ALTER TABLE ... DROP PRIMARY KEY",
            Self::ChangeColumnType => "a change of a column's type",
            Self::Truncate => "TRUNCATE",
            Self::DeleteWithoutFilter => "DELETE of the whole table",
            Self::UpdateWithoutFilter => "UPDATE of the whole table",
        }
    }
}

/// Parses `text` as [`Session::run_one_statement`] does, in `dialect`, without a database,
/// and says whether its one statement is destructive. It refuses what `run_one_statement`
/// refuses (no statement, a second one, transaction control, `COPY`) and fails on a parse
/// error, both before anything could run.
pub fn destructive_statement(
    text: &str,
    dialect: Dialect,
) -> Result<Option<DestructiveStatement>, OneStatementError> {
    Session::with_dialect(&NoDatabase, dialect).destructive_statement(text)
}

pub(crate) fn classify(statement: &Statement) -> Option<DestructiveStatement> {
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
        Statement::Truncate(table) => Some(DestructiveStatement::new(
            DestructiveKind::Truncate,
            vec![shown(&table.name)],
            false,
        )),
        Statement::Delete(delete) => {
            (!filters_rows(delete.filter.as_ref(), &delete.table)).then(|| {
                DestructiveStatement::new(
                    DestructiveKind::DeleteWithoutFilter,
                    vec![shown(&delete.table.name)],
                    false,
                )
            })
        }
        Statement::Update(update) => {
            (!filters_rows(update.filter.as_ref(), &update.table)).then(|| {
                DestructiveStatement::new(
                    DestructiveKind::UpdateWithoutFilter,
                    vec![shown(&update.table.name)],
                    false,
                )
            })
        }
        Statement::AlterTable(alter) => altered(&alter.name, &alter.actions),
        Statement::CreateTable(_)
        | Statement::Insert(_)
        | Statement::Select(_)
        | Statement::Query(_)
        | Statement::Begin { .. }
        | Statement::Commit
        | Statement::Rollback
        | Statement::Set { .. }
        | Statement::Show { .. }
        | Statement::Reset { .. }
        | Statement::MySql(_)
        | Statement::Pragma(_)
        | Statement::CreateIndex(_)
        | Statement::Explain(_)
        | Statement::CreateSequence(_)
        | Statement::AlterSequence(_)
        | Statement::CreateSchema { .. }
        | Statement::RenameSchema { .. }
        | Statement::CreateView(_)
        | Statement::AlterView(_)
        | Statement::Comment(_)
        | Statement::Copy(_)
        | Statement::Ignored { .. }
        | Statement::AlterSession(_)
        | Statement::SqlServer(_) => None,
        Statement::Merge(merge) => merge
            .update
            .as_ref()
            .filter(|_| !filters_rows(Some(&merge.on), &merge.target))
            .map(|update| {
                let kind = if update.delete_condition.is_some() {
                    DestructiveKind::DeleteWithoutFilter
                } else {
                    DestructiveKind::UpdateWithoutFilter
                };
                DestructiveStatement::new(kind, vec![shown(&merge.target.name)], false)
            }),
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
    name.text.replace(crate::schemas::QUALIFIER, ".")
}

fn filters_rows(filter: Option<&Expr>, table: &TableReference) -> bool {
    let base = crate::schemas::base_name(&table.name.text);
    let names_target = |qualifier: Option<&str>| match qualifier {
        None => true,
        Some(qualifier) => {
            table
                .alias
                .as_deref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
                || qualifier.eq_ignore_ascii_case(base)
                || qualifier.eq_ignore_ascii_case(&shown(&table.name))
        }
    };
    filter.is_some_and(|filter| names_column(filter, &names_target))
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
