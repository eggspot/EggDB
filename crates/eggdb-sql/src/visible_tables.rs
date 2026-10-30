use crate::encoding::{
    KeyOwner, SequenceDefinition, TableDefinition, is_view_record, key_owner, prefix_end,
    sequence_key, sequences_prefix, table_key,
};
use crate::error::SqlResult;
use crate::schemas::{DEFAULT_SCHEMA, QUALIFIER};
use crate::sqlserver_catalog::DBO;
use crate::store::{EntryVisitor, KeyValueRead, ScanDirection};
use std::collections::HashSet;

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
        let shown = Shown::of(&self.internal, &inner)?;
        Ok(VisibleRead {
            inner,
            shown: Some(shown),
        })
    }
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
    reader.scan(prefix, &prefix_end(prefix), ScanDirection::Forward, &mut |key, _| {
        if let Some(name) = key.get(prefix.len()..) {
            names.push(String::from_utf8_lossy(name).into_owned());
        }
        Ok(true)
    })?;
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
}

impl Shown {
    fn of(listed: &[String], reader: &dyn KeyValueRead) -> SqlResult<Self> {
        let mut shown = Self {
            relations: HashSet::new(),
            hidden: HashSet::new(),
            tables: HashSet::new(),
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
                    return Ok(true);
                }
                shown.relations.insert(name.to_vec());
                if !is_view_record(value) {
                    let text = String::from_utf8_lossy(name);
                    match TableDefinition::decode(&text, value) {
                        Ok(table) => {
                            shown.tables.insert(table.id);
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
