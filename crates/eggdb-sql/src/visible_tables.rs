use crate::ast::{Expr, ExprKind};
use crate::dialect::Dialect;
use crate::encoding::{
    Column, CommentKind, ForeignKey, IndexDefinition, KeyOwner, NAMESPACE, SequenceDefinition,
    TableDefinition, ViewColumn, ViewDefinition, decode_row, encode_row, every_dependency_prefix,
    every_reference_prefix, is_temporary_key, is_view_record, key_owner, meta_key, prefix_end,
    rows_prefix, sequence_key, sequences_prefix, split_comment_key, table_indexes_prefix,
    table_key, trigger_key, triggers_prefix,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::schemas::{DEFAULT_SCHEMA, QUALIFIER};
use crate::sqlserver_catalog::DBO;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;
use std::collections::{HashMap, HashSet};

/// The tables and views a session may see, such as those `eggdb mcp --tables` names (D156).
///
/// A [`VisibleRead`] made from it hides every other table, view and sequence from the SQL
/// engine above it: their definitions, rows, indexes, comments, foreign-key and dependency
/// records read as absent, so every dialect, every catalog (`pg_catalog`,
/// `information_schema`, SQL Server's `sys` views, Oracle's dictionary) and every name
/// resolution, a view's query included, finds them missing. A sequence stays visible when it
/// is named or when a visible table owns it. A table or view named with a column list, such as
/// `orders(id,total)`, shows only those columns (D281, D306).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleTables {
    internal: Vec<String>,
    columns: Vec<Option<Vec<String>>>,
}

/// A name of a [`VisibleTables`] that no table, view or sequence of the database has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTableName {
    /// The name as [`VisibleTables::names`] shows it, such as `archive.orders`.
    pub name: String,
    /// The stored name it matches when letter case is ignored, shown the same way.
    pub stored_as: Option<String>,
}

/// A column list of a [`VisibleTables`] that cannot be served as given, so the table it
/// names stays hidden (D281).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnservableColumnList {
    /// The table or view, shown as [`VisibleTables::names`] shows names, without its columns.
    pub table: String,
    /// Why, in words, such as `has no column "totl"`.
    pub reason: String,
}

impl VisibleTables {
    /// The tables and views with these names, each `name` or `schema.name`, split at the first
    /// dot, and optionally followed by the columns to show in parentheses, as in
    /// `orders(id,total)`; `public.` and `dbo.` name the default schema. Names match exactly as
    /// stored, and a table given twice counts once, in the order and with the columns first
    /// given.
    pub fn new<'n>(names: impl IntoIterator<Item = &'n str>) -> Self {
        let mut internal: Vec<String> = Vec::new();
        let mut columns = Vec::new();
        for entry in names {
            let (name, listed_columns) = split_column_list(entry);
            let stored = internal_name(name);
            if !internal.contains(&stored) {
                internal.push(stored);
                columns.push(listed_columns);
            }
        }
        Self { internal, columns }
    }

    /// The names, in order: the default schema's bare, others as `schema.name`, each followed
    /// by its column list when it has one, as in `orders(id,total)`.
    pub fn names(&self) -> Vec<String> {
        self.internal
            .iter()
            .zip(&self.columns)
            .map(|(name, columns)| match columns {
                Some(columns) => format!("{}({})", shown_name(name), columns.join(",")),
                None => shown_name(name),
            })
            .collect()
    }

    /// Whether some name has a column list.
    pub fn limits_columns(&self) -> bool {
        self.columns.iter().any(Option::is_some)
    }

    /// The column lists that cannot be served as `reader` sees the database: a column the
    /// table or view does not have, a primary-key column of a table left out, no column at
    /// all, a SQL Server view whose query starts with `WITH`, or a relation that does not exist
    /// yet (only a table listed without columns may be created by a confined writer).
    pub fn unservable_column_lists(
        &self,
        reader: &dyn KeyValueRead,
    ) -> SqlResult<Vec<UnservableColumnList>> {
        let mut unservable = Vec::new();
        for (name, columns) in self.internal.iter().zip(&self.columns) {
            let Some(columns) = columns else {
                continue;
            };
            let table = shown_name(name);
            let Some(stored) = reader.get(&table_key(name))? else {
                unservable.push(UnservableColumnList {
                    table,
                    reason: "does not exist yet: a table listed with columns must exist when the session starts".to_owned(),
                });
                continue;
            };
            let mut reasons = Vec::new();
            if is_view_record(&stored) {
                reasons.extend(unservable_view_columns(name, &stored, columns));
            } else if columns.is_empty() {
                reasons.push("has an empty column list".to_owned());
            } else {
                let definition = TableDefinition::decode(name, &stored)?;
                for column in columns {
                    if !definition
                        .columns
                        .iter()
                        .any(|stored| &stored.name == column)
                    {
                        reasons.push(format!("has no column \"{column}\""));
                    }
                }
                for &at in &definition.primary_key {
                    if let Some(key) = definition.columns.get(at)
                        && !columns.contains(&key.name)
                    {
                        reasons.push(format!(
                            "leaves out its primary key column \"{}\": list it too",
                            key.name
                        ));
                    }
                }
            }
            unservable.extend(reasons.into_iter().map(|reason| UnservableColumnList {
                table: table.clone(),
                reason,
            }));
        }
        Ok(unservable)
    }

    /// The names no table, view or sequence that `reader` sees has.
    pub fn unknown_names(&self, reader: &dyn KeyValueRead) -> SqlResult<Vec<UnknownTableName>> {
        let mut unknown = Vec::new();
        let mut stored: Option<Vec<String>> = None;
        for name in &self.internal {
            if reader.get(&table_key(name))?.is_some() || reader.get(&sequence_key(name))?.is_some()
            {
                continue;
            }
            if stored.is_none() {
                stored = Some(relation_names(reader)?);
            }
            let stored_as = stored
                .iter()
                .flatten()
                .find(|candidate| candidate.to_lowercase() == name.to_lowercase())
                .map(|candidate| shown_name(candidate));
            unknown.push(UnknownTableName {
                name: shown_name(name),
                stored_as,
            });
        }
        Ok(unknown)
    }

    /// `inner` with every table, view and sequence but these hidden, as of the snapshot
    /// `inner` reads.
    pub fn read<R: KeyValueRead>(&self, inner: R) -> SqlResult<VisibleRead<R>> {
        let shown = Shown::of(self, &inner, false)?;
        Ok(VisibleRead {
            inner,
            shown: Some(shown),
        })
    }

    /// `inner`, a write transaction, confined to these tables and views
    /// (D257): its reads hide everything else as [`VisibleTables::read`]
    /// does, and a change to anything else is refused with `42501`. That covers another table's
    /// definition, rows and indexes, a sequence that is neither named nor owned by a listed
    /// table, a schema, type, routine, package or role, and keys outside SQL; a new table, view
    /// or sequence may be created only under a listed name, or as a sequence a listed table
    /// owns. A listed table that an [`OutsideLink`] ties to another relation cannot be changed
    /// where the change could reach that relation: its definition when it has a foreign key to
    /// it, whose new and changed keys the writer checks against the other table itself (D306);
    /// its definition, row deletions and changes to the columns the foreign key points at when
    /// the other relation has a foreign key to it (inserts and changes to other columns go
    /// through, D281); its definition when a view reads it. A table listed with columns takes
    /// inserts, which give the other columns their defaults, and changes to the listed columns,
    /// which keep the others' values, with the indexes and checks over the other columns kept;
    /// deleting its rows, changing its definition, and a write whose outcome would depend on a
    /// value the session cannot see are refused (D306). A default, check, generated expression
    /// or row-security policy that calls a user-defined routine or holds a subquery cannot be
    /// written. A transaction with a refused change rolls back at commit even when the refusal
    /// was caught along the way.
    pub fn write<W: KeyValueWrite>(&self, inner: W) -> SqlResult<VisibleWrite<W>> {
        let mut shown = Shown::of(self, &inner, true)?;
        let links = links_of(&shown, &inner)?;
        let mut rows_blocked = HashSet::new();
        let mut keys_guarded: HashMap<u32, KeyGuard> = HashMap::new();
        let mut parents_outside: HashMap<u32, ParentsOutside> = HashMap::new();
        for (&id, name) in &shown.table_names {
            if !shown.tables.contains(&id) {
                continue;
            }
            for (at, link) in links.iter().enumerate() {
                if &link.listed != name {
                    continue;
                }
                match link.kind {
                    OutsideLinkKind::References => {
                        match parents_outside_of(&inner, name, &link.outside)? {
                            Some((child, keys)) => parents_outside
                                .entry(id)
                                .or_insert_with(|| ParentsOutside {
                                    child,
                                    keys: Vec::new(),
                                })
                                .keys
                                .extend(keys),
                            None => {
                                rows_blocked.insert(id);
                            }
                        }
                    }
                    OutsideLinkKind::ReferencedBy => {
                        match referenced_positions(&inner, name, &link.outside)? {
                            Some((columns, positions)) => {
                                let guard = keys_guarded.entry(id).or_insert_with(|| KeyGuard {
                                    columns,
                                    positions: Vec::new(),
                                    link: at,
                                });
                                for position in positions {
                                    if !guard.positions.contains(&position) {
                                        guard.positions.push(position);
                                    }
                                }
                            }
                            None => {
                                rows_blocked.insert(id);
                            }
                        }
                    }
                    OutsideLinkKind::ReadByView | OutsideLinkKind::Reads => {}
                }
            }
        }
        for id in &rows_blocked {
            keys_guarded.remove(id);
            parents_outside.remove(id);
        }
        let mut narrowed_writes = HashMap::new();
        for (&id, narrowed) in &shown.narrowed {
            narrowed_writes.insert(id, NarrowedWrites::of(&inner, &shown, narrowed)?);
        }
        for &id in parents_outside.keys() {
            shown.leave_outside_foreign_keys_to_the_writer(&inner, id)?;
        }
        Ok(VisibleWrite {
            inner,
            confined: Some(Confined {
                listed: self
                    .internal
                    .iter()
                    .zip(&self.columns)
                    .filter(|(_, columns)| columns.is_none())
                    .map(|(name, _)| name.clone())
                    .collect(),
                shown,
                links,
                rows_blocked,
                keys_guarded,
                parents_outside,
                narrowed_writes,
                inserted_here: HashSet::new(),
                refused: false,
                dropped_trigger_records: Vec::new(),
                dropped_definitions: Vec::new(),
            }),
        })
    }

    /// The ties between these tables and views and the relations outside them, as `reader`
    /// sees them: foreign keys either way and views reading across. A writing session confined
    /// by [`VisibleTables::write`] cannot change what they tie, so `eggdb mcp` names them at
    /// start.
    pub fn outside_links(&self, reader: &dyn KeyValueRead) -> SqlResult<Vec<OutsideLink>> {
        let shown = Shown::of(self, reader, false)?;
        Ok(links_of(&shown, reader)?
            .into_iter()
            .map(|link| link.shown())
            .collect())
    }
}

