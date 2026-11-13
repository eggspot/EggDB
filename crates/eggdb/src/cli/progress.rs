use crate::cli::output::Output;
use eggdb_platform::time::Instant;
use std::time::Duration;

const SHOWN_EVERY: Duration = Duration::from_secs(1);

pub(crate) struct ProgressLine {
    enabled: bool,
    last: Instant,
    width: usize,
}

impl ProgressLine {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last: Instant::now(),
            width: 0,
        }
    }

    pub(crate) fn show(&mut self, messages: &mut Output<'_>, line: impl FnOnce() -> String) {
        if !self.enabled || self.last.elapsed() < SHOWN_EVERY {
            return;
        }
        self.last = Instant::now();
        let line = line();
        let width = line.chars().count();
        messages.text("\r");
        messages.text(&line);
        messages.text(&" ".repeat(self.width.saturating_sub(width)));
        self.width = width;
        let _ = messages.finish();
    }

    pub(crate) fn clear(&mut self, messages: &mut Output<'_>) {
        if self.width == 0 {
            return;
        }
        messages.text("\r");
        messages.text(&" ".repeat(self.width));
        messages.text("\r");
        self.width = 0;
        let _ = messages.finish();
    }
}
