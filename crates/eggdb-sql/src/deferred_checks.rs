use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use std::cell::RefCell;

const MAX_PENDING: usize = 1 << 22;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PendingCheck {
    ParentOfChild {
        child: String,
        constraint: String,
        values: Vec<Value>,
    },
    ChildrenOfParent {
        parent: String,
        child: String,
        constraint: String,
        values: Vec<Value>,
    },
}

impl PendingCheck {
    pub(crate) fn constraint(&self) -> &str {
        match self {
            Self::ParentOfChild { constraint, .. } | Self::ChildrenOfParent { constraint, .. } => {
                constraint
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DeferralModes {
    all: Option<bool>,
    named: Vec<(String, bool)>,
}

impl DeferralModes {
    pub(crate) fn deferred(&self, constraint: &str, initially_deferred: bool) -> bool {
        self.named
            .iter()
            .rev()
            .find(|(name, _)| name == constraint)
            .map(|(_, deferred)| *deferred)
            .or(self.all)
            .unwrap_or(initially_deferred)
    }

    pub(crate) fn set(&mut self, names: Option<&[String]>, deferred: bool) {
        match names {
            None => {
                self.all = Some(deferred);
                self.named.clear();
            }
            Some(names) => {
                for name in names {
                    self.named.retain(|(known, _)| known != name);
                    self.named.push((name.clone(), deferred));
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct DeferredState {
    pub(crate) modes: DeferralModes,
    pub(crate) pending: Vec<PendingCheck>,
}

impl DeferredState {
    pub(crate) fn clear(&mut self) {
        self.modes = DeferralModes::default();
        self.pending.clear();
    }

    pub(crate) fn add(&mut self, checks: Vec<PendingCheck>) -> SqlResult<()> {
        if self.pending.len().saturating_add(checks.len()) > MAX_PENDING {
            return Err(too_many());
        }
        self.pending.extend(checks);
        Ok(())
    }

    pub(crate) fn take_for(&mut self, names: Option<&[String]>) -> Vec<PendingCheck> {
        let (taken, kept): (Vec<PendingCheck>, Vec<PendingCheck>) =
            std::mem::take(&mut self.pending)
                .into_iter()
                .partition(|check| {
                    names.is_none_or(|names| names.iter().any(|name| name == check.constraint()))
                });
        self.pending = kept;
        taken
    }
}

fn too_many() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a transaction may defer at most {MAX_PENDING} foreign key checks"),
    )
    .with_hint("Commit sooner, or run SET CONSTRAINTS ALL IMMEDIATE to check what is pending.")
}

struct Frame {
    modes: DeferralModes,
    pending: Vec<PendingCheck>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn enter(modes: &DeferralModes) {
    FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut() {
            frames.push(Frame {
                modes: modes.clone(),
                pending: Vec::new(),
            });
        }
    });
}

pub(crate) fn leave() -> Vec<PendingCheck> {
    FRAMES.with(|frames| {
        frames
            .try_borrow_mut()
            .ok()
            .and_then(|mut frames| frames.pop())
            .map(|frame| frame.pending)
            .unwrap_or_default()
    })
}

pub(crate) fn take_statement_pending() -> Vec<PendingCheck> {
    FRAMES.with(|frames| {
        frames
            .try_borrow_mut()
            .ok()
            .and_then(|mut frames| {
                frames
                    .last_mut()
                    .map(|frame| std::mem::take(&mut frame.pending))
            })
            .unwrap_or_default()
    })
}

pub(crate) fn is_deferred(constraint: &str, initially_deferred: bool) -> bool {
    FRAMES.with(|frames| {
        frames.try_borrow().ok().is_some_and(|frames| {
            frames
                .last()
                .is_some_and(|frame| frame.modes.deferred(constraint, initially_deferred))
        })
    })
}

pub(crate) fn defer(check: PendingCheck) -> SqlResult<()> {
    FRAMES.with(|frames| {
        let Ok(mut frames) = frames.try_borrow_mut() else {
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a deferred foreign key check could not be recorded",
            ));
        };
        let Some(frame) = frames.last_mut() else {
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a deferred foreign key check outside a statement",
            ));
        };
        if frame.pending.len() >= MAX_PENDING {
            return Err(too_many());
        }
        frame.pending.push(check);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{DeferralModes, DeferredState, PendingCheck};
    use crate::types::Value;

    #[test]
    fn named_settings_override_all_which_overrides_the_declaration() {
        let mut modes = DeferralModes::default();
        assert!(modes.deferred("fk", true));
        assert!(!modes.deferred("fk", false));
        modes.set(None, true);
        assert!(modes.deferred("fk", false));
        modes.set(Some(&["fk".to_owned()]), false);
        assert!(!modes.deferred("fk", true));
        assert!(modes.deferred("other", false));
        modes.set(None, false);
        assert!(!modes.deferred("fk", true));
    }

    #[test]
    fn checks_are_taken_by_constraint() {
        let mut state = DeferredState::default();
        let check = |name: &str| PendingCheck::ParentOfChild {
            child: "c".into(),
            constraint: name.into(),
            values: vec![Value::Integer(1)],
        };
        state.add(vec![check("a"), check("b"), check("a")]).unwrap();
        let taken = state.take_for(Some(&["a".to_owned()]));
        assert_eq!(taken.len(), 2);
        assert_eq!(state.pending, vec![check("b")]);
        assert_eq!(state.take_for(None).len(), 1);
        assert!(state.pending.is_empty());
    }
}