/// How an [`OutsideLink`] ties a listed relation to one outside the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsideLinkKind {
    /// The outside table has a foreign key to the listed table.
    ReferencedBy,
    /// The listed table has a foreign key to the outside table.
    References,
    /// The outside view reads the listed table or view.
    ReadByView,
    /// The listed view reads the outside table, view or sequence.
    Reads,
}

impl OutsideLinkKind {
    fn blocks_rows(self) -> bool {
        matches!(self, Self::ReferencedBy | Self::References)
    }
}

/// A listed table or view tied to a relation outside a [`VisibleTables`], so that a change to
/// the listed one could reach the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideLink {
    /// The listed relation, shown as [`VisibleTables::names`] shows names.
    pub listed: String,
    /// The relation outside the list, shown the same way.
    pub outside: String,
    /// How they are tied.
    pub kind: OutsideLinkKind,
}

impl OutsideLink {
    /// What a confined writer may not do to the listed relation because of this tie, in
    /// words, such as `change its definition`.
    pub fn blocked_changes(&self) -> &'static str {
        match self.kind {
            OutsideLinkKind::References => "change its definition",
            OutsideLinkKind::ReferencedBy => {
                "delete its rows, change the columns that foreign key points at, or change its definition"
            }
            OutsideLinkKind::ReadByView | OutsideLinkKind::Reads => "change its definition",
        }
    }

    /// The tie in words, such as `table "order_items" has a foreign key to "orders"`.
    pub fn describe(&self) -> String {
        let (listed, outside) = (&self.listed, &self.outside);
        match self.kind {
            OutsideLinkKind::ReferencedBy => {
                format!("table \"{outside}\" has a foreign key to \"{listed}\"")
            }
            OutsideLinkKind::References => {
                format!("\"{listed}\" has a foreign key to table \"{outside}\"")
            }
            OutsideLinkKind::ReadByView => format!("view \"{outside}\" reads \"{listed}\""),
            OutsideLinkKind::Reads => format!("view \"{listed}\" reads \"{outside}\""),
        }
    }
}

#[derive(Debug)]
struct Link {
    listed: Vec<u8>,
    outside: Vec<u8>,
    kind: OutsideLinkKind,
}

impl Link {
    fn shown(&self) -> OutsideLink {
        OutsideLink {
            listed: shown_name(&String::from_utf8_lossy(&self.listed)),
            outside: shown_name(&String::from_utf8_lossy(&self.outside)),
            kind: self.kind,
        }
    }
}

fn links_of(shown: &Shown, reader: &dyn KeyValueRead) -> SqlResult<Vec<Link>> {
    let mut links = Vec::new();
    for (prefix, toward_listed, from_listed) in [
        (
            every_reference_prefix(),
            OutsideLinkKind::ReferencedBy,
            OutsideLinkKind::References,
        ),
        (
            every_dependency_prefix(),
            OutsideLinkKind::ReadByView,
            OutsideLinkKind::Reads,
        ),
    ] {
        reader.scan(
            &prefix,
            &prefix_end(&prefix),
            ScanDirection::Forward,
            &mut |key, _| {
                let KeyOwner::Relations(target, source) = key_owner(key) else {
                    return Ok(true);
                };
                let hidden_source = shown.hidden.contains(source)
                    || (toward_listed == OutsideLinkKind::ReferencedBy
                        && source != target
                        && shown.references_through_hidden_columns(source, target));
                if shown.relations.contains(target) && hidden_source {
                    links.push(Link {
                        listed: target.to_vec(),
                        outside: source.to_vec(),
                        kind: toward_listed,
                    });
                } else if shown.relations.contains(source) && shown.hidden.contains(target) {
                    links.push(Link {
                        listed: source.to_vec(),
                        outside: target.to_vec(),
                        kind: from_listed,
                    });
                }
                Ok(true)
            },
        )?;
    }
    Ok(links)
}

fn internal_name(name: &str) -> String {
    match name.split_once('.') {
        Some((schema, relation)) if schema == DEFAULT_SCHEMA || schema == DBO => {
            relation.to_owned()
        }
        Some((schema, relation)) => format!("{schema}{QUALIFIER}{relation}"),
        None => name.to_owned(),
    }
}

fn shown_name(internal: &str) -> String {
    match internal.split_once(QUALIFIER) {
        Some((schema, relation)) => format!("{schema}.{relation}"),
        None => internal.to_owned(),
    }
}

fn names_under(reader: &dyn KeyValueRead, prefix: &[u8]) -> SqlResult<Vec<String>> {
    let mut names = Vec::new();
    reader.scan(
        prefix,
        &prefix_end(prefix),
        ScanDirection::Forward,
        &mut |key, _| {
            if let Some(name) = key.get(prefix.len()..) {
                names.push(String::from_utf8_lossy(name).into_owned());
            }
            Ok(true)
        },
    )?;
    Ok(names)
}

fn relation_names(reader: &dyn KeyValueRead) -> SqlResult<Vec<String>> {
    let mut names = names_under(reader, &table_key(""))?;
    names.extend(names_under(reader, &sequences_prefix())?);
    Ok(names)
}

fn split_column_list(entry: &str) -> (&str, Option<Vec<String>>) {
    let Some((name, rest)) = entry.split_once('(') else {
        return (entry, None);
    };
    let Some(inside) = rest.strip_suffix(')') else {
        return (entry, None);
    };
    let mut columns: Vec<String> = Vec::new();
    for column in inside.split(',').map(str::trim) {
        if !column.is_empty() && !columns.iter().any(|kept| kept == column) {
            columns.push(column.to_owned());
        }
    }
    (name.trim_end(), Some(columns))
}

fn referenced_positions(
    reader: &dyn KeyValueRead,
    parent: &[u8],
    child: &[u8],
) -> SqlResult<Option<(Vec<Column>, Vec<usize>)>> {
    let (Ok(parent_name), Ok(child_name)) =
        (std::str::from_utf8(parent), std::str::from_utf8(child))
    else {
        return Ok(None);
    };
    let (Some(parent_stored), Some(child_stored)) = (
        reader.get(&table_key(parent_name))?,
        reader.get(&table_key(child_name))?,
    ) else {
        return Ok(None);
    };
    if is_view_record(&parent_stored) || is_view_record(&child_stored) {
        return Ok(None);
    }
    let (Ok(parent_table), Ok(child_table)) = (
        TableDefinition::decode(parent_name, &parent_stored),
        TableDefinition::decode(child_name, &child_stored),
    ) else {
        return Ok(None);
    };
    let mut positions = Vec::new();
    let mut references = child_table
        .foreign_keys
        .iter()
        .filter(|key| key.parent == parent_name)
        .peekable();
    if references.peek().is_none() {
        return Ok(None);
    }
    for key in references {
        if key.parent_columns.is_empty() {
            positions.extend(parent_table.primary_key.iter().copied());
        }
        for column in &key.parent_columns {
            match parent_table.column_index(column) {
                Some(at) => positions.push(at),
                None => return Ok(None),
            }
        }
    }
    if positions.is_empty() {
        return Ok(None);
    }
    Ok(Some((parent_table.columns, positions)))
}

fn parents_outside_of(
    reader: &dyn KeyValueRead,
    child: &[u8],
    parent: &[u8],
) -> SqlResult<Option<(TableDefinition, Vec<(ForeignKey, TableDefinition)>)>> {
    let (Ok(child_name), Ok(parent_name)) =
        (std::str::from_utf8(child), std::str::from_utf8(parent))
    else {
        return Ok(None);
    };
    let (Some(child_stored), Some(parent_stored)) = (
        reader.get(&table_key(child_name))?,
        reader.get(&table_key(parent_name))?,
    ) else {
        return Ok(None);
    };
    if is_view_record(&child_stored) || is_view_record(&parent_stored) {
        return Ok(None);
    }
    let (Ok(child_table), Ok(parent_table)) = (
        TableDefinition::decode(child_name, &child_stored),
        TableDefinition::decode(parent_name, &parent_stored),
    ) else {
        return Ok(None);
    };
    let keys: Vec<(ForeignKey, TableDefinition)> = child_table
        .foreign_keys
        .iter()
        .filter(|key| key.parent == parent_name)
        .map(|key| (key.clone(), parent_table.clone()))
        .collect();
    if keys.is_empty() {
        return Ok(None);
    }
    Ok(Some((child_table, keys)))
}

