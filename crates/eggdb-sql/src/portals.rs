use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::{OutputColumn, QueryResult};
use crate::types::Value;
use std::cell::{Cell, RefCell};

pub(crate) const MAX_OPEN_PORTALS: usize = 1_000;

thread_local! {
    static OPEN: RefCell<Portals> = RefCell::new(Portals::default());
    static PROCEDURE_TRANSACTIONS_ENDED: Cell<u64> = const { Cell::new(0) };
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Portal {
    pub(crate) name: String,
    pub(crate) columns: Vec<OutputColumn>,
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) position: usize,
    pub(crate) scroll: Option<bool>,
    pub(crate) holdable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortalDirection {
    Next,
    Prior,
    First,
    Last,
    Absolute(i64),
    Relative(i64),
    Forward(i64),
    ForwardAll,
    Backward(i64),
    BackwardAll,
}

#[derive(Debug, Default)]
pub(crate) struct Portals {
    open: Vec<Portal>,
    unnamed: u64,
}

impl Portals {
    pub(crate) fn close_at_transaction_end(&mut self) {
        self.open.retain(|portal| portal.holdable);
    }

    pub(crate) fn close_all(&mut self) {
        self.open.clear();
    }

    pub(crate) fn columns_of(&self, name: &str) -> Option<Vec<OutputColumn>> {
        self.open
            .iter()
            .find(|portal| portal.name == name)
            .map(|portal| portal.columns.clone())
    }
}

pub(crate) fn install(portals: Portals) {
    OPEN.with(|open| *open.borrow_mut() = portals);
}

pub(crate) fn take() -> Portals {
    OPEN.with(|open| std::mem::take(&mut *open.borrow_mut()))
}

pub(crate) fn transactions_ended() -> u64 {
    PROCEDURE_TRANSACTIONS_ENDED.with(Cell::get)
}

pub(crate) fn end_procedure_transaction() {
    OPEN.with(|open| open.borrow_mut().close_at_transaction_end());
    PROCEDURE_TRANSACTIONS_ENDED.with(|ended| ended.set(ended.get().wrapping_add(1)));
}

pub(crate) fn next_unnamed_name() -> String {
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        open.unnamed += 1;
        format!("<unnamed portal {}>", open.unnamed)
    })
}

pub(crate) fn is_open(name: &str) -> bool {
    OPEN.with(|open| open.borrow().open.iter().any(|portal| portal.name == name))
}

pub(crate) fn open_count() -> usize {
    OPEN.with(|open| open.borrow().open.len())
}

pub(crate) fn hand_over(portal: Portal) {
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        open.open.retain(|found| found.name != portal.name);
        open.open.push(portal);
    });
}

pub(crate) fn adopt(name: &str) -> Option<Portal> {
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        let at = open.open.iter().position(|portal| portal.name == name)?;
        Some(open.open.remove(at))
    })
}

fn missing(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_CURSOR_NAME,
        format!("cursor \"{name}\" does not exist"),
    )
}

fn too_many() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a session keeps at most {MAX_OPEN_PORTALS} cursors open"),
    )
}

pub(crate) fn declare(portal: Portal) -> SqlResult<()> {
    if is_open(&portal.name) {
        return Err(SqlError::new(
            SqlState::DUPLICATE_CURSOR,
            format!("cursor \"{}\" already exists", portal.name),
        ));
    }
    if open_count() >= MAX_OPEN_PORTALS {
        return Err(too_many());
    }
    hand_over(portal);
    Ok(())
}

pub(crate) fn close(name: Option<&str>) -> SqlResult<QueryResult> {
    let Some(name) = name else {
        OPEN.with(|open| open.borrow_mut().close_all());
        return Ok(QueryResult::command("CLOSE CURSOR ALL"));
    };
    adopt(name).ok_or_else(|| missing(name))?;
    Ok(QueryResult::command("CLOSE CURSOR"))
}

pub(crate) fn fetch(
    name: &str,
    direction: PortalDirection,
    only_move: bool,
) -> SqlResult<QueryResult> {
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        let portal = open
            .open
            .iter_mut()
            .find(|portal| portal.name == name)
            .ok_or_else(|| missing(name))?;
        let rows = portal.moved(direction)?;
        if only_move {
            return Ok(QueryResult::command(format!("MOVE {}", rows.len())));
        }
        let mut result = QueryResult::command(format!("FETCH {}", rows.len()));
        result.columns = Some(portal.columns.clone());
        result.rows = rows
            .into_iter()
            .filter_map(|at| portal.rows.get(at).cloned())
            .collect();
        Ok(result)
    })
}

impl Portal {
    fn length(&self) -> i64 {
        i64::try_from(self.rows.len()).unwrap_or(i64::MAX - 1)
    }

    fn row_at(&self, position: i64) -> Option<usize> {
        if (1..=self.length()).contains(&position) {
            usize::try_from(position - 1).ok()
        } else {
            None
        }
    }

    fn place(&mut self, position: i64) {
        let clamped = position.clamp(0, self.length().saturating_add(1));
        self.position = usize::try_from(clamped).unwrap_or(0);
    }

    fn refuse_backward(&self) -> SqlResult<()> {
        if self.scroll == Some(false) {
            return Err(SqlError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cursor can only scan forward",
            )
            .with_hint("Declare it with SCROLL option to enable backward scan."));
        }
        Ok(())
    }

    fn moved(&mut self, direction: PortalDirection) -> SqlResult<Vec<usize>> {
        let position = i64::try_from(self.position).unwrap_or(0);
        let length = self.length();
        match direction {
            PortalDirection::Next => self.stepped(1),
            PortalDirection::Prior => self.stepped(-1),
            PortalDirection::Forward(count) => self.stepped(count),
            PortalDirection::Backward(count) => self.stepped(count.saturating_neg()),
            PortalDirection::ForwardAll => self.stepped(i64::MAX),
            PortalDirection::BackwardAll => self.stepped(i64::MIN + 1),
            PortalDirection::First => self.jumped(1),
            PortalDirection::Last => self.jumped(length),
            PortalDirection::Absolute(target) if target >= 0 => self.jumped(target),
            PortalDirection::Absolute(target) => {
                self.jumped(length.saturating_add(1).saturating_add(target))
            }
            PortalDirection::Relative(offset) => self.jumped(position.saturating_add(offset)),
        }
    }

    fn jumped(&mut self, target: i64) -> SqlResult<Vec<usize>> {
        if target < i64::try_from(self.position).unwrap_or(0) {
            self.refuse_backward()?;
        }
        self.place(target);
        let position = i64::try_from(self.position).unwrap_or(0);
        Ok(self.row_at(position).into_iter().collect())
    }

    fn stepped(&mut self, count: i64) -> SqlResult<Vec<usize>> {
        let mut position = i64::try_from(self.position).unwrap_or(0);
        if count == 0 {
            return Ok(self.row_at(position).into_iter().collect());
        }
        if count < 0 {
            self.refuse_backward()?;
        }
        let step = count.signum();
        let wanted = count.unsigned_abs();
        let mut rows = Vec::new();
        let mut taken = 0u64;
        while taken < wanted {
            crate::cancel::check()?;
            position += step;
            match self.row_at(position) {
                Some(at) => rows.push(at),
                None => break,
            }
            taken += 1;
        }
        self.place(position);
        Ok(rows)
    }
}
