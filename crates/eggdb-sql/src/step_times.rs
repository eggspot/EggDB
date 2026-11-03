use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

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

struct Recording {
    started: Instant,
    selects: Vec<MeasuredSelect>,
    metering: bool,
}

thread_local! {
    static RECORDING: RefCell<Option<Recording>> = const { RefCell::new(None) };
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

pub(crate) fn record<T>(run: impl FnOnce() -> T) -> (T, Vec<MeasuredSelect>) {
    let outer = replace_recording(Some(Recording {
        started: Instant::now(),
        selects: Vec::new(),
        metering: false,
    }));
    let result = run();
    let recorded = replace_recording(outer);
    (
        result,
        recorded.map(|recorded| recorded.selects).unwrap_or_default(),
    )
}

pub(crate) struct SelectMeter {
    started: Instant,
    first_row: Cell<Option<Duration>>,
    last_row: Cell<Option<Duration>>,
    rows: Cell<u64>,
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