#[derive(Debug)]
struct ParentsOutside {
    child: TableDefinition,
    keys: Vec<(ForeignKey, TableDefinition)>,
}

const SHOWN_COLUMNS_LABEL: &str = "eggdb_shown_columns";

#[derive(Debug)]
struct NarrowedView {
    kept_names: Vec<String>,
    definition: Vec<u8>,
}

impl NarrowedView {
    fn of(name: &str, stored: &[u8], wanted: &[String]) -> Option<Self> {
        let view = ViewDefinition::decode(name, stored).ok()?;
        let kept: Vec<ViewColumn> = view
            .columns
            .iter()
            .filter(|column| wanted.contains(&column.name))
            .cloned()
            .collect();
        if kept.is_empty() || kept.len() != wanted.len() {
            return None;
        }
        let query = shown_columns_query(&view, &kept)?;
        let kept_names = kept.iter().map(|column| column.name.clone()).collect();
        let definition = ViewDefinition {
            query,
            columns: kept,
            expands_wildcard: false,
            ..view
        }
        .encode();
        Some(Self {
            kept_names,
            definition,
        })
    }
}

fn quoted_in(dialect: Dialect, name: &str) -> String {
    match dialect {
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
        Dialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
        Dialect::PostgreSql | Dialect::Sqlite | Dialect::Oracle => {
            format!("\"{}\"", name.replace('"', "\"\""))
        }
    }
}

fn starts_with_with(query: &str) -> bool {
    let trimmed = query.trim_start_matches(|character: char| character.is_whitespace() || character == '(');
    trimmed
        .get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("with"))
        && trimmed[4..]
            .chars()
            .next()
            .is_none_or(|next| !next.is_alphanumeric() && next != '_')
}

fn shown_columns_query(view: &ViewDefinition, kept: &[ViewColumn]) -> Option<String> {
    if view.dialect == Dialect::SqlServer && starts_with_with(&view.query) {
        return None;
    }
    let label = quoted_in(view.dialect, SHOWN_COLUMNS_LABEL);
    let every: Vec<String> = view
        .columns
        .iter()
        .map(|column| quoted_in(view.dialect, &column.name))
        .collect();
    let shown: Vec<String> = kept
        .iter()
        .map(|column| quoted_in(view.dialect, &column.name))
        .collect();
    Some(format!(
        "WITH {label} ({}) AS ({}) SELECT {} FROM {label}",
        every.join(", "),
        view.query,
        shown.join(", ")
    ))
}

fn unservable_view_columns(name: &str, stored: &[u8], columns: &[String]) -> Vec<String> {
    let Ok(view) = ViewDefinition::decode(name, stored) else {
        return vec!["is a view whose definition cannot be read".to_owned()];
    };
    if columns.is_empty() {
        return vec!["has an empty column list".to_owned()];
    }
    let reasons: Vec<String> = columns
        .iter()
        .filter(|column| !view.columns.iter().any(|stored| &stored.name == *column))
        .map(|column| format!("has no column \"{column}\""))
        .collect();
    if reasons.is_empty() && NarrowedView::of(name, stored, columns).is_none() {
        return vec!["is a SQL Server view whose query starts with WITH: list it without columns, or list the columns of the tables it reads".to_owned()];
    }
    reasons
}

const GENERATED_HIDDEN: &str = "a column this session cannot see takes generated values (an identity, a serial or a sequence default)";
const DOMAIN_HIDDEN: &str =
    "a column this session cannot see has a domain, whose checks would test its value";
const CODE_IN_HIDDEN_DEFAULT: &str = "the default of a column this session cannot see calls a routine, holds a subquery or cannot be read";
const UNIQUE_HIDDEN: &str =
    "a unique index over a column this session cannot see would be checked against other rows";
const FOREIGN_KEY_HIDDEN: &str =
    "a foreign key over a column this session cannot see would be checked against its table";
const PARTITIONED_HIDDEN: &str = "it is partitioned or a partition, and its bounds may use a column this session cannot see";
const REFERENCED_THROUGH_HIDDEN: &str =
    "a foreign key of another listed table points at a column of it this session cannot see";

#[derive(Debug)]
struct NarrowedWrites {
    shown_columns: Vec<Column>,
    hidden: Vec<usize>,
    insert_refusal: Option<&'static str>,
    update_refusal: Option<&'static str>,
    tied: Vec<usize>,
    maintained: Vec<IndexDefinition>,
}

impl NarrowedWrites {
    fn of(reader: &dyn KeyValueRead, shown: &Shown, narrowed: &Narrowed) -> SqlResult<Self> {
        let full = &narrowed.full;
        let hidden: Vec<usize> = (0..full.columns.len())
            .filter(|at| !narrowed.kept.contains(at))
            .collect();
        let kept_among = |columns: &[usize]| -> Vec<usize> {
            columns
                .iter()
                .copied()
                .filter(|at| narrowed.kept.contains(at))
                .collect()
        };
        let maintained: Vec<IndexDefinition> = full
            .indexes
            .iter()
            .filter(|index| narrowed.hidden_indexes.contains(&index.id))
            .cloned()
            .collect();
        let mut tied = Vec::new();
        for index in maintained.iter().filter(|index| index.unique) {
            tied.extend(kept_among(&index.columns));
        }
        for key in &full.foreign_keys {
            if key.columns.iter().any(|at| hidden.contains(at)) {
                tied.extend(kept_among(&key.columns));
            }
        }
        let shown_definition = TableDefinition::decode(&full.name, &narrowed.definition)?;
        for check in &full.checks {
            if shown_definition
                .checks
                .iter()
                .any(|kept| kept.name == check.name)
            {
                continue;
            }
            match crate::constraints::parse_check(check) {
                Ok(expr) => tied.extend(
                    narrowed
                        .kept
                        .iter()
                        .copied()
                        .filter(|&at| mentions_column(&expr, &full.columns[at].name)),
                ),
                Err(_) => tied.extend(narrowed.kept.iter().copied()),
            }
        }
        tied.sort_unstable();
        tied.dedup();
        let mut tables = crate::execute::TableCache::default();
        let partitioned = crate::partitions::constraint_of(reader, &mut tables, full)?.is_some()
            || crate::partitions::partitioned(reader, &mut tables, full)?.is_some();
        let (insert_refusal, update_refusal) = if partitioned {
            (Some(PARTITIONED_HIDDEN), Some(PARTITIONED_HIDDEN))
        } else {
            (
                hidden_insert_refusal(reader, full, &hidden)?,
                referenced_through_hidden_columns(reader, shown, full, &hidden)?
                    .then_some(REFERENCED_THROUGH_HIDDEN),
            )
        };
        Ok(Self {
            shown_columns: narrowed
                .kept
                .iter()
                .map(|&at| full.columns[at].clone())
                .collect(),
            hidden,
            insert_refusal,
            update_refusal,
            tied,
            maintained,
        })
    }
}

fn hidden_insert_refusal(
    reader: &dyn KeyValueRead,
    full: &TableDefinition,
    hidden: &[usize],
) -> SqlResult<Option<&'static str>> {
    let mut tables = crate::execute::TableCache::default();
    for &at in hidden {
        let column = &full.columns[at];
        if column.auto_increment
            || full.identities.iter().any(|identity| identity.column == at)
            || column
                .default
                .as_deref()
                .is_some_and(crate::sequences::mentions_sequence_call)
        {
            return Ok(Some(GENERATED_HIDDEN));
        }
        if full.domain_of(at).is_some() {
            return Ok(Some(DOMAIN_HIDDEN));
        }
        let expr = match crate::execute::default_expression(column, full.default_dialect(at)) {
            Ok(Some(expr)) => expr,
            Ok(None) => continue,
            Err(_) => return Ok(Some(CODE_IN_HIDDEN_DEFAULT)),
        };
        if has_subquery(&expr) {
            return Ok(Some(CODE_IN_HIDDEN_DEFAULT));
        }
        let mut called = Vec::new();
        called_names(&expr, &mut called);
        for function in called {
            if !matches!(
                crate::routines::resolve(reader, &mut tables, &function),
                Ok(None)
            ) {
                return Ok(Some(CODE_IN_HIDDEN_DEFAULT));
            }
        }
    }
    Ok(None)
}

