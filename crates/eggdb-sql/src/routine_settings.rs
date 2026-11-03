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

#[derive(Default)]
struct Journal {
    root: Option<Settings>,
    entries: Vec<JournalEntry>,
}

thread_local! {
    static JOURNAL: RefCell<Journal> = RefCell::new(Journal::default());
    static VERSION: Cell<u64> = const { Cell::new(0) };
}

pub(crate) const NEVER_SYNCED: u64 = u64::MAX;

fn bump() {
    VERSION.with(|version| version.set(version.get().wrapping_add(1) % NEVER_SYNCED));
}

fn record(current: &Settings, name: &str, value: Option<&str>, lasts: Lasts) {
    JOURNAL.with(|journal| {
        let Ok(mut journal) = journal.try_borrow_mut() else {
            return;
        };
        if journal.entries.is_empty() {
            journal.root = Some(current.clone());
        }
        journal.entries.push(JournalEntry {
            name: Settings::name_of(name),
            value: value.map(str::to_owned),
            lasts,
            active: true,
            committed: false,
        });
    });
    bump();
}

fn with_entries(change: impl FnOnce(&mut Vec<JournalEntry>)) {
    JOURNAL.with(|journal| {
        if let Ok(mut journal) = journal.try_borrow_mut() {
            change(&mut journal.entries);
        }
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
    let start = mark();
    for (name, value) in clause {
        record(current, name, Some(value.as_str()), Lasts::Routine);
    }
    let scope = ClauseScope {
        start,
        count: clause.len(),
        names: clause
            .iter()
            .map(|(name, _)| Settings::name_of(name))
            .collect(),
    };
    Ok(Some((scope, next)))
}

pub(crate) fn leave_clause(scope: ClauseScope) {
    let own_end = scope.start.saturating_add(scope.count);
    with_entries(|entries| {
        for (at, entry) in entries.iter_mut().enumerate().skip(scope.start) {
            let restored = at < own_end
                || (entry.lasts == Lasts::Transaction && scope.names.contains(&entry.name));
            if restored {
                entry.active = false;
            }
        }
    });
}

pub(crate) fn mark() -> usize {
    JOURNAL.with(|journal| {
        journal
            .try_borrow()
            .map_or(0, |journal| journal.entries.len())
    })
}

pub(crate) fn undo_since(at: usize) {
    if mark() <= at {
        return;
    }
    with_entries(|entries| {
        for entry in entries.iter_mut().skip(at) {
            entry.active = false;
        }
    });
}

pub(crate) fn end_procedure_transaction(committed: bool) {
    if mark() == 0 {
        return;
    }
    with_entries(|entries| {
        for entry in entries.iter_mut().filter(|entry| entry.active) {
            match entry.lasts {
                Lasts::Transaction => entry.active = false,
                Lasts::Session if committed => entry.committed = true,
                Lasts::Session if !entry.committed => entry.active = false,
                Lasts::Session | Lasts::Routine => {}
            }
        }
    });
}

pub(crate) fn effective_if_changed(synced: &mut u64) -> Option<Settings> {
    let version = VERSION.with(Cell::get);
    if *synced == version {
        return None;
    }
    *synced = version;
    JOURNAL.with(|journal| {
        let journal = journal.try_borrow().ok()?;
        if journal.entries.is_empty() {
            return None;
        }
        let mut effective = journal.root.clone()?;
        for entry in journal.entries.iter().filter(|entry| entry.active) {
            let _ = effective.set(&entry.name, entry.value.as_deref());
        }
        Some(effective)
    })
}

pub(crate) fn take_for_session(succeeded: bool) -> Vec<SettingChange> {
    let taken = JOURNAL.with(|journal| {
        journal
            .try_borrow_mut()
            .map(|mut journal| {
                journal.root = None;
                std::mem::take(&mut journal.entries)
            })
            .unwrap_or_default()
    });
    if taken.is_empty() {
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
        assert!(take_for_session(true).is_empty());
    }

    #[test]
    fn an_invalid_value_changes_nothing() {
        let _ = take_for_session(true);
        let base = Settings::default();
        assert!(change(&base, "server_version", Some("1"), SettingScope::Session).is_err());
        assert_eq!(mark(), 0);
    }
}
