use crate::error::SqlResult;
use crate::settings::{SettingChange, SettingScope, Settings};
use std::cell::{Cell, RefCell};
use std::ops::Deref;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lasts {
    Session,
    Transaction,
    Routine,
}

#[derive(Debug, Clone)]
struct JournalEntry {
    name: String,
    value: Option<String>,
    lasts: Lasts,
    active: bool,
    committed: bool,
}

impl JournalEntry {
    fn is_superseded_by(&self, later: &JournalEntry) -> bool {
        self.name == later.name
            && self.lasts != Lasts::Routine
            && (!self.active
                || (later.lasts == Lasts::Session && (!self.committed || later.committed)))
    }

    fn takes_value_of(&self, later: &JournalEntry) -> bool {
        self.lasts == Lasts::Transaction
            && later.lasts == Lasts::Transaction
            && self.active
            && !self.committed
            && !later.committed
    }
}

#[derive(Default)]
struct Journal {
    root: Option<Settings>,
    entries: Vec<JournalEntry>,
    floor: usize,
}

impl Journal {
    fn push_coalesced(&mut self, entry: JournalEntry) {
        if !entry.active {
            return;
        }
        let floor = self.floor.min(self.entries.len());
        let mut index = floor;
        while let Some(earlier) = self.entries.get(index) {
            if earlier.is_superseded_by(&entry) {
                self.entries.remove(index);
            } else {
                index += 1;
            }
        }
        let continued = self
            .entries
            .get_mut(floor..)
            .and_then(|level| {
                level
                    .iter_mut()
                    .rev()
                    .find(|earlier| earlier.name == entry.name)
            })
            .filter(|last| last.takes_value_of(&entry));
        match continued {
            Some(last) => last.value = entry.value,
            None => self.entries.push(entry),
        }
    }

    fn coalesce_from(&mut self, start: usize) {
        let start = start.min(self.entries.len());
        let level = self.entries.split_off(start);
        for entry in level {
            self.push_coalesced(entry);
        }
    }
}

thread_local! {
    static JOURNAL: RefCell<Journal> = RefCell::new(Journal::default());
    static VERSION: Cell<u64> = const { Cell::new(0) };
}

pub(crate) const NEVER_SYNCED: u64 = u64::MAX;

fn bump() {
    VERSION.with(|version| version.set(version.get().wrapping_add(1) % NEVER_SYNCED));
}

pub(crate) fn version() -> u64 {
    VERSION.with(Cell::get)
}

fn with_journal<T: Default>(change: impl FnOnce(&mut Journal) -> T) -> T {
    JOURNAL.with(|journal| {
        journal
            .try_borrow_mut()
            .map(|mut journal| change(&mut journal))
            .unwrap_or_default()
    })
}

fn record(current: &Settings, name: &str, value: Option<&str>, lasts: Lasts) {
    with_journal(|journal| {
        if journal.root.is_none() {
            journal.root = Some(current.clone());
        }
        journal.push_coalesced(JournalEntry {
            name: Settings::name_of(name),
            value: value.map(str::to_owned),
            lasts,
            active: true,
            committed: false,
        });
    });
    bump();
}

pub(crate) enum RoutineSettings<'r> {
    Given(&'r Settings),
    Changed(Arc<Settings>),
}

impl Deref for RoutineSettings<'_> {
    type Target = Settings;

    fn deref(&self) -> &Settings {
        match self {
            Self::Given(settings) => settings,
            Self::Changed(settings) => settings,
        }
    }
}

pub(crate) struct EnteredSettings {
    _path: crate::schemas::EnteredPath,
    _clock: crate::datetime::StatementClockGuard,
}

impl RoutineSettings<'_> {
    pub(crate) fn enter(&self) -> Option<EnteredSettings> {
        let Self::Changed(settings) = self else {
            return None;
        };
        Some(EnteredSettings {
            _path: crate::schemas::enter(
                settings,
                crate::dialect::Dialect::PostgreSql,
                &crate::schemas::PathCache::default(),
            ),
            _clock: crate::datetime::enter_zone(settings.time_zone),
        })
    }
}