fn referenced_through_hidden_columns(
    reader: &dyn KeyValueRead,
    shown: &Shown,
    full: &TableDefinition,
    hidden: &[usize],
) -> SqlResult<bool> {
    if !full.referenced {
        return Ok(false);
    }
    let hidden_names: Vec<&str> = hidden
        .iter()
        .map(|&at| full.columns[at].name.as_str())
        .collect();
    for child_name in crate::constraints::referencing_tables(reader, &full.name)? {
        if !shown.relations.contains(child_name.as_bytes()) {
            continue;
        }
        let child = if child_name == full.name {
            Some(full.clone())
        } else {
            match reader.get(&table_key(&child_name))? {
                Some(stored) if !is_view_record(&stored) => {
                    TableDefinition::decode(&child_name, &stored).ok()
                }
                _ => None,
            }
        };
        let Some(child) = child else {
            return Ok(true);
        };
        if child.foreign_keys.iter().any(|key| {
            key.parent == full.name
                && key
                    .parent_columns
                    .iter()
                    .any(|column| hidden_names.contains(&column.as_str()))
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug)]
struct NarrowedRow {
    old: Option<Vec<Value>>,
    new: Vec<Value>,
}

fn hidden_not_null(table: &str) -> SqlError {
    SqlError::new(
        SqlState::NOT_NULL_VIOLATION,
        format!(
            "null value in a column of relation \"{table}\" that this session cannot see violates not-null constraint"
        ),
    )
    .with_hint("Ask the user to give that column a default, or to list it too (--tables).")
}

fn hidden_check_failed(table: &str, error: &SqlError) -> SqlError {
    let state = if error.state() == SqlState::CHECK_VIOLATION {
        SqlState::CHECK_VIOLATION
    } else {
        error.state()
    };
    SqlError::new(
        state,
        format!(
            "new row for relation \"{table}\" violates a check over a column this session cannot see"
        ),
    )
}

fn hidden_default_failed(table: &str, error: &SqlError) -> SqlError {
    SqlError::new(
        error.state(),
        format!(
            "the default of a column of relation \"{table}\" that this session cannot see could not be computed"
        ),
    )
}

fn row_of_width(bytes: &[u8], columns: &[Column]) -> SqlResult<Vec<Value>> {
    let mut row = decode_row(bytes, columns)?;
    row.resize(columns.len(), Value::Null);
    Ok(row)
}

fn narrowed_update(
    narrowed: &Narrowed,
    writes: &NarrowedWrites,
    old: Vec<Value>,
    shown_values: &[Value],
) -> Result<NarrowedRow, Refusal> {
    let table = Confined::shown(&narrowed.name);
    if let Some(reason) = writes.update_refusal {
        return Err(Refusal::HiddenUpdate { table, reason });
    }
    let mut new = old.clone();
    for (from, &at) in narrowed.kept.iter().enumerate() {
        new[at] = shown_values.get(from).cloned().unwrap_or(Value::Null);
    }
    if let Some(&at) = writes
        .tied
        .iter()
        .find(|&&at| !crate::constraints::same_value(old.get(at), new.get(at)))
    {
        return Err(Refusal::TiedColumn {
            table,
            column: narrowed.full.columns[at].name.clone(),
        });
    }
    Ok(NarrowedRow {
        old: Some(old),
        new,
    })
}

fn narrowed_insert(
    narrowed: &Narrowed,
    writes: &NarrowedWrites,
    shown_values: &[Value],
) -> SqlResult<Result<NarrowedRow, Refusal>> {
    let table = Confined::shown(&narrowed.name);
    if let Some(reason) = writes.insert_refusal {
        return Ok(Err(Refusal::HiddenInsert { table, reason }));
    }
    let full = &narrowed.full;
    let mut new = vec![Value::Null; full.columns.len()];
    for (from, &at) in narrowed.kept.iter().enumerate() {
        new[at] = shown_values.get(from).cloned().unwrap_or(Value::Null);
    }
    for &at in &writes.hidden {
        new[at] = crate::execute::default_value(full, at)
            .map_err(|error| hidden_default_failed(&table, &error))?;
        if full.columns[at].not_null && new[at].is_null() {
            return Err(hidden_not_null(&table));
        }
    }
    let unchecked = |columns: &[usize]| {
        columns.iter().any(|at| writes.hidden.contains(at))
            && columns.iter().all(|&at| !new[at].is_null())
    };
    if full.foreign_keys.iter().any(|key| unchecked(&key.columns)) {
        return Ok(Err(Refusal::HiddenInsert {
            table,
            reason: FOREIGN_KEY_HIDDEN,
        }));
    }
    if writes
        .maintained
        .iter()
        .any(|index| index.unique && unchecked(&index.columns))
    {
        return Ok(Err(Refusal::HiddenInsert {
            table,
            reason: UNIQUE_HIDDEN,
        }));
    }
    crate::constraints::PreparedChecks::of(full)
        .and_then(|checks| checks.check(full, &new))
        .map_err(|error| hidden_check_failed(&table, &error))?;
    Ok(Ok(NarrowedRow { old: None, new }))
}

#[derive(Debug)]
struct Narrowed {
    name: Vec<u8>,
    parents_through_hidden_columns: Vec<Vec<u8>>,
    full: TableDefinition,
    kept: Vec<usize>,
    kept_names: Vec<String>,
    hidden_indexes: HashSet<u32>,
    definition: Vec<u8>,
}

impl Narrowed {
    fn of(table: &TableDefinition, wanted: &[String]) -> Option<Self> {
        let kept: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| wanted.contains(&column.name))
            .map(|(at, _)| at)
            .collect();
        if kept.is_empty() || table.primary_key.iter().any(|at| !kept.contains(at)) {
            return None;
        }
        let mut shown = table.clone();
        for at in (0..table.columns.len()).rev() {
            if !kept.contains(&at) {
                shown = without_column(&shown, at);
            }
        }
        let hidden_indexes = table
            .indexes
            .iter()
            .map(|index| index.id)
            .filter(|id| !shown.indexes.iter().any(|index| index.id == *id))
            .collect();
        let parents_through_hidden_columns = table
            .foreign_keys
            .iter()
            .filter(|key| key.columns.iter().any(|at| !kept.contains(at)))
            .map(|key| key.parent.as_bytes().to_vec())
            .collect();
        Some(Self {
            name: table.name.as_bytes().to_vec(),
            parents_through_hidden_columns,
            full: table.clone(),
            kept_names: kept
                .iter()
                .map(|&at| table.columns[at].name.clone())
                .collect(),
            kept,
            hidden_indexes,
            definition: shown.encode(),
        })
    }

    fn row(&self, stored: &[u8]) -> SqlResult<Vec<u8>> {
        let values = decode_row(stored, &self.full.columns)?;
        let shown: Vec<_> = self
            .kept
            .iter()
            .take_while(|&&at| at < values.len())
            .map(|&at| values[at].clone())
            .collect();
        Ok(encode_row(&shown))
    }

    fn hides_index_entry(&self, key: &[u8], id: u32) -> bool {
        let prefix = table_indexes_prefix(id);
        let Some(rest) = key.strip_prefix(prefix.as_slice()) else {
            return false;
        };
        rest.get(..4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .is_none_or(|bytes| self.hidden_indexes.contains(&u32::from_be_bytes(bytes)))
    }
}

fn without_column(table: &TableDefinition, removed: usize) -> TableDefinition {
    let mut shown = table.clone();
    let name = shown.columns.remove(removed).name;
    shown.primary_key = shifted(&table.primary_key, removed).unwrap_or_default();
    if shown.primary_key.is_empty() {
        shown.primary_key_name = None;
    }
    shown.identities = table.identities_without_column(removed);
    shown.default_dialects = table.default_dialects_without_column(removed);
    shown.domains = table.domains_without_column(removed);
    if shown.auto_increment_column().is_none() {
        shown.identity_seed = None;
    }
    shown.indexes = table
        .indexes
        .iter()
        .filter_map(|index| {
            shifted(&index.columns, removed).map(|columns| IndexDefinition {
                columns,
                ..index.clone()
            })
        })
        .collect();
    shown.checks.retain(|check| {
        crate::constraints::parse_check(check).is_ok_and(|expr| !mentions_column(&expr, &name))
    });
    shown.foreign_keys = table
        .foreign_keys
        .iter()
        .filter(|key| key.parent != table.name || !key.parent_columns.contains(&name))
        .filter_map(|key| {
            shifted(&key.columns, removed).map(|columns| ForeignKey {
                columns,
                ..key.clone()
            })
        })
        .collect();
    shown.forget_missing_statuses();
    shown
}

fn shifted(positions: &[usize], removed: usize) -> Option<Vec<usize>> {
    if positions.contains(&removed) {
        return None;
    }
    Some(
        positions
            .iter()
            .map(|&at| if at > removed { at - 1 } else { at })
            .collect(),
    )
}

fn mentions_column(expr: &Expr, column: &str) -> bool {
    if let ExprKind::Column { name, .. } = &expr.kind
        && name == column
    {
        return true;
    }
    crate::eval::children(expr)
        .into_iter()
        .any(|child| mentions_column(child, column))
}

#[derive(Debug)]
struct Shown {
    relations: HashSet<Vec<u8>>,
    hidden: HashSet<Vec<u8>>,
    tables: HashSet<u32>,
    table_names: HashMap<u32, Vec<u8>>,
    narrowed: HashMap<u32, Narrowed>,
    narrowed_names: HashMap<Vec<u8>, u32>,
    narrowed_views: HashMap<Vec<u8>, NarrowedView>,
    unchecked_definitions: HashMap<Vec<u8>, Vec<u8>>,
}

impl Shown {
    fn leave_outside_foreign_keys_to_the_writer(
        &mut self,
        reader: &dyn KeyValueRead,
        id: u32,
    ) -> SqlResult<()> {
        let Some(name) = self.table_names.get(&id).cloned() else {
            return Ok(());
        };
        let text = String::from_utf8_lossy(&name).into_owned();
        let shown = match self.narrowed.get(&id) {
            Some(narrowed) => narrowed.definition.clone(),
            None => match reader.get(&table_key(&text))? {
                Some(stored) => stored,
                None => return Ok(()),
            },
        };
        let mut definition = TableDefinition::decode(&text, &shown)?;
        let hidden = &self.hidden;
        definition
            .foreign_keys
            .retain(|key| !hidden.contains(key.parent.as_bytes()));
        definition.forget_missing_statuses();
        let encoded = definition.encode();
        match self.narrowed.get_mut(&id) {
            Some(narrowed) => narrowed.definition = encoded,
            None => {
                self.unchecked_definitions.insert(name, encoded);
            }
        }
        Ok(())
    }

    fn narrows_anything(&self) -> bool {
        !self.narrowed.is_empty()
            || !self.narrowed_views.is_empty()
            || !self.unchecked_definitions.is_empty()
    }

    fn is_narrowed(&self, name: &[u8]) -> bool {
        self.narrowed_names.contains_key(name) || self.narrowed_views.contains_key(name)
    }

    fn references_through_hidden_columns(&self, child: &[u8], parent: &[u8]) -> bool {
        self.narrowed_named(child).is_some_and(|narrowed| {
            narrowed
                .parents_through_hidden_columns
                .iter()
                .any(|listed| listed == parent)
        })
    }

    fn of(tables: &VisibleTables, reader: &dyn KeyValueRead, name_tables: bool) -> SqlResult<Self> {
        let mut shown = Self {
            relations: HashSet::new(),
            hidden: HashSet::new(),
            tables: HashSet::new(),
            table_names: HashMap::new(),
            narrowed: HashMap::new(),
            narrowed_names: HashMap::new(),
            narrowed_views: HashMap::new(),
            unchecked_definitions: HashMap::new(),
        };
        let tables_prefix = table_key("");
        let mut failure = None;
        reader.scan(
            &tables_prefix,
            &prefix_end(&tables_prefix),
            ScanDirection::Forward,
            &mut |key, value| {
                let Some(name) = key.get(tables_prefix.len()..) else {
                    return Ok(true);
                };
                let position = std::str::from_utf8(name)
                    .ok()
                    .and_then(|text| tables.internal.iter().position(|wanted| wanted == text));
                let column_list = position
                    .and_then(|at| tables.columns.get(at))
                    .and_then(Option::as_ref);
                let view = is_view_record(value);
                if position.is_none() {
                    shown.hidden.insert(name.to_vec());
                    if name_tables
                        && !view
                        && let Ok(table) =
                            TableDefinition::decode(&String::from_utf8_lossy(name), value)
                    {
                        shown.table_names.insert(table.id, name.to_vec());
                    }
                    return Ok(true);
                }
                if view {
                    if let Some(columns) = column_list {
                        match NarrowedView::of(&String::from_utf8_lossy(name), value, columns) {
                            Some(narrowed) => {
                                shown.narrowed_views.insert(name.to_vec(), narrowed);
                            }
                            None => {
                                shown.hidden.insert(name.to_vec());
                                return Ok(true);
                            }
                        }
                    }
                    shown.relations.insert(name.to_vec());
                    return Ok(true);
                }
                let text = String::from_utf8_lossy(name);
                let table = match TableDefinition::decode(&text, value) {
                    Ok(table) => table,
                    Err(error) => {
                        failure = Some(error);
                        return Ok(false);
                    }
                };
                if name_tables {
                    shown.table_names.insert(table.id, name.to_vec());
                }
                if let Some(columns) = column_list {
                    let Some(narrowed) = Narrowed::of(&table, columns) else {
                        shown.hidden.insert(name.to_vec());
                        return Ok(true);
                    };
                    shown.narrowed_names.insert(name.to_vec(), table.id);
                    shown.narrowed.insert(table.id, narrowed);
                }
                shown.relations.insert(name.to_vec());
                shown.tables.insert(table.id);
                Ok(true)
            },
        )?;
        if let Some(error) = failure {
            return Err(error);
        }
        let sequences = sequences_prefix();
        reader.scan(
            &sequences,
            &prefix_end(&sequences),
            ScanDirection::Forward,
            &mut |key, value| {
                let Some(name) = key.get(sequences.len()..) else {
                    return Ok(true);
                };
                let listed_itself = std::str::from_utf8(name).is_ok_and(|text| {
                    tables
                        .internal
                        .iter()
                        .zip(&tables.columns)
                        .any(|(wanted, columns)| wanted == text && columns.is_none())
                });
                let owned_by_visible = SequenceDefinition::decode(value)
                    .ok()
                    .and_then(|sequence| sequence.owned_by)
                    .is_some_and(|(owner, column)| {
                        shown.relations.contains(owner.as_bytes())
                            && shown.shows_column(owner.as_bytes(), &column)
                    });
                if listed_itself || owned_by_visible {
                    shown.relations.insert(name.to_vec());
                } else {
                    shown.hidden.insert(name.to_vec());
                }
                Ok(true)
            },
        )?;
        Ok(shown)
    }

    fn narrowed_named(&self, name: &[u8]) -> Option<&Narrowed> {
        self.narrowed_names
            .get(name)
            .and_then(|id| self.narrowed.get(id))
    }

    fn shows_column(&self, table: &[u8], column: &str) -> bool {
        if let Some(view) = self.narrowed_views.get(table) {
            return view.kept_names.iter().any(|kept| kept == column);
        }
        self.narrowed_named(table)
            .is_none_or(|narrowed| narrowed.kept_names.iter().any(|kept| kept == column))
    }

    fn hides(&self, key: &[u8]) -> bool {
        let owner = key_owner(key);
        let hidden = match owner {
            KeyOwner::Everyone => false,
            KeyOwner::Relation(name) => !self.relations.contains(name),
            KeyOwner::Table(id) => !self.tables.contains(&id),
            KeyOwner::Relations(first, second) => {
                self.hidden.contains(first) || self.hidden.contains(second)
            }
            KeyOwner::CommentOn(owner) => self.hidden.contains(owner),
            KeyOwner::Unreadable => true,
        };
        hidden || (self.narrows_anything() && self.hides_narrowed_part(key, owner))
    }

    fn hides_narrowed_part(&self, key: &[u8], owner: KeyOwner<'_>) -> bool {
        match owner {
            KeyOwner::Table(id) => self
                .narrowed
                .get(&id)
                .is_some_and(|narrowed| narrowed.hides_index_entry(key, id)),
            KeyOwner::CommentOn(owner) => {
                self.is_narrowed(owner)
                    && key
                        .get(NAMESPACE.len() + 1..)
                        .and_then(split_comment_key)
                        .is_none_or(|(_, kind, _)| kind != CommentKind::Relation)
            }
            KeyOwner::Everyone => match crate::roles::column_grant_target(key) {
                Some((table, column)) => std::str::from_utf8(column)
                    .is_ok_and(|column| !self.shows_column(table, column)),
                None => narrowed_setting_owner(key).is_some_and(|owner| self.is_narrowed(owner)),
            },
            KeyOwner::Relation(_) | KeyOwner::Relations(..) | KeyOwner::Unreadable => false,
        }
    }

    fn reshapes(&self, key: &[u8]) -> bool {
        if !self.narrows_anything() {
            return false;
        }
        match key_owner(key) {
            KeyOwner::Relation(name) => {
                (self.is_narrowed(name) || self.unchecked_definitions.contains_key(name))
                    && std::str::from_utf8(name).is_ok_and(|text| key == table_key(text).as_slice())
            }
            KeyOwner::Table(id) => {
                self.narrowed.contains_key(&id) && key.starts_with(&rows_prefix(id))
            }
            _ => false,
        }
    }

    fn reshaped(&self, key: &[u8], value: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        if !self.reshapes(key) {
            return Ok(None);
        }
        match key_owner(key) {
            KeyOwner::Relation(name) => Ok(self
                .narrowed_named(name)
                .map(|narrowed| narrowed.definition.clone())
                .or_else(|| {
                    self.narrowed_views
                        .get(name)
                        .map(|view| view.definition.clone())
                })
                .or_else(|| self.unchecked_definitions.get(name).cloned())),
            KeyOwner::Table(id) => match self.narrowed.get(&id) {
                Some(narrowed) => narrowed.row(value).map(Some),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }
}

fn narrowed_setting_owner(key: &[u8]) -> Option<&[u8]> {
    let (&space, rest) = key.strip_prefix(NAMESPACE)?.split_first()?;
    if space == crate::statistics::STATISTICS_SPACE {
        Some(rest)
    } else if space == crate::sqlite_schema_text::SQLITE_TEXT_SPACE {
        Some(crate::sqlite_schema_text::owning_relation(rest))
    } else {
        None
    }
}

fn get_through(
    shown: Option<&Shown>,
    inner: &dyn KeyValueRead,
    key: &[u8],
) -> SqlResult<Option<Vec<u8>>> {
    let Some(shown) = shown else {
        return inner.get(key);
    };
    if shown.hides(key) {
        return Ok(None);
    }
    let Some(value) = inner.get(key)? else {
        return Ok(None);
    };
    Ok(Some(shown.reshaped(key, &value)?.unwrap_or(value)))
}

fn get_with_through(
    shown: Option<&Shown>,
    inner: &dyn KeyValueRead,
    key: &[u8],
    read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
) -> SqlResult<bool> {
    let Some(shown) = shown else {
        return inner.get_with(key, read);
    };
    if shown.hides(key) {
        return Ok(false);
    }
    if !shown.reshapes(key) {
        return inner.get_with(key, read);
    }
    match get_through(Some(shown), inner, key)? {
        Some(value) => {
            read(&value)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

fn scan_through(
    shown: Option<&Shown>,
    inner: &dyn KeyValueRead,
    start: &[u8],
    end: &[u8],
    direction: ScanDirection,
    visit: &mut EntryVisitor<'_>,
) -> SqlResult<()> {
    let Some(shown) = shown else {
        return inner.scan(start, end, direction, visit);
    };
    inner.scan(start, end, direction, &mut |key, value| {
        if shown.hides(key) {
            return Ok(true);
        }
        match shown.reshaped(key, value)? {
            Some(reshaped) => visit(key, &reshaped),
            None => visit(key, value),
        }
    })
}

/// A read of a store through [`VisibleTables`]: what it hides reads as absent, and scans
/// pass over it; a table listed with columns reads with only those columns. Made by [`VisibleTables::read`], or by [`VisibleRead::everything`] to hide
/// nothing.
#[derive(Debug)]
pub struct VisibleRead<R> {
    inner: R,
    shown: Option<Shown>,
}

impl<R: KeyValueRead> VisibleRead<R> {
    /// `inner` with nothing hidden.
    pub fn everything(inner: R) -> Self {
        Self { inner, shown: None }
    }
}

impl<R: KeyValueRead> KeyValueRead for VisibleRead<R> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        get_through(self.shown.as_ref(), &self.inner, key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        get_with_through(self.shown.as_ref(), &self.inner, key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan_through(
            self.shown.as_ref(),
            &self.inner,
            start,
            end,
            direction,
            visit,
        )
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }

    fn collection_names(&self) -> SqlResult<Vec<String>> {
        match self.shown {
            None => self.inner.collection_names(),
            Some(_) => Ok(Vec::new()),
        }
    }

    fn has_collection(&self, name: &str) -> SqlResult<bool> {
        match self.shown {
            None => self.inner.has_collection(name),
            Some(_) => Ok(false),
        }
    }

    fn collection_get(&self, name: &str, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        match self.shown {
            None => self.inner.collection_get(name, key),
            Some(_) => Ok(None),
        }
    }

    fn collection_scan(
        &self,
        name: &str,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        match self.shown {
            None => self
                .inner
                .collection_scan(name, start, end, direction, visit),
            Some(_) => Ok(()),
        }
    }
}

/// A write transaction confined by [`VisibleTables::write`], or by
/// [`VisibleWrite::everything`] to nothing.
#[derive(Debug)]
pub struct VisibleWrite<W> {
    inner: W,
    confined: Option<Confined>,
}

#[derive(Debug)]
struct Confined {
    listed: Vec<String>,
    shown: Shown,
    links: Vec<Link>,
    rows_blocked: HashSet<u32>,
    keys_guarded: HashMap<u32, KeyGuard>,
    parents_outside: HashMap<u32, ParentsOutside>,
    narrowed_writes: HashMap<u32, NarrowedWrites>,
    inserted_here: HashSet<Vec<u8>>,
    refused: bool,
    dropped_trigger_records: Vec<Vec<u8>>,
    dropped_definitions: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct KeyGuard {
    columns: Vec<Column>,
    positions: Vec<usize>,
    link: usize,
}

impl KeyGuard {
    fn keeps_referenced_columns(&self, old: &[u8], new: &[u8]) -> bool {
        let (Ok(old), Ok(new)) = (
            decode_row(old, &self.columns),
            decode_row(new, &self.columns),
        ) else {
            return false;
        };
        self.positions
            .iter()
            .all(|&at| old.get(at).is_some() && old.get(at) == new.get(at))
    }
}

enum Written<'k> {
    Free,
    Definition(&'k [u8]),
    Rows(u32),
    Link(&'k [u8], &'k [u8]),
    Shared,
}

enum Refusal {
    Outside(String),
    OutsideTable(u32),
    Linked { link: OutsideLink, rows: bool },
    ReferencedRow(OutsideLink),
    Narrowed(String),
    NarrowedView(String),
    NarrowedDelete(String),
    HiddenInsert {
        table: String,
        reason: &'static str,
    },
    HiddenUpdate {
        table: String,
        reason: &'static str,
    },
    TiedColumn {
        table: String,
        column: String,
    },
    Shared,
    Trigger,
    Code { routine: String, place: StoredCode },
    Subquery(StoredCode),
    UnreadablePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredCode {
    TableExpression,
    Policy,
}

impl StoredCode {
    fn what(self) -> &'static str {
        match self {
            Self::TableExpression => "a default, check or generated expression",
            Self::Policy => "a row-security policy",
        }
    }

    fn runs_when(self) -> &'static str {
        match self {
            Self::TableExpression => "every later session that writes the table",
            Self::Policy => "every later session that reads or writes the table",
        }
    }
}

impl Refusal {
    fn error(self) -> SqlError {
        let hint = "Ask the user to add it to the tables this session is limited to (--tables).";
        match self {
            Self::Outside(name) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot change \"{name}\": this session may change only the tables it is limited to (--tables)"
                ),
            )
            .with_hint(hint),
            Self::OutsideTable(id) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot change table number {id}: this session may change only the tables it is limited to (--tables)"
                ),
            )
            .with_hint(hint),
            Self::Linked { link, rows } => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot change the {} of \"{}\": {}, and this session may not change \"{}\" (--tables), so the change could reach it",
                    if rows { "rows" } else { "definition" },
                    link.listed,
                    link.describe(),
                    link.outside
                ),
            )
            .with_hint(format!(
                "Ask the user to add \"{}\" to the tables this session is limited to (--tables).",
                link.outside
            )),
            Self::ReferencedRow(link) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot delete a row of \"{}\" or change a column a foreign key points at: {}, and this session may neither change nor check \"{}\" (--tables)",
                    link.listed,
                    link.describe(),
                    link.outside
                ),
            )
            .with_hint(format!(
                "Inserting rows and changing other columns works. To delete rows or change those columns, ask the user to add \"{}\" to the tables this session is limited to (--tables).",
                link.outside
            )),
            Self::Narrowed(name) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot change \"{name}\": this session may read only some of its columns (--tables), so it may not change its definition"
                ),
            )
            .with_hint("Inserting rows and changing the listed columns works. To change the table itself, ask the user to list it without a column list (--tables)."),
            Self::NarrowedView(name) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot change view \"{name}\": this session may read only some of its columns (--tables)"
                ),
            )
            .with_hint("Ask the user to list the view without a column list (--tables) to let this session change it."),
            Self::NarrowedDelete(name) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot delete rows of \"{name}\" or change their key: this session may read only some of its columns (--tables), and the row would take the others with it"
                ),
            )
            .with_hint("Inserting rows and changing the listed columns works. To delete rows, ask the user to list the table without a column list (--tables)."),
            Self::HiddenInsert { table, reason } => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("cannot insert into \"{table}\": {reason} (--tables)"),
            )
            .with_hint("Ask the user to list that column too, or to list the table without a column list (--tables)."),
            Self::HiddenUpdate { table, reason } => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("cannot change the rows of \"{table}\": {reason} (--tables)"),
            )
            .with_hint("Ask the user to list the table without a column list (--tables)."),
            Self::TiedColumn { table, column } => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot change column \"{column}\" of \"{table}\": a check, unique index or foreign key ties it to a column this session cannot see (--tables), so the change would test that column's value"
                ),
            )
            .with_hint("Ask the user to list the other columns of that check, index or foreign key too (--tables)."),
            Self::Shared => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "cannot change a schema, type, routine, package, role or other object outside a table: this session may change only the tables it is limited to (--tables)",
            )
            .with_hint("Ask the user to make this change, or to run the session without --tables."),
            Self::Trigger => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "cannot create, change or drop a trigger: a trigger runs its function in every later session that changes the table, where --tables does not confine it, and dropping one switches off what it guards",
            )
            .with_hint("Ask the user to change the trigger."),
            Self::Code { routine, place } => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot write {} that calls the routine \"{routine}\": it runs in {}, where --tables does not confine it",
                    place.what(),
                    place.runs_when()
                ),
            )
            .with_hint("Ask the user to make this change."),
            Self::Subquery(place) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot write {} with a subquery: it runs in {}, where --tables does not confine it",
                    place.what(),
                    place.runs_when()
                ),
            )
            .with_hint("Ask the user to make this change."),
            Self::UnreadablePolicy => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "cannot write a row-security policy whose expressions cannot be read back: this session writes only policies it can check (--tables)",
            )
            .with_hint("Ask the user to make this change."),
        }
    }
}

