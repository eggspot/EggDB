use crate::ast::{
    AlterAction, BinaryOperator, Delete, Expr, ExprKind, Insert, Literal, Name, OnDuplicate,
    ReferentialAction, Statement, TableReference, UnaryOperator, Update,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};
use crate::execute::TableCache;
use crate::session::{OneStatementError, Session};
use crate::store::StoreFailure;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};
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
    /// `CALL` of a procedure or a `DO` block, whose code may change or drop anything.
    RunCode,
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
            Self::ChangeColumnType => "a change of a column's type",
            Self::Truncate => "TRUNCATE",
            Self::DeleteWithoutFilter => "DELETE of the whole table",
            Self::UpdateWithoutFilter => "UPDATE of the whole table",
            Self::DeleteOfManyRows => "DELETE that can remove many rows",
            Self::UpdateOfManyRows => "UPDATE that can change many rows",
            Self::InsertOverwritingRows => {
                "INSERT that overwrites existing rows (REPLACE or an upsert)"
            }
            Self::RunCode => "CALL or DO, which runs code that may change or drop data",
        }
    }
}

/// Parses `text` as [`Session::run_one_statement`] does, in `dialect`, without a database,
/// and says whether its one statement is destructive. It refuses what `run_one_statement`
/// refuses (no statement, a second one, transaction control, `COPY`) and fails on a parse
/// error, both before anything could run. With no database it knows no table's keys, so every
/// `DELETE` and `UPDATE` is destructive here; [`Session::destructive_statement`] on a session
/// over the database reads them.
pub fn destructive_statement(
    text: &str,
    dialect: Dialect,
) -> Result<Option<DestructiveStatement>, OneStatementError> {
    Session::with_dialect(&NoDatabase, dialect).destructive_statement(text)
}

pub(crate) fn classify(
    statement: &Statement,
    keys: &mut KeyLookup<'_>,
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
        Statement::Insert(insert) => inserted(insert),
        Statement::AlterTable(alter) => altered(&alter.name, &alter.actions),
        Statement::MaterializedView(crate::ast::MaterializedViewCommand::Drop {
            names, ..
        }) => dropped(DestructiveKind::DropView, names, false),
        Statement::MaterializedView(_) | Statement::InstallExtension { .. } => None,
        Statement::Database(crate::served_databases::DatabaseStatement::Drop { names, .. }) => {
            dropped(DestructiveKind::DropDatabase, names, false)
        }
        Statement::Database(crate::served_databases::DatabaseStatement::Create { .. }) => None,
        Statement::CreateTable(_)
        | Statement::Select(_)
        | Statement::Query(_)
        | Statement::Begin { .. }
        | Statement::Commit
        | Statement::Rollback
        | Statement::Savepoint(_)
        | Statement::Analyze(_)
        | Statement::Role(_)
        | Statement::RowSecurity(_)
        | Statement::Listen(_)
        | Statement::Unlisten(_)
        | Statement::Notify { .. }
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
        | Statement::Discard(_)
        | Statement::Cursor(_)
        | Statement::CreateType(_)
        | Statement::CreateDomain(_)
        | Statement::DropType { cascade: false, .. }
        | Statement::AlterType(_)
        | Statement::AlterDomain(_)
        | Statement::SetConstraints { .. }
        | Statement::CreateTrigger(_)
        | Statement::DropTrigger(_)
        | Statement::EnableTrigger(_)
        | Statement::AlterTrigger(_)
        | Statement::CreateRoutine(_)
        | Statement::DropRoutine(_)
        | Statement::AlterRoutine(_)
        | Statement::CreatePackage(_)
        | Statement::DropPackage(_)
        | Statement::AlterPackage(_)
        | Statement::SqlServer(_) => None,
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
        Statement::PlsqlBlock(_) => Some(DestructiveStatement::new(
            DestructiveKind::RunCode,
            vec![crate::plsql::ANONYMOUS_BLOCK_NAME.to_owned()],
            false,
        )),
        Statement::With(with) => crate::with_statement::classify(with, keys),
        Statement::Merge(merge) => merge.update.as_ref().map(|update| {
            let kind = if update.delete_condition.is_some() {
                DestructiveKind::DeleteOfManyRows
            } else {
                DestructiveKind::UpdateOfManyRows
            };
            DestructiveStatement::new(kind, vec![shown(&merge.target.name)], false)
        }),
    }
}

