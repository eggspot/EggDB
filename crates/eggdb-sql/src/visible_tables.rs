use crate::encoding::{
    CommentKind, KeyOwner, NAMESPACE, SequenceDefinition, TableDefinition, every_dependency_prefix,
    every_reference_prefix, is_temporary_key, is_view_record, key_owner, meta_key, prefix_end,
    sequence_key, sequences_prefix, split_comment_key, table_key, trigger_key, triggers_prefix,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::schemas::{DEFAULT_SCHEMA, QUALIFIER};
use crate::sqlserver_catalog::DBO;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection};
use std::collections::{HashMap, HashSet};

/// The tables and views a session may see, such as those `eggdb mcp --tables` names (D156).
///
/// A [`VisibleRead`] made from it hides every other table, view and sequence from the SQL
/// engine above it: their definitions, rows, indexes, comments, foreign-key and dependency
/// records read as absent, so every dialect, every catalog (`pg_catalog`,
/// `information_schema`, SQL Server's `sys` views, Oracle's dictionary) and every name
/// resolution, a view's query included, finds them missing. A sequence stays visible when it
/// is named or when a visible table owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleTables {
    internal: Vec<String>,
}

/// A name of a [`VisibleTables`] that no table, view or sequence of the database has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTableName {
    /// The name as [`VisibleTables::names`] shows it, such as `archive.orders`.
    pub name: String,
    /// The stored name it matches when letter case is ignored, shown the same way.
    pub stored_as: Option<String>,
}