pub(crate) fn change(
    current: &Settings,
    name: &str,
    value: Option<&str>,
    scope: SettingScope,
) -> SqlResult<Settings> {
    let mut next = current.clone();
    next.set(name, value)?;
    let lasts = match scope {
        SettingScope::Session => Lasts::Session,
        SettingScope::Transaction => Lasts::Transaction,
    };
    record(current, name, value, lasts);
    Ok(next)
}

pub(crate) struct ClauseScope {
    start: usize,
    count: usize,
    names: Vec<String>,
    outer_floor: usize,
}

pub(crate) fn enter_clause(
    current: &Settings,
    clause: &[(String, String)],
) -> SqlResult<Option<(ClauseScope, Settings)>> {
    if clause.is_empty() {
        return Ok(None);
    }
    let mut next = current.clone();
    for (name, value) in clause {
        next.set(name, Some(value.as_str()))?;
    }
    let names: Vec<String> = clause
        .iter()
        .map(|(name, _)| Settings::name_of(name))
        .collect();
    let (start, outer_floor) = with_journal(|journal| {
        if journal.root.is_none() {
            journal.root = Some(current.clone());
        }
        let start = journal.entries.len();
        for (name, (_, value)) in names.iter().zip(clause) {
            journal.entries.push(JournalEntry {
                name: name.clone(),
                value: Some(value.clone()),
                lasts: Lasts::Routine,
                active: true,
                committed: false,
            });
        }
        let outer_floor = std::mem::replace(&mut journal.floor, journal.entries.len());
        (start, outer_floor)
    });
    bump();
    let scope = ClauseScope {
        start,
        count: clause.len(),
        names,
        outer_floor,
    };
    Ok(Some((scope, next)))
}

pub(crate) fn leave_clause(scope: ClauseScope) {
    let own_end = scope.start.saturating_add(scope.count);
    with_journal(|journal| {
        for (at, entry) in journal.entries.iter_mut().enumerate().skip(scope.start) {
            let restored = at < own_end
                || (entry.lasts == Lasts::Transaction && scope.names.contains(&entry.name));
            if restored {
                entry.active = false;
            }
        }
        journal.floor = scope.outer_floor.min(scope.start);
        journal.coalesce_from(scope.start);
    });
    bump();
}

pub(crate) struct SettingsMark {
    at: usize,
    outer_floor: usize,
}

pub(crate) fn mark() -> SettingsMark {
    with_journal(|journal| {
        let at = journal.entries.len();
        let outer_floor = std::mem::replace(&mut journal.floor, at);
        SettingsMark { at, outer_floor }
    })
}

impl Default for SettingsMark {
    fn default() -> Self {
        Self {
            at: usize::MAX,
            outer_floor: usize::MAX,
        }
    }
}

pub(crate) fn release(mark: SettingsMark) {
    let changed = with_journal(|journal| {
        journal.floor = mark.outer_floor.min(mark.at);
        let changed = journal.entries.len() > mark.at;
        journal.coalesce_from(mark.at);
        changed
    });
    if changed {
        bump();
    }
}

pub(crate) fn undo_since(mark: SettingsMark) {
    let changed = with_journal(|journal| {
        journal.floor = mark.outer_floor.min(mark.at);
        let changed = journal.entries.len() > mark.at;
        journal.entries.truncate(mark.at);
        changed
    });
    if changed {
        bump();
    }
}

pub(crate) fn end_procedure_transaction(committed: bool) {
    let changed = with_journal(|journal| {
        if journal.entries.is_empty() {
            return false;
        }
        for entry in journal.entries.iter_mut().filter(|entry| entry.active) {
            match entry.lasts {
                Lasts::Transaction => entry.active = false,
                Lasts::Session if committed => entry.committed = true,
                Lasts::Session if !entry.committed => entry.active = false,
                Lasts::Session | Lasts::Routine => {}
            }
        }
        let floor = journal.floor;
        journal.coalesce_from(floor);
        true
    });
    if changed {
        bump();
    }
}

pub(crate) fn effective_if_changed(synced: &mut u64) -> Option<Settings> {
    let version = version();
    if *synced == version {
        return None;
    }
    *synced = version;
    JOURNAL.with(|journal| {
        let journal = journal.try_borrow().ok()?;
        let mut effective = journal.root.clone()?;
        for entry in journal.entries.iter().filter(|entry| entry.active) {
            let _ = effective.set(&entry.name, entry.value.as_deref());
        }
        Some(effective)
    })
}