fn new_stored_code(
    reader: &dyn KeyValueRead,
    key: &[u8],
    value: &[u8],
) -> SqlResult<Option<Refusal>> {
    let (expressions, place) = match new_policy_conditions(key, value) {
        Some(Some(expressions)) => (expressions, StoredCode::Policy),
        Some(None) => return Ok(Some(Refusal::UnreadablePolicy)),
        None => (
            new_table_expressions(reader, key, value)?,
            StoredCode::TableExpression,
        ),
    };
    if expressions.iter().any(has_subquery) {
        return Ok(Some(Refusal::Subquery(place)));
    }
    let mut called = Vec::new();
    for expr in &expressions {
        called_names(expr, &mut called);
    }
    let mut tables = crate::execute::TableCache::default();
    for function in called {
        let found = crate::routines::resolve(reader, &mut tables, &function);
        if !matches!(found, Ok(None)) {
            return Ok(Some(Refusal::Code {
                routine: shown_name(&function),
                place,
            }));
        }
    }
    Ok(None)
}

fn new_policy_conditions(key: &[u8], value: &[u8]) -> Option<Option<Vec<Expr>>> {
    let conditions = crate::row_security::stored_conditions(key, value)?;
    let Ok(conditions) = conditions else {
        return Some(None);
    };
    let settings = crate::settings::Settings::default();
    let mut expressions = Vec::with_capacity(conditions.len());
    for condition in conditions {
        match crate::row_security::condition_expression(&condition, &settings) {
            Ok(expr) => expressions.push(expr),
            Err(_) => return Some(None),
        }
    }
    Some(Some(expressions))
}

