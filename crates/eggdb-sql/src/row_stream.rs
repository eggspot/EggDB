//! Rows handed on as a query produces them (D1340): [`Session::execute_streamed`] arms a sink
//! for the one table its query reads, and the scan of that table, the outermost one, claims it
//! and sends each row instead of collecting it, so a large result never sits in memory whole.
//!
//! [`Session::execute_streamed`]: crate::Session::execute_streamed

use crate::error::SqlResult;
use crate::types::Value;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::ptr::NonNull;

type Sink = dyn FnMut(&[Value]) -> SqlResult<bool>;

struct Armed {
    table: String,
    sink: NonNull<Sink>,
}

thread_local! {
    static ARMED: RefCell<Option<Armed>> = const { RefCell::new(None) };
    static CLAIMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Keeps a sink armed while a statement runs; dropping it disarms the sink.
pub(crate) struct ArmedSink<'s> {
    borrowed: PhantomData<&'s mut Sink>,
}

impl ArmedSink<'_> {
    /// Whether a scan took the sink and sent its rows to it.
    pub(crate) fn was_claimed(&self) -> bool {
        CLAIMED.with(std::cell::Cell::get)
    }
}

impl Drop for ArmedSink<'_> {
    fn drop(&mut self) {
        ARMED.with(|armed| armed.borrow_mut().take());
        CLAIMED.with(|claimed| claimed.set(false));
    }
}

fn key(table: &str) -> String {
    crate::schemas::base_name(table).to_ascii_lowercase()
}

/// Arms `sink` for the scan of `table` until the returned guard drops.
pub(crate) fn arm<'s>(table: &str, sink: &'s mut (dyn FnMut(&[Value]) -> SqlResult<bool> + 's)) -> ArmedSink<'s> {
    let borrowed: NonNull<dyn FnMut(&[Value]) -> SqlResult<bool> + 's> = NonNull::from(sink);
    // SAFETY: only the lifetime bound changes; the pointer is used only while the guard, which borrows the sink for 's, is alive (see `Claim::send`).
    let erased: NonNull<Sink> = unsafe { std::mem::transmute(borrowed) };
    ARMED.with(|armed| *armed.borrow_mut() = Some(Armed { table: key(table), sink: erased }));
    CLAIMED.with(|claimed| claimed.set(false));
    ArmedSink { borrowed: PhantomData }
}

/// The armed sink, taken by the scan of `table` when it is the table the sink waits for.
pub(crate) struct Claim {
    sink: NonNull<Sink>,
}

/// Takes the sink when one is armed for `table`; later scans, nested in this one's rows,
/// find none.
pub(crate) fn claim(table: &str) -> Option<Claim> {
    ARMED.with(|armed| {
        let mut armed = armed.borrow_mut();
        if armed.as_ref().is_some_and(|found| found.table == key(table)) {
            CLAIMED.with(|claimed| claimed.set(true));
            armed.take().map(|found| Claim { sink: found.sink })
        } else {
            None
        }
    })
}

impl Claim {
    /// Sends `row`; `false` asks the scan to stop.
    pub(crate) fn send(&mut self, row: &[Value]) -> SqlResult<bool> {
        // SAFETY: the sink outlives this claim: it was armed by an `ArmedSink` that borrows it and stays alive while the statement that made this claim runs on this thread, and only this claim holds the pointer.
        let sink = unsafe { self.sink.as_mut() };
        sink(row)
    }
}

fn plain(expr: &crate::ast::Expr) -> bool {
    use crate::ast::ExprKind;
    !matches!(expr.kind, ExprKind::Subquery(_) | ExprKind::Aggregate { .. } | ExprKind::Window { .. })
        && crate::eval::children(expr).into_iter().all(plain)
}

/// The table a query reads when it can stream: one table, no join, grouping, sorting,
/// `DISTINCT`, locking, subqueries, aggregates or window functions.
pub(crate) fn streamable(select: &crate::ast::Select) -> Option<String> {
    let from = select.from.as_ref()?;
    let first = &from.first;
    let simple = !select.distinct
        && select.distinct_on.is_empty()
        && select.group_by.is_empty()
        && select.grouping_sets.is_none()
        && select.having.is_none()
        && select.order_by.is_empty()
        && select.locking.is_none()
        && from.joins.is_empty()
        && first.derived.is_none()
        && first.generated.is_none()
        && first.partition_selection.is_none()
        && first.system_time.is_none()
        && select.filter.as_ref().is_none_or(plain)
        && select.items.iter().all(|item| match item {
            crate::ast::SelectItem::Expr { expr, .. } => plain(expr),
            _ => true,
        });
    simple.then(|| first.name.text.clone())
}

#[cfg(test)]
mod tests {
    use crate::Session;
    use crate::memory_store::MemoryStore;
    use crate::types::Value;

    fn streamed(session: &mut Session<'_>, sql: &str, stop_after: usize) -> (Vec<String>, Vec<Vec<Value>>, Vec<String>) {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let mut tags = Vec::new();
        session
            .execute_streamed(
                sql,
                &mut |found| columns.extend(found.iter().map(|column| column.name.clone())),
                &mut |row| {
                    rows.push(row.to_vec());
                    Ok(rows.len() < stop_after)
                },
                &mut |result| {
                    assert!(result.rows.is_empty());
                    tags.push(result.tag);
                },
            )
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        (columns, rows, tags)
    }

    #[test]
    fn a_plain_scan_streams_its_rows_and_others_send_theirs_at_the_end() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        session
            .execute(
                "CREATE TABLE items (id integer PRIMARY KEY, name text);
                 INSERT INTO items SELECT n, 'item ' || n FROM generate_series(1, 1000) AS n",
                &mut |_| {},
            )
            .unwrap();
        let (columns, rows, tags) = streamed(&mut session, "SELECT id, name FROM items WHERE id % 2 = 0", usize::MAX);
        assert_eq!(columns, ["id", "name"]);
        assert_eq!(rows.len(), 500);
        assert_eq!(rows[0], [Value::Integer(2), Value::Text("item 2".into())]);
        assert_eq!(tags, ["SELECT 500"]);
        let (_, rows, tags) = streamed(&mut session, "SELECT id FROM items ORDER BY id DESC LIMIT 3", usize::MAX);
        assert_eq!(rows, [[Value::Integer(1000)], [Value::Integer(999)], [Value::Integer(998)]]);
        assert_eq!(tags, ["SELECT 3"]);
        let (_, rows, _) = streamed(&mut session, "SELECT id FROM items", 10);
        assert_eq!(rows.len(), 10);
    }
}