pub(crate) fn take_for_session(succeeded: bool) -> Vec<SettingChange> {
    let (had_root, taken) = with_journal(|journal| {
        journal.floor = 0;
        (
            journal.root.take().is_some(),
            std::mem::take(&mut journal.entries),
        )
    });
    if !had_root && taken.is_empty() {
        return Vec::new();
    }
    bump();
    taken
        .into_iter()
        .filter(|entry| entry.active && (succeeded || entry.committed))
        .filter_map(|entry| {
            let scope = match entry.lasts {
                Lasts::Session => SettingScope::Session,
                Lasts::Transaction => SettingScope::Transaction,
                Lasts::Routine => return None,
            };
            Some(SettingChange {
                name: entry.name,
                value: entry.value,
                scope,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(name: &str, value: &str) -> Settings {
        let mut settings = Settings::default();
        settings.set(name, Some(value)).unwrap();
        settings
    }

    fn journal_len() -> usize {
        JOURNAL.with(|journal| journal.borrow().entries.len())
    }

    #[test]
    fn a_clause_is_restored_at_exit_and_a_plain_set_inside_it_outlives_it() {
        let _ = take_for_session(true);
        let base = settings_with("app.mode", "outer");
        let (scope, inside) = enter_clause(&base, &[("app.mode".into(), "inner".into())])
            .unwrap()
            .unwrap();
        assert_eq!(inside.get("app.mode").unwrap(), "inner");
        let local = change(
            &inside,
            "app.mode",
            Some("local"),
            SettingScope::Transaction,
        )
        .unwrap();
        assert_eq!(local.get("app.mode").unwrap(), "local");
        let mut synced = NEVER_SYNCED;
        leave_clause(scope);
        let after = effective_if_changed(&mut synced).unwrap();
        assert_eq!(after.get("app.mode").unwrap(), "outer");
        assert!(take_for_session(true).is_empty());

        let (scope, inside) = enter_clause(&base, &[("app.mode".into(), "inner".into())])
            .unwrap()
            .unwrap();
        change(&inside, "app.mode", Some("kept"), SettingScope::Session).unwrap();
        leave_clause(scope);
        let after = effective_if_changed(&mut synced).unwrap();
        assert_eq!(after.get("app.mode").unwrap(), "kept");
        assert_eq!(
            take_for_session(true),
            vec![SettingChange {
                name: "app.mode".into(),
                value: Some("kept".into()),
                scope: SettingScope::Session,
            }]
        );
    }

    #[test]
    fn a_set_local_inside_a_routine_without_a_clause_lasts_to_the_transaction_end() {
        let _ = take_for_session(true);
        let base = Settings::default();
        let (scope, inside) = enter_clause(&base, &[("app.other".into(), "x".into())])
            .unwrap()
            .unwrap();
        change(
            &inside,
            "app.tenant",
            Some("blue"),
            SettingScope::Transaction,
        )
        .unwrap();
        leave_clause(scope);
        assert_eq!(
            take_for_session(true),
            vec![SettingChange {
                name: "app.tenant".into(),
                value: Some("blue".into()),
                scope: SettingScope::Transaction,
            }]
        );
    }

    #[test]
    fn a_procedure_commit_ends_set_local_and_a_rollback_undoes_uncommitted_sets() {
        let _ = take_for_session(true);
        let base = Settings::default();
        let first = change(&base, "app.a", Some("1"), SettingScope::Session).unwrap();
        let second = change(&first, "app.b", Some("2"), SettingScope::Transaction).unwrap();
        end_procedure_transaction(true);
        let mut synced = NEVER_SYNCED;
        let after_commit = effective_if_changed(&mut synced).unwrap();
        assert_eq!(after_commit.get("app.a").unwrap(), "1");
        assert!(after_commit.get("app.b").is_err());
        change(&second, "app.c", Some("3"), SettingScope::Session).unwrap();
        end_procedure_transaction(false);
        let after_rollback = effective_if_changed(&mut synced).unwrap();
        assert!(after_rollback.get("app.c").is_err());
        assert_eq!(
            take_for_session(false),
            vec![SettingChange {
                name: "app.a".into(),
                value: Some("1".into()),
                scope: SettingScope::Session,
            }]
        );
    }

    #[test]
    fn an_exception_block_undoes_the_changes_made_inside_it() {
        let _ = take_for_session(true);
        let base = Settings::default();
        let at = mark();
        change(&base, "app.a", Some("1"), SettingScope::Session).unwrap();
        undo_since(at);
        let mut synced = NEVER_SYNCED;
        let after = effective_if_changed(&mut synced).unwrap();
        assert!(after.get("app.a").is_err());
        assert!(take_for_session(true).is_empty());
    }

    #[test]
    fn an_invalid_value_changes_nothing() {
        let _ = take_for_session(true);
        let base = Settings::default();
        assert!(change(&base, "server_version", Some("1"), SettingScope::Session).is_err());
        assert_eq!(journal_len(), 0);
    }

    #[test]
    fn repeated_sets_of_one_name_keep_one_entry_each_level() {
        let _ = take_for_session(true);
        let mut current = Settings::default();
        for round in 0..100_000 {
            let value = round.to_string();
            let scope = if round % 2 == 0 {
                SettingScope::Session
            } else {
                SettingScope::Transaction
            };
            current = change(&current, "app.n", Some(&value), scope).unwrap();
        }
        assert!(journal_len() <= 2);
        for round in 0..10_000 {
            let block = mark();
            let value = round.to_string();
            current = change(&current, "app.n", Some(&value), SettingScope::Session).unwrap();
            release(block);
        }
        assert_eq!(journal_len(), 1);
        let mut synced = NEVER_SYNCED;
        let effective = effective_if_changed(&mut synced).unwrap();
        assert_eq!(effective.get("app.n").unwrap(), "9999");
        assert_eq!(
            take_for_session(true),
            vec![SettingChange {
                name: "app.n".into(),
                value: Some("9999".into()),
                scope: SettingScope::Session,
            }]
        );
    }

    #[test]
    fn coalesced_sets_keep_set_local_after_set_and_set_after_set_local() {
        let _ = take_for_session(true);
        let base = Settings::default();
        let a = change(&base, "app.n", Some("s1"), SettingScope::Session).unwrap();
        let b = change(&a, "app.n", Some("l1"), SettingScope::Transaction).unwrap();
        let c = change(&b, "app.n", Some("l2"), SettingScope::Transaction).unwrap();
        assert_eq!(journal_len(), 2);
        let mut synced = NEVER_SYNCED;
        let effective = effective_if_changed(&mut synced).unwrap();
        assert_eq!(effective.get("app.n").unwrap(), "l2");
        change(&c, "app.n", Some("s2"), SettingScope::Session).unwrap();
        assert_eq!(
            take_for_session(true),
            vec![SettingChange {
                name: "app.n".into(),
                value: Some("s2".into()),
                scope: SettingScope::Session,
            }]
        );
    }

    #[test]
    fn calls_of_a_routine_with_a_clause_leave_no_entries_behind() {
        let _ = take_for_session(true);
        let base = Settings::default();
        for _ in 0..10_000 {
            let (scope, _) = enter_clause(&base, &[("app.mode".into(), "inner".into())])
                .unwrap()
                .unwrap();
            leave_clause(scope);
        }
        assert_eq!(journal_len(), 0);
        assert!(take_for_session(true).is_empty());
    }

    #[test]
    fn a_procedure_commit_loop_keeps_one_committed_entry_per_name() {
        let _ = take_for_session(true);
        let mut current = Settings::default();
        for round in 0..10_000 {
            let value = round.to_string();
            current = change(&current, "app.n", Some(&value), SettingScope::Session).unwrap();
            current = change(&current, "app.l", Some(&value), SettingScope::Transaction).unwrap();
            end_procedure_transaction(true);
        }
        assert_eq!(journal_len(), 1);
        change(&current, "app.n", Some("lost"), SettingScope::Session).unwrap();
        end_procedure_transaction(false);
        assert_eq!(
            take_for_session(false),
            vec![SettingChange {
                name: "app.n".into(),
                value: Some("9999".into()),
                scope: SettingScope::Session,
            }]
        );
    }
}
