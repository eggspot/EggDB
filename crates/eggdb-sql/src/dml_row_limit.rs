use std::cell::Cell;

thread_local! {
    static LIMIT: Cell<Option<DmlRowLimit>> = const { Cell::new(None) };
}

/// How many rows Transact-SQL's `TOP` lets an `INSERT`, `UPDATE` or `DELETE` change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DmlRowLimit {
    Rows(u64),
    Percent(f64),
}

impl DmlRowLimit {
    /// The number of rows allowed out of `candidates`; a percentage rounds up, as SQL Server's
    /// does.
    pub(crate) fn of(self, candidates: usize) -> u64 {
        match self {
            Self::Rows(rows) => rows,
            Self::Percent(percent) => {
                let wanted = (percent * candidates as f64 / 100.0).ceil();
                if wanted.is_nan() || wanted <= 0.0 {
                    0
                } else if wanted >= candidates as f64 {
                    u64::try_from(candidates).unwrap_or(u64::MAX)
                } else {
                    wanted as u64
                }
            }
        }
    }

    pub(crate) fn of_usize(self, candidates: usize) -> usize {
        usize::try_from(self.of(candidates)).unwrap_or(usize::MAX)
    }
}

/// Restores the row limit that was in force before [`limit`] when the statement ends.
pub(crate) struct Limited {
    previous: Option<DmlRowLimit>,
}

impl Drop for Limited {
    fn drop(&mut self) {
        LIMIT.with(|held| held.set(self.previous));
    }
}

/// Limits the next `INSERT`, `UPDATE` or `DELETE` this thread runs as Transact-SQL's `TOP`
/// asks, until the returned guard drops.
pub(crate) fn limit(limit: Option<DmlRowLimit>) -> Limited {
    Limited {
        previous: LIMIT.with(|held| held.replace(limit)),
    }
}

/// The row limit for the statement starting now, cleared so the statements it runs in turn
/// (triggers, cascades) are not limited too.
pub(crate) fn take() -> Option<DmlRowLimit> {
    LIMIT.with(Cell::take)
}