fn new_table_expressions(
    reader: &dyn KeyValueRead,
    key: &[u8],
    value: &[u8],
) -> SqlResult<Vec<Expr>> {
    let mut expressions = Vec::new();
    let Written::Definition(name) = written(key) else {
        return Ok(expressions);
    };
    let Ok(text) = std::str::from_utf8(name) else {
        return Ok(expressions);
    };
    if key != table_key(text).as_slice() || is_view_record(value) {
        return Ok(expressions);
    }
    let Ok(table) = TableDefinition::decode(text, value) else {
        return Ok(expressions);
    };
    let before = match reader.get(key)? {
        Some(stored) if !is_view_record(&stored) => TableDefinition::decode(text, &stored).ok(),
        _ => None,
    };
    for (at, column) in table.columns.iter().enumerate() {
        let kept = before.as_ref().is_some_and(|before| {
            before
                .columns
                .iter()
                .any(|old| old.name == column.name && old.default == column.default)
        });
        if !kept
            && let Ok(Some(expr)) =
                crate::execute::default_expression(column, table.default_dialect(at))
        {
            expressions.push(expr);
        }
    }
    for check in &table.checks {
        let kept = before.as_ref().is_some_and(|before| {
            before
                .checks
                .iter()
                .any(|old| old.expression == check.expression)
        });
        if !kept && let Ok(expr) = crate::constraints::parse_check(check) {
            expressions.push(expr);
        }
    }
    Ok(expressions)
}