impl VisibleTables {
    /// The tables and views with these names, each `name` or `schema.name`, split at the first
    /// dot; `public.` and `dbo.` name the default schema. Names match exactly as stored, and a
    /// name given twice counts once, in the order first given.
    pub fn new<'n>(names: impl IntoIterator<Item = &'n str>) -> Self {
        let mut internal: Vec<String> = Vec::new();
        for name in names {
            let stored = internal_name(name);
            if !internal.contains(&stored) {
                internal.push(stored);
            }
        }
        Self { internal }
    }

    /// The names, in order: the default schema's bare, others as `schema.name`.
    pub fn names(&self) -> Vec<String> {
        self.internal.iter().map(|name| shown_name(name)).collect()
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
        let shown = Shown::of(&self.internal, &inner, false)?;
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
    /// where the change could reach that relation: its rows and definition when a foreign key
    /// joins them, its definition when a view reads it. A transaction with a refused change
    /// rolls back at commit even when the refusal was caught along the way.
    pub fn write<W: KeyValueWrite>(&self, inner: W) -> SqlResult<VisibleWrite<W>> {
        let shown = Shown::of(&self.internal, &inner, true)?;
        let links = links_of(&shown, &inner)?;
        let rows_blocked = shown
            .table_names
            .iter()
            .filter(|(id, name)| {
                shown.tables.contains(*id)
                    && links
                        .iter()
                        .any(|link| link.kind.blocks_rows() && &link.listed == *name)
            })
            .map(|(id, _)| *id)
            .collect();
        Ok(VisibleWrite {
            inner,
            confined: Some(Confined {
                listed: self.internal.clone(),
                shown,
                links,
                rows_blocked,
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
        let shown = Shown::of(&self.internal, reader, false)?;
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
    /// What a confined writer cannot change because of this tie, in words, such as `rows and
    /// definition`.
    pub fn blocked_changes(&self) -> &'static str {
        if self.kind.blocks_rows() {
            "rows and definition"
        } else {
            "definition"
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
                if shown.relations.contains(target) && shown.hidden.contains(source) {
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

#[derive(Debug)]
struct Shown {
    relations: HashSet<Vec<u8>>,
    hidden: HashSet<Vec<u8>>,
    tables: HashSet<u32>,
    table_names: HashMap<u32, Vec<u8>>,
}

impl Shown {
    fn of(listed: &[String], reader: &dyn KeyValueRead, name_tables: bool) -> SqlResult<Self> {
        let mut shown = Self {
            relations: HashSet::new(),
            hidden: HashSet::new(),
            tables: HashSet::new(),
            table_names: HashMap::new(),
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
                let visible = std::str::from_utf8(name)
                    .is_ok_and(|text| listed.iter().any(|wanted| wanted == text));
                if !visible {
                    shown.hidden.insert(name.to_vec());
                    if name_tables
                        && !is_view_record(value)
                        && let Ok(table) =
                            TableDefinition::decode(&String::from_utf8_lossy(name), value)
                    {
                        shown.table_names.insert(table.id, name.to_vec());
                    }
                    return Ok(true);
                }
                shown.relations.insert(name.to_vec());
                if !is_view_record(value) {
                    let text = String::from_utf8_lossy(name);
                    match TableDefinition::decode(&text, value) {
                        Ok(table) => {
                            shown.tables.insert(table.id);
                            if name_tables {
                                shown.table_names.insert(table.id, name.to_vec());
                            }
                        }
                        Err(error) => {
                            failure = Some(error);
                            return Ok(false);
                        }
                    }
                }
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
                let listed_itself = std::str::from_utf8(name)
                    .is_ok_and(|text| listed.iter().any(|wanted| wanted == text));
                let owned_by_visible = SequenceDefinition::decode(value)
                    .ok()
                    .and_then(|sequence| sequence.owned_by)
                    .is_some_and(|(owner, _)| shown.relations.contains(owner.as_bytes()));
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

    fn hides(&self, key: &[u8]) -> bool {
        match key_owner(key) {
            KeyOwner::Everyone => false,
            KeyOwner::Relation(name) => !self.relations.contains(name),
            KeyOwner::Table(id) => !self.tables.contains(&id),
            KeyOwner::Relations(first, second) => {
                self.hidden.contains(first) || self.hidden.contains(second)
            }
            KeyOwner::CommentOn(owner) => self.hidden.contains(owner),
            KeyOwner::Unreadable => true,
        }
    }
}

/// A read of a store through [`VisibleTables`]: what it hides reads as absent, and scans
/// pass over it. Made by [`VisibleTables::read`], or by [`VisibleRead::everything`] to hide
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

    fn hides(&self, key: &[u8]) -> bool {
        self.shown.as_ref().is_some_and(|shown| shown.hides(key))
    }
}

impl<R: KeyValueRead> KeyValueRead for VisibleRead<R> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        if self.hides(key) {
            return Ok(None);
        }
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        if self.hides(key) {
            return Ok(false);
        }
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let Some(shown) = &self.shown else {
            return self.inner.scan(start, end, direction, visit);
        };
        self.inner.scan(start, end, direction, &mut |key, value| {
            if shown.hides(key) {
                return Ok(true);
            }
            visit(key, value)
        })
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
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
    refused: bool,
    dropped_trigger_records: Vec<Vec<u8>>,
    dropped_definitions: Vec<Vec<u8>>,
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
    Linked(OutsideLink),
    Shared,
    Trigger,
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
            Self::Linked(link) => SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot change the {} of \"{}\": {}, and this session may not change \"{}\" (--tables), so the change could reach it",
                    link.blocked_changes(),
                    link.listed,
                    link.describe(),
                    link.outside
                ),
            )
            .with_hint(format!(
                "Ask the user to add \"{}\" to the tables this session is limited to (--tables).",
                link.outside
            )),
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
        }
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
        || space == crate::sqlite_schema_text::SQLITE_TEXT_SPACE
    {
        Some(rest)
    } else if space == crate::roles::GRANT_SPACE || space == crate::row_security::POLICY_SPACE {
        let end = rest.iter().position(|&byte| byte == 0)?;
        rest.get(..end)
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
            Written::Rows(id) => self.check_rows(id),
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
        if self.may_use(name) {
            return match self.links.iter().find(|link| link.listed == name) {
                Some(link) => Err(Refusal::Linked(link.shown())),
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

    fn check_rows(&self, id: u32) -> Result<(), Refusal> {
        if !self.shown.tables.contains(&id) {
            return Err(match self.shown.table_names.get(&id) {
                Some(name) => Refusal::Outside(Self::shown(name)),
                None => Refusal::OutsideTable(id),
            });
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
            Some(link) => Err(Refusal::Linked(link.shown())),
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
    /// `inner` with nothing hidden and nothing refused.
    pub fn everything(inner: W) -> Self {
        Self {
            inner,
            confined: None,
        }
    }

    fn hides(&self, key: &[u8]) -> bool {
        self.confined
            .as_ref()
            .is_some_and(|confined| confined.shown.hides(key))
    }
}

impl<W: KeyValueWrite> KeyValueRead for VisibleWrite<W> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        if self.hides(key) {
            return Ok(None);
        }
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        if self.hides(key) {
            return Ok(false);
        }
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let Some(confined) = &self.confined else {
            return self.inner.scan(start, end, direction, visit);
        };
        self.inner.scan(start, end, direction, &mut |key, value| {
            if confined.shown.hides(key) {
                return Ok(true);
            }
            visit(key, value)
        })
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }
}

impl<W: KeyValueWrite> KeyValueWrite for VisibleWrite<W> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        if let Some(confined) = &mut self.confined {
            confined.admit(key, value)?;
        }
        self.inner.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        if let Some(confined) = &mut self.confined {
            confined.admit(key, value)?;
        }
        self.inner.put_if_absent(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
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
        let deleted = self.inner.delete(key)?;
        if deleted {
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
