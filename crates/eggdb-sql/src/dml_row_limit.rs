use std::cell::Cell;

thread_local! {
    static LIMIT: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Restores the row limit that was in force before [`limit`] when the statement ends.
pub(crate) struct Limited {
    previous: Option<u64>,
}

impl Drop for Limited {
    fn drop(&mut self) {
        LIMIT.with(|held| held.set(self.previous));
    }
}

/// Limits the next `UPDATE` or `DELETE` this thread runs to `count` rows, as Transact-SQL's
/// `TOP (n)` asks, until the returned guard drops.
pub(crate) fn limit(count: Option<u64>) -> Limited {
    Limited {
        previous: LIMIT.with(|held| held.replace(count)),
    }
}

/// The row limit for the statement starting now, cleared so the statements it runs in turn
/// (triggers, cascades) are not limited too.
pub(crate) fn take() -> Option<u64> {
    LIMIT.with(Cell::take)
}