fn has_subquery(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Subquery(_))
        || crate::eval::children(expr).into_iter().any(has_subquery)
}

fn called_names(expr: &Expr, called: &mut Vec<String>) {
    if let ExprKind::Function { name, arguments } = &expr.kind {
        if name == crate::routines::ROUTINE_CALL {
            if let Some(Expr {
                kind: ExprKind::Literal(crate::ast::Literal::String(routine)),
                ..
            }) = arguments.first()
            {
                called.push(routine.clone());
            }
        } else if !name.is_empty() && !name.starts_with('*') {
            called.push(name.clone());
        }
    }
    for child in crate::eval::children(expr) {
        called_names(child, called);
    }
}

fn rolled_back() -> SqlError {
    SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        "the change was rolled back: a part of it reached outside the tables this session may change (--tables)",
    )
}

fn written(key: &[u8]) -> Written<'_> {
    if is_temporary_key(key) || key == meta_key().as_slice() {
        return Written::Free;
    }
    match key_owner(key) {
        KeyOwner::Relation(name) => Written::Definition(name),
        KeyOwner::Table(id) => Written::Rows(id),
        KeyOwner::Relations(first, second) => Written::Link(first, second),
        KeyOwner::CommentOn(owner) => {
            let on_schema = key
                .get(NAMESPACE.len() + 1..)
                .and_then(split_comment_key)
                .is_none_or(|(_, kind, _)| kind == CommentKind::Schema);
            if on_schema {
                Written::Shared
            } else {
                Written::Definition(owner)
            }
        }
        KeyOwner::Everyone => table_setting_owner(key).map_or(Written::Shared, Written::Definition),
        KeyOwner::Unreadable => Written::Shared,
    }
}

fn table_setting_owner(key: &[u8]) -> Option<&[u8]> {
    let (&space, rest) = key.strip_prefix(NAMESPACE)?.split_first()?;
    if space == crate::statistics::STATISTICS_SPACE
        || space == crate::row_security::ROW_SECURITY_SPACE
    {
        Some(rest)
    } else if space == crate::sqlite_schema_text::SQLITE_TEXT_SPACE {
        Some(crate::sqlite_schema_text::owning_relation(rest))
    } else if space == crate::roles::GRANT_SPACE || space == crate::row_security::POLICY_SPACE {
        let end = rest.iter().rposition(|&byte| byte == 0)?;
        rest.get(..end)
    } else if space == crate::roles::COLUMN_GRANT_SPACE {
        crate::roles::column_grant_target(key).map(|(table, _)| table)
    } else {
        None
    }
}

impl Confined {
    fn is_listed(&self, name: &[u8]) -> bool {
        self.listed.iter().any(|listed| listed.as_bytes() == name)
    }

    fn may_use(&self, name: &[u8]) -> bool {
        !self.shown.hidden.contains(name)
            && (self.shown.relations.contains(name) || self.is_listed(name))
    }

    fn shown(name: &[u8]) -> String {
        shown_name(&String::from_utf8_lossy(name))
    }

    fn check(&self, key: &[u8], value: Option<&[u8]>) -> Result<(), Refusal> {
        if value.is_some() && key.starts_with(&triggers_prefix()) && !is_temporary_key(key) {
            return Err(Refusal::Trigger);
        }
        match written(key) {
            Written::Free => Ok(()),
            Written::Definition(name) => self.check_definition(name, key, value),
            Written::Rows(id) => self.check_rows(id, key),
            Written::Link(first, second) => {
                for side in [first, second] {
                    if self.shown.hidden.contains(side) {
                        return Err(Refusal::Outside(Self::shown(side)));
                    }
                }
                if self.may_use(first) || self.may_use(second) {
                    Ok(())
                } else {
                    Err(Refusal::Shared)
                }
            }
            Written::Shared => Err(Refusal::Shared),
        }
    }

    fn check_definition(
        &self,
        name: &[u8],
        key: &[u8],
        value: Option<&[u8]>,
    ) -> Result<(), Refusal> {
        if self.shown.narrowed_named(name).is_some() {
            return Err(Refusal::Narrowed(Self::shown(name)));
        }
        if self.shown.narrowed_views.contains_key(name) {
            return Err(Refusal::NarrowedView(Self::shown(name)));
        }
        if self.may_use(name) {
            return match self.links.iter().find(|link| link.listed == name) {
                Some(link) => Err(Refusal::Linked {
                    link: link.shown(),
                    rows: false,
                }),
                None => Ok(()),
            };
        }
        if !self.shown.hidden.contains(name)
            && let Some(value) = value
            && self.is_owned_sequence(name, key, value)
        {
            return Ok(());
        }
        Err(Refusal::Outside(Self::shown(name)))
    }

    fn is_owned_sequence(&self, name: &[u8], key: &[u8], value: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(name) else {
            return false;
        };
        key == sequence_key(text).as_slice()
            && SequenceDefinition::decode(value)
                .ok()
                .and_then(|sequence| sequence.owned_by)
                .is_some_and(|(owner, _)| self.may_use(owner.as_bytes()))
    }

    fn check_rows(&self, id: u32, key: &[u8]) -> Result<(), Refusal> {
        if !self.shown.tables.contains(&id) {
            return Err(match self.shown.table_names.get(&id) {
                Some(name) => Refusal::Outside(Self::shown(name)),
                None => Refusal::OutsideTable(id),
            });
        }
        if let Some(narrowed) = self.shown.narrowed.get(&id)
            && narrowed.hides_index_entry(key, id)
        {
            return Err(Refusal::Narrowed(Self::shown(&narrowed.name)));
        }
        if !self.rows_blocked.contains(&id) {
            return Ok(());
        }
        let link = self.shown.table_names.get(&id).and_then(|name| {
            self.links
                .iter()
                .find(|link| link.kind.blocks_rows() && &link.listed == name)
        });
        match link {
            Some(link) => Err(Refusal::Linked {
                link: link.shown(),
                rows: true,
            }),
            None => Err(Refusal::OutsideTable(id)),
        }
    }

    fn learn(&mut self, key: &[u8], value: &[u8]) {
        let Written::Definition(name) = written(key) else {
            return;
        };
        let Ok(text) = std::str::from_utf8(name) else {
            return;
        };
        if key == table_key(text).as_slice() {
            self.shown.relations.insert(name.to_vec());
            if !is_view_record(value)
                && let Ok(table) = TableDefinition::decode(text, value)
            {
                self.shown.tables.insert(table.id);
                self.shown.table_names.insert(table.id, name.to_vec());
            }
        } else if key == sequence_key(text).as_slice() {
            self.shown.relations.insert(name.to_vec());
        }
    }

    fn note_deleted(&mut self, key: &[u8]) {
        if key.starts_with(&triggers_prefix()) && !is_temporary_key(key) {
            self.dropped_trigger_records.push(key.to_vec());
            return;
        }
        let Written::Definition(name) = written(key) else {
            return;
        };
        if let Ok(text) = std::str::from_utf8(name)
            && key == table_key(text).as_slice()
        {
            self.dropped_definitions.push(trigger_key(text));
        }
    }