fn deleted(delete: &Delete, keys: &mut KeyLookup<'_>) -> Option<DestructiveStatement> {
    let kind = match row_reach(delete.filter.as_ref(), &delete.table, keys) {
        RowReach::WholeTable => DestructiveKind::DeleteWithoutFilter,
        RowReach::Pinned(table) if !table.delete_reaches_further => return None,
        RowReach::Pinned(_) | RowReach::Many => DestructiveKind::DeleteOfManyRows,
    };
    Some(DestructiveStatement::new(
        kind,
        vec![shown(&delete.table.name)],
        false,
    ))
}

fn updated(update: &Update, keys: &mut KeyLookup<'_>) -> Option<DestructiveStatement> {
    let kind = match row_reach(update.filter.as_ref(), &update.table, keys) {
        RowReach::WholeTable => DestructiveKind::UpdateWithoutFilter,
        RowReach::Pinned(table) if !table.update_reaches_further_with(update) => return None,
        RowReach::Pinned(_) | RowReach::Many => DestructiveKind::UpdateOfManyRows,
    };
    Some(DestructiveStatement::new(
        kind,
        vec![shown(&update.table.name)],
        false,
    ))
}

fn inserted(insert: &Insert) -> Option<DestructiveStatement> {
    match insert.on_duplicate {
        OnDuplicate::Replace | OnDuplicate::Update(_) => Some(DestructiveStatement::new(
            DestructiveKind::InsertOverwritingRows,
            vec![shown(&insert.table.name)],
            false,
        )),
        OnDuplicate::Fail | OnDuplicate::Ignore => None,
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
            | AlterAction::ValidateConstraint(_)
            | AlterAction::RenameConstraint { .. }
            | AlterAction::AlterIdentity { .. }
            | AlterAction::DropIdentity { .. }
            | AlterAction::AttachPartition { .. }
            | AlterAction::DetachPartition(_)
            | AlterAction::MoveColumn { .. }
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

fn row_reach(filter: Option<&Expr>, table: &TableReference, keys: &mut KeyLookup<'_>) -> RowReach {
    let names_target = |qualifier: Option<&str>| {
        qualifier.is_none_or(|qualifier| qualifies_target(qualifier, table))
    };
    let Some(filter) = filter.filter(|filter| names_column(filter, &names_target)) else {
        return RowReach::WholeTable;
    };
    match keys(&table.name.text) {
        Some(found) if found.pins_rows(filter, table) => RowReach::Pinned(found),
        _ => RowReach::Many,
    }
}

const MOST_PINNED_ROWS: usize = 100;

/// Finds what the classifier needs to know of a table by its internal name, or `None` when it
/// cannot tell (no database, no such table, a view, or a read that failed).
pub(crate) type KeyLookup<'a> = dyn FnMut(&str) -> Option<TableKeys> + 'a;

/// A table's keys and what else a change of its rows reaches, as the classifier needs them to
/// tell whether a `DELETE` or `UPDATE` changes only a few rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableKeys {
    columns: Vec<KeyedColumn>,
    keys: Vec<Vec<usize>>,
    delete_reaches_further: bool,
    update_reaches_further: bool,
    columns_cascading_on_update: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyedColumn {
    name: String,
    character: bool,
}

/// Reads the keys of table `name`: its primary key and its unique indexes, whether a `DELETE`
/// or `UPDATE` of its rows fires a trigger, and which foreign keys of other tables change their
/// rows when its rows are deleted or their referenced columns updated. `None` when `name` is
/// not a table.
pub(crate) fn table_keys(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<Option<TableKeys>> {
    let Some(table) = tables.table(reader, name)? else {
        return Ok(None);
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
        delete_reaches_further: crate::triggers::fire_on(
            reader,
            tables,
            &table.name,
            crate::triggers::TriggerEvent::Delete,
        )?,
        update_reaches_further: crate::triggers::fire_on(
            reader,
            tables,
            &table.name,
            crate::triggers::TriggerEvent::Update,
        )?,
        columns_cascading_on_update: Vec::new(),
    };
    for child_name in crate::constraints::referencing_tables(reader, &table.name)? {
        let Some(child) = tables.table(reader, &child_name)? else {
            found.delete_reaches_further = true;
            found.update_reaches_further = true;
            continue;
        };
        for key in child
            .foreign_keys
            .iter()
            .filter(|key| key.parent == table.name)
        {
            found.delete_reaches_further |= changes_child_rows(key.on_delete);
            if !changes_child_rows(key.on_update) {
                continue;
            }
            for column in &key.parent_columns {
                match table.column_index(column) {
                    Some(index) => found.columns_cascading_on_update.push(index),
                    None => found.update_reaches_further = true,
                }
            }
        }
    }
    Ok(Some(found))
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
