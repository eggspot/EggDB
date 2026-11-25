use std::cell::{Cell, RefCell};
use eggdb_platform_core::time::Instant;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowTimes {
    pub(crate) first_row: Duration,
    pub(crate) last_row: Duration,
    pub(crate) rows: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MeasuredSelect {
    pub(crate) source: Option<RowTimes>,
    pub(crate) output: RowTimes,
    pub(crate) skipped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MeasuredSubplan {
    pub(crate) position: usize,
    pub(crate) loops: u64,
    pub(crate) rows: u64,
    pub(crate) total: Duration,
    /// Whether this is the scan inside the subplan (D1363) rather than the subplan itself.
    pub(crate) scan: bool,
}

/// What one level of a join saw (D1350): the rows that reached it, after the filters that
/// apply there, and when the first and the last arrived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MeasuredLevel {
    pub(crate) rows: u64,
    pub(crate) first_row: Option<Duration>,
    pub(crate) last_row: Option<Duration>,
}

struct Recording {
    started: Instant,
    selects: Vec<MeasuredSelect>,
    subplans: Vec<MeasuredSubplan>,
    joins: Vec<Vec<MeasuredLevel>>,
    join_claimed: bool,
    metering: bool,
    running_subplan: Option<usize>,
    subplan_scan_claimed: bool,
}

thread_local! {
    static RECORDING: RefCell<Option<Recording>> = const { RefCell::new(None) };
    static FINISHED_JOINS: RefCell<Vec<Vec<MeasuredLevel>>> = const { RefCell::new(Vec::new()) };
}

/// The join levels the last recording measured, outermost join first.
pub(crate) fn take_joins() -> Vec<Vec<MeasuredLevel>> {
    FINISHED_JOINS
        .try_with(|finished| finished.try_borrow_mut().map(|mut finished| std::mem::take(&mut *finished)).unwrap_or_default())
        .unwrap_or_default()
}

/// Counts the rows reaching each level of the first join a recorded statement runs.
pub(crate) struct JoinMeter {
    started: Instant,
    levels: RefCell<Vec<MeasuredLevel>>,
}

pub(crate) fn claim_join(levels: usize) -> Option<JoinMeter> {
    let mut claimed = None;
    with_recording(|recording| {
        if !recording.join_claimed {
            recording.join_claimed = true;
            claimed = Some(JoinMeter {
                started: recording.started,
                levels: RefCell::new(vec![MeasuredLevel::default(); levels]),
            });
        }
    });
    claimed
}

impl JoinMeter {
    pub(crate) fn arrived(&self, level: usize) {
        if let Ok(mut levels) = self.levels.try_borrow_mut()
            && let Some(found) = levels.get_mut(level)
        {
            let now = self.started.elapsed();
            found.rows = found.rows.saturating_add(1);
            found.first_row.get_or_insert(now);
            found.last_row = Some(now);
        }
    }
}

impl Drop for JoinMeter {
    fn drop(&mut self) {
        let levels = std::mem::take(self.levels.get_mut());
        with_recording(|recording| recording.joins.push(levels));
    }
}

fn replace_recording(next: Option<Recording>) -> Option<Recording> {
    RECORDING
        .try_with(|recording| {
            recording
                .try_borrow_mut()
                .ok()
                .and_then(|mut current| std::mem::replace(&mut *current, next))
        })
        .ok()
        .flatten()
}

fn with_recording(change: impl FnOnce(&mut Recording)) {
    let _ = RECORDING.try_with(|recording| {
        if let Ok(mut borrowed) = recording.try_borrow_mut()
            && let Some(current) = Option::as_mut(&mut *borrowed)
        {
            change(current);
        }
    });
}

#[cfg(test)]
pub(crate) fn record<T>(run: impl FnOnce() -> T) -> (T, Vec<MeasuredSelect>) {
    let (result, selects, _) = record_with_subplans(run);
    (result, selects)
}

pub(crate) fn record_with_subplans<T>(
    run: impl FnOnce() -> T,
) -> (T, Vec<MeasuredSelect>, Vec<MeasuredSubplan>) {
    let outer = replace_recording(Some(Recording {
        started: Instant::now(),
        selects: Vec::new(),
        subplans: Vec::new(),
        joins: Vec::new(),
        join_claimed: false,
        metering: false,
        running_subplan: None,
        subplan_scan_claimed: false,
    }));
    let result = run();
    match replace_recording(outer) {
        Some(recorded) => {
            let _ = FINISHED_JOINS.try_with(|finished| {
                if let Ok(mut finished) = finished.try_borrow_mut() {
                    *finished = recorded.joins;
                }
            });
            (result, recorded.selects, recorded.subplans)
        }
        None => (result, Vec::new(), Vec::new()),
    }
}

pub(crate) fn measure_subplan<T, E>(
    position: usize,
    run: impl FnOnce() -> Result<T, E>,
    rows_of: impl FnOnce(&T) -> usize,
) -> Result<T, E> {
    let mut outer = None;
    with_recording(|recording| {
        outer = Some((recording.metering, recording.running_subplan, recording.subplan_scan_claimed));
        recording.metering = true;
        recording.running_subplan = Some(position);
        recording.subplan_scan_claimed = false;
    });
    let Some((outer_metering, outer_subplan, outer_claimed)) = outer else {
        return run();
    };
    let started = Instant::now();
    let result = run();
    let elapsed = started.elapsed();
    let rows = row_count(result.as_ref().map_or(0, rows_of));
    with_recording(|recording| {
        recording.metering = outer_metering;
        recording.running_subplan = outer_subplan;
        recording.subplan_scan_claimed = outer_claimed;
        match recording
            .subplans
            .iter_mut()
            .find(|subplan| subplan.position == position && !subplan.scan)
        {
            Some(found) => {
                found.loops = found.loops.saturating_add(1);
                found.rows = found.rows.saturating_add(rows);
                found.total = found.total.saturating_add(elapsed);
            }
            None => recording.subplans.push(MeasuredSubplan {
                position,
                loops: 1,
                rows,
                total: elapsed,
                scan: false,
            }),
        }
    });
    result
}

pub(crate) fn unmeasured<T>(run: impl FnOnce() -> T) -> T {
    let mut outer_metering = None;
    with_recording(|recording| {
        outer_metering = Some(recording.metering);
        recording.metering = true;
    });
    let result = run();
    if let Some(outer_metering) = outer_metering {
        with_recording(|recording| recording.metering = outer_metering);
    }
    result
}

pub(crate) struct SelectMeter {
    started: Instant,
    first_row: Cell<Option<Duration>>,
    last_row: Cell<Option<Duration>>,
    rows: Cell<u64>,
    subplan: Option<usize>,
}

pub(crate) fn claim() -> Option<SelectMeter> {
    let mut claimed = None;
    with_recording(|recording| {
        if !recording.metering {
            recording.metering = true;
            claimed = Some(SelectMeter {
                started: recording.started,
                first_row: Cell::new(None),
                last_row: Cell::new(None),
                rows: Cell::new(0),
                subplan: None,
            });
        } else if let Some(position) = recording.running_subplan
            && !recording.subplan_scan_claimed
        {
            recording.subplan_scan_claimed = true;
            claimed = Some(SelectMeter {
                started: Instant::now(),
                first_row: Cell::new(None),
                last_row: Cell::new(None),
                rows: Cell::new(0),
                subplan: Some(position),
            });
        }
    });
    claimed
}

fn row_count(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

impl SelectMeter {
    fn since_start(&self) -> Duration {
        self.started.elapsed()
    }

    pub(crate) fn source_row(&self) {
        self.rows.set(self.rows.get().saturating_add(1));
        if self.first_row.get().is_none() {
            self.first_row.set(Some(self.since_start()));
        }
    }

    pub(crate) fn source_read(&self) {
        let now = self.since_start();
        if self.first_row.get().is_none() {
            self.first_row.set(Some(now));
        }
        self.last_row.set(Some(now));
    }

    pub(crate) fn finish(&self, output_rows: usize, skipped: usize) {
        let finished = self.since_start();
        if let Some(position) = self.subplan {
            let rows = self.rows.get();
            let total = self.last_row.get().unwrap_or(finished);
            with_recording(|recording| {
                match recording
                    .subplans
                    .iter_mut()
                    .find(|subplan| subplan.position == position && subplan.scan)
                {
                    Some(found) => {
                        found.loops = found.loops.saturating_add(1);
                        found.rows = found.rows.saturating_add(rows);
                        found.total = found.total.saturating_add(total);
                    }
                    None => recording.subplans.push(MeasuredSubplan {
                        position,
                        loops: 1,
                        rows,
                        total,
                        scan: true,
                    }),
                }
            });
            return;
        }
        let source = self.last_row.get().map(|last_row| RowTimes {
            first_row: self.first_row.get().unwrap_or(last_row).min(last_row),
            last_row,
            rows: self.rows.get(),
        });
        let measured = MeasuredSelect {
            source,
            output: RowTimes {
                first_row: source.map_or(finished, |source| source.first_row),
                last_row: finished,
                rows: row_count(output_rows),
            },
            skipped: row_count(skipped),
        };
        with_recording(|recording| recording.selects.push(measured));
    }
}

impl Drop for SelectMeter {
    fn drop(&mut self) {
        if self.subplan.is_some() {
            return;
        }
        with_recording(|recording| recording.metering = false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_claimed_outside_a_recording() {
        assert!(claim().is_none());
    }

    #[test]
    fn a_recording_keeps_one_meter_at_a_time_in_claim_order() {
        let ((), selects) = record(|| {
            let first = claim().expect("the first select is metered");
            assert!(claim().is_none(), "a select inside it is not");
            first.source_row();
            first.source_row();
            first.source_read();
            first.finish(1, 1);
            drop(first);
            let second = claim().expect("the next select is metered");
            second.finish(0, 0);
        });
        assert_eq!(selects.len(), 2);
        let source = selects[0].source.expect("the first read its source");
        assert_eq!(source.rows, 2);
        assert!(source.first_row <= source.last_row);
        assert_eq!(selects[0].output.rows, 1);
        assert_eq!(selects[0].skipped, 1);
        assert!(selects[0].output.last_row >= source.last_row);
        assert_eq!(selects[1].source, None);
        assert!(claim().is_none(), "the recording ended");
    }

    #[test]
    fn a_subplan_adds_up_its_loops_and_rows_and_meters_its_own_scan_apart() {
        let ((), selects, subplans) = record_with_subplans(|| {
            for rows in [2usize, 3] {
                let measured: Result<usize, ()> = measure_subplan(
                    7,
                    || {
                        let scan = claim().expect("the subplan's scan is metered");
                        assert!(claim().is_none(), "one scan per loop");
                        for _ in 0..rows * 2 {
                            scan.source_row();
                        }
                        scan.source_read();
                        scan.finish(rows, 0);
                        Ok(rows)
                    },
                    |rows| *rows,
                );
                assert_eq!(measured, Ok(rows));
            }
            let outer = claim().expect("the statement's own select is metered after its subplans");
            outer.finish(1, 0);
        });
        assert_eq!(selects.len(), 1);
        let subplan = subplans.iter().find(|subplan| !subplan.scan).expect("the subplan");
        assert_eq!((subplan.position, subplan.loops, subplan.rows), (7, 2, 5));
        let scan = subplans.iter().find(|subplan| subplan.scan).expect("its scan");
        assert_eq!((scan.position, scan.loops, scan.rows), (7, 2, 10));
    }

    #[test]
    fn a_subplan_outside_a_recording_only_runs() {
        let measured: Result<usize, ()> = measure_subplan(1, || Ok(4), |rows| *rows);
        assert_eq!(measured, Ok(4));
        assert!(claim().is_none());
    }

    #[test]
    fn a_nested_recording_leaves_the_outer_one_as_it_was() {
        let ((), outer) = record(|| {
            let ((), inner) = record(|| {
                if let Some(meter) = claim() {
                    meter.finish(3, 0);
                }
            });
            assert_eq!(inner.len(), 1);
            if let Some(meter) = claim() {
                meter.finish(5, 0);
            }
        });
        assert_eq!(outer.len(), 1);
        assert_eq!(outer[0].output.rows, 5);
    }
}