    fn drops_a_trigger_of_a_kept_table(&self) -> bool {
        self.dropped_trigger_records
            .iter()
            .any(|record| !self.dropped_definitions.contains(record))
    }

    fn admit(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        match self.check(key, Some(value)) {
            Ok(()) => {
                self.learn(key, value);
                Ok(())
            }
            Err(refusal) => {
                self.refused = true;
                Err(refusal.error())
            }
        }
    }
}

impl<W: KeyValueWrite> VisibleWrite<W> {
    fn admit(&mut self, key: &[u8], value: &[u8], replaces: bool) -> SqlResult<()> {
        let Some(confined) = &mut self.confined else {
            return Ok(());
        };
        if let Some(refusal) = new_stored_code(&self.inner, key, value)? {
            confined.refused = true;
            return Err(refusal.error());
        }
        confined.admit(key, value)?;
        if replaces {
            self.guard_referenced_columns(key, Some(value))?;
        }
        Ok(())
    }

    fn guard_referenced_columns(&mut self, key: &[u8], new: Option<&[u8]>) -> SqlResult<()> {
        let Some(confined) = &mut self.confined else {
            return Ok(());
        };
        let KeyOwner::Table(id) = key_owner(key) else {
            return Ok(());
        };
        let Some(guard) = confined.keys_guarded.get(&id) else {
            return Ok(());
        };
        if !key.starts_with(&rows_prefix(id)) {
            return Ok(());
        }
        let Some(old) = self.inner.get(key)? else {
            return Ok(());
        };
        if new.is_some_and(|new| guard.keeps_referenced_columns(&old, new)) {
            return Ok(());
        }
        let refusal = match confined.links.get(guard.link) {
            Some(link) => Refusal::ReferencedRow(link.shown()),
            None => Refusal::OutsideTable(id),
        };
        confined.refused = true;
        Err(refusal.error())
    }

    fn narrowed_id(&self, key: &[u8]) -> Option<u32> {
        let confined = self.confined.as_ref()?;
        let KeyOwner::Table(id) = key_owner(key) else {
            return None;
        };
        (confined.shown.narrowed.contains_key(&id) && key.starts_with(&rows_prefix(id)))
            .then_some(id)
    }

    fn narrowed_row(&mut self, id: u32, key: &[u8], value: &[u8]) -> SqlResult<NarrowedRow> {
        let Some(confined) = &self.confined else {
            return Err(crate::encoding::corrupt("row"));
        };
        let (Some(narrowed), Some(writes)) = (
            confined.shown.narrowed.get(&id),
            confined.narrowed_writes.get(&id),
        ) else {
            return Err(crate::encoding::corrupt("row"));
        };
        let shown_values = decode_row(value, &writes.shown_columns)?;
        let planned = match self.inner.get(key)? {
            Some(stored) => {
                let old = row_of_width(&stored, &narrowed.full.columns)?;
                narrowed_update(narrowed, writes, old, &shown_values)
            }
            None => narrowed_insert(narrowed, writes, &shown_values)?,
        };
        match planned {
            Ok(row) => Ok(row),
            Err(refusal) => {
                if let Some(confined) = &mut self.confined {
                    confined.refused = true;
                }
                Err(refusal.error())
            }
        }
    }

    fn write_hidden_entries(&mut self, id: u32, key: &[u8], row: &NarrowedRow) -> SqlResult<()> {
        let Some(confined) = &self.confined else {
            return Ok(());
        };
        let (Some(narrowed), Some(writes)) = (
            confined.shown.narrowed.get(&id),
            confined.narrowed_writes.get(&id),
        ) else {
            return Ok(());
        };
        let entries: Vec<(Option<Vec<u8>>, Vec<u8>)> = writes
            .maintained
            .iter()
            .map(|index| {
                (
                    row.old
                        .as_ref()
                        .map(|old| crate::indexes::entry_key(&narrowed.full, index, old, key)),
                    crate::indexes::entry_key(&narrowed.full, index, &row.new, key),
                )
            })
            .collect();
        for (old, new) in entries {
            if old.as_ref() == Some(&new) {
                continue;
            }
            if let Some(old) = old {
                self.inner.delete(&old)?;
            }
            self.inner.put(&new, &[])?;
        }
        Ok(())
    }

    fn delete_narrowed_row(&mut self, id: u32, key: &[u8]) -> SqlResult<bool> {
        let Some(stored) = self.inner.get(key)? else {
            return Ok(false);
        };
        let Some(confined) = &mut self.confined else {
            return self.inner.delete(key);
        };
        let (Some(narrowed), Some(writes)) = (
            confined.shown.narrowed.get(&id),
            confined.narrowed_writes.get(&id),
        ) else {
            return Err(crate::encoding::corrupt("row"));
        };
        if !confined.inserted_here.contains(key) {
            let refusal = Refusal::NarrowedDelete(Confined::shown(&narrowed.name));
            confined.refused = true;
            return Err(refusal.error());
        }
        let row = row_of_width(&stored, &narrowed.full.columns)?;
        let entries: Vec<Vec<u8>> = writes
            .maintained
            .iter()
            .map(|index| crate::indexes::entry_key(&narrowed.full, index, &row, key))
            .collect();
        confined.inserted_here.remove(key);
        for entry in entries {
            self.inner.delete(&entry)?;
        }
        self.inner.delete(key)
    }

    fn check_parents_outside(&self, key: &[u8], row: &[u8]) -> SqlResult<()> {
        let Some(confined) = &self.confined else {
            return Ok(());
        };
        let KeyOwner::Table(id) = key_owner(key) else {
            return Ok(());
        };
        let Some(parents) = confined.parents_outside.get(&id) else {
            return Ok(());
        };
        if !key.starts_with(&rows_prefix(id)) {
            return Ok(());
        }
        let new = row_of_width(row, &parents.child.columns)?;
        let old = match self.inner.get(key)? {
            Some(stored) => Some(row_of_width(&stored, &parents.child.columns)?),
            None => None,
        };
        for (foreign_key, parent) in &parents.keys {
            crate::constraints::check_parent_outside_the_session(
                &self.inner,
                &parents.child,
                foreign_key,
                parent,
                &new,
                old.as_deref(),
            )?;
        }
        Ok(())
    }

    fn put_checked(&mut self, key: &[u8], value: &[u8], replaces: bool) -> SqlResult<bool> {
        let narrowed = match self.narrowed_id(key) {
            Some(id) => {
                if !replaces && self.inner.get(key)?.is_some() {
                    return Ok(false);
                }
                Some((id, self.narrowed_row(id, key, value)?))
            }
            None => None,
        };
        let merged = narrowed.as_ref().map(|(_, row)| encode_row(&row.new));
        let stored = merged.as_deref().unwrap_or(value);
        self.admit(key, stored, replaces)?;
        self.check_parents_outside(key, stored)?;
        if let Some((id, row)) = &narrowed {
            self.write_hidden_entries(*id, key, row)?;
        }
        let written = if replaces {
            self.inner.put(key, stored)?;
            true
        } else {
            self.inner.put_if_absent(key, stored)?
        };
        if written
            && narrowed.as_ref().is_some_and(|(_, row)| row.old.is_none())
            && let Some(confined) = &mut self.confined
        {
            confined.inserted_here.insert(key.to_vec());
        }
        Ok(written)
    }

    /// `inner` with nothing hidden and nothing refused.
    pub fn everything(inner: W) -> Self {
        Self {
            inner,
            confined: None,
        }
    }

    fn shown(&self) -> Option<&Shown> {
        self.confined.as_ref().map(|confined| &confined.shown)
    }
}

impl<W: KeyValueWrite> KeyValueRead for VisibleWrite<W> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        get_through(self.shown(), &self.inner, key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        get_with_through(self.shown(), &self.inner, key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan_through(self.shown(), &self.inner, start, end, direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }
}

impl<W: KeyValueWrite> KeyValueWrite for VisibleWrite<W> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        if self.confined.is_none() {
            return self.inner.put(key, value);
        }
        self.put_checked(key, value, true).map(|_| ())
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        if self.confined.is_none() {
            return self.inner.put_if_absent(key, value);
        }
        self.put_checked(key, value, false)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        if let Some(id) = self.narrowed_id(key) {
            return self.delete_narrowed_row(id, key);
        }
        let Some(confined) = &mut self.confined else {
            return self.inner.delete(key);
        };
        if let Err(refusal) = confined.check(key, None) {
            if self.inner.get(key)?.is_none() {
                return Ok(false);
            }
            confined.refused = true;
            return Err(refusal.error());
        }
        self.guard_referenced_columns(key, None)?;
        let deleted = self.inner.delete(key)?;
        if deleted && let Some(confined) = &mut self.confined {
            confined.note_deleted(key);
        }
        Ok(deleted)
    }

    fn commit(self) -> SqlResult<()> {
        let Some(confined) = &self.confined else {
            return self.inner.commit();
        };
        if confined.refused {
            self.inner.rollback()?;
            return Err(rolled_back());
        }
        if confined.drops_a_trigger_of_a_kept_table() {
            self.inner.rollback()?;
            return Err(Refusal::Trigger.error());
        }
        self.inner.commit()
    }

    fn rollback(self) -> SqlResult<()> {
        self.inner.rollback()
    }
}
