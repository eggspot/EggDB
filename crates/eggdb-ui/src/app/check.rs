use super::modal::{error_line, wrap};
use super::{Hint, Ui};
use crate::frame::Frame;
use crate::source::{CheckSummary, ErrorKind, Source, SourceError, Summary};
use crate::terminal::KeyCode;
use crate::text;
use crate::theme::{Role, Symbol};
use std::time::{Duration, Instant};

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

#[derive(Debug)]
pub(super) struct CheckRun {
    started: Instant,
    elapsed: Duration,
    outcome: Option<Result<CheckSummary, SourceError>>,
}

impl CheckRun {
    pub(super) fn running_label(&self) -> Option<String> {
        let seconds = self.elapsed.as_secs();
        self.outcome
            .is_none()
            .then(|| format!("check running {:02}:{:02}", seconds / 60, seconds % 60))
    }
}

fn hit_rate(summary: &Summary) -> String {
    let lookups = summary.cache_hits + summary.cache_misses;
    if lookups == 0 {
        return "no lookups yet".to_owned();
    }
    format!(
        "{:.1}% hits",
        summary.cache_hits as f64 * 100.0 / lookups as f64
    )
}

impl<S: Source> Ui<S> {
    pub(super) fn check_running(&self) -> bool {
        self.check
            .as_ref()
            .is_some_and(|check| check.outcome.is_none())
    }

    pub(super) fn start_check(&mut self) {
        self.summary = self.source.summary();
        match self.source.start_check() {
            Ok(()) => {
                self.check = Some(CheckRun {
                    started: Instant::now(),
                    elapsed: Duration::ZERO,
                    outcome: None,
                });
            }
            Err(error) => self.error(error),
        }
    }

    pub(super) fn poll_check(&mut self, now: Instant) -> bool {
        let outcome = self.source.check_outcome();
        let Some(check) = self.check.as_mut() else {
            return false;
        };
        let seconds_before = check.elapsed.as_secs();
        check.elapsed = now.saturating_duration_since(check.started);
        let Some(outcome) = outcome else {
            return check.elapsed.as_secs() != seconds_before;
        };
        if let Ok(report) = &outcome
            && self.filter.is_empty()
        {
            self.count = report.entries;
            self.count_resume = None;
            self.counting = false;
        }
        check.outcome = Some(outcome);
        true
    }

    pub(super) fn check_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Enter | KeyCode::Esc if !self.check_running() => {
                self.check = None;
                self.reread_window();
            }
            _ => {}
        }
    }

    pub(super) fn check_hints(&self) -> Option<Vec<Hint>> {
        self.check.as_ref()?;
        Some(if self.check_running() {
            vec![Hint::new("Ctrl-C", "quit")]
        } else {
            vec![Hint::new("Enter", "back"), Hint::new("q", "quit")]
        })
    }

    pub(super) fn draw_check(&self, frame: &mut Frame, check: &CheckRun) {
        let dot = format!(" {} ", self.theme.symbol(Symbol::MiddleDot));
        let mut lines: Vec<(String, Role)> = Vec::new();
        if let Ok(summary) = &self.summary {
            lines.push((
                "Stats (from the header, read without touching a page)".to_owned(),
                Role::Plain,
            ));
            lines.push((
                format!(
                    "  page size {}{dot}{} pages{dot}{} free{dot}file {}",
                    text::file_size(summary.page_size as u64),
                    text::count(summary.page_count),
                    text::count(summary.free_pages),
                    text::file_size(summary.file_bytes)
                ),
                Role::Plain,
            ));
            let last_commit = summary
                .last_commit_detail
                .clone()
                .unwrap_or_else(|| "none yet".to_owned());
            lines.push((format!("  last commit {last_commit}"), Role::Plain));
            lines.push((
                format!(
                    "  cache {}, {} pages held, {}{dot}log since checkpoint {}",
                    text::file_size(summary.cache_bytes),
                    text::count(summary.cached_pages),
                    hit_rate(summary),
                    text::file_size(summary.log_bytes)
                ),
                Role::Plain,
            ));
            lines.push((String::new(), Role::Plain));
        }
        match &check.outcome {
            None => {
                let seconds = check.elapsed.as_secs();
                lines.push(("Check".to_owned(), Role::Plain));
                lines.push((
                    format!(
                        "  reading every page{} {}  {seconds} s",
                        self.theme.symbol(Symbol::Ellipsis),
                        SPINNER[(seconds % 4) as usize]
                    ),
                    Role::Plain,
                ));
                lines.push((String::new(), Role::Plain));
                lines.push((
                    "Other operations wait while the check runs; Ctrl-C quits.".to_owned(),
                    Role::Dim,
                ));
            }
            Some(Ok(report)) => self.report_lines(&mut lines, report, check.elapsed),
            Some(Err(error)) => self.failure_lines(&mut lines, error),
        }
        let width = usize::from(self.size.cols.saturating_sub(4));
        let mut row = 2u16;
        let last_row = self.size.rows.saturating_sub(2);
        for (line, role) in lines {
            let indent = line.len() - line.trim_start().len();
            for wrapped in wrap(line.trim_start(), width.saturating_sub(indent)) {
                if row >= last_row {
                    return;
                }
                self.put(
                    frame,
                    2 + indent as u16,
                    row,
                    &wrapped,
                    role,
                    self.size.cols.saturating_sub(3),
                );
                row += 1;
            }
        }
    }

    fn report_lines(
        &self,
        lines: &mut Vec<(String, Role)>,
        report: &CheckSummary,
        elapsed: Duration,
    ) {
        let dot = format!(" {} ", self.theme.symbol(Symbol::MiddleDot));
        lines.push((
            format!(
                "Check  OK {} every page accounted for, {:.1} s",
                self.theme.symbol(Symbol::EmDash),
                elapsed.as_secs_f64()
            ),
            Role::Success,
        ));
        lines.push((
            format!(
                "  {} keys{dot}keys {}{dot}values {}",
                text::count(report.entries),
                text::file_size(report.key_bytes),
                text::file_size(report.value_bytes)
            ),
            Role::Plain,
        ));
        lines.push((
            format!(
                "  tree height {}, {} pages{dot}{} large values on {} overflow pages",
                report.tree_height,
                text::count(report.tree_pages),
                text::count(report.overflow_values),
                text::count(report.overflow_pages)
            ),
            Role::Plain,
        ));
        lines.push((
            format!(
                "  {} free pages{dot}{} log pages{dot}{} leaked pages",
                text::count(report.free_pages),
                text::count(report.log_pages),
                text::count(report.leaked_pages)
            ),
            Role::Plain,
        ));
        if report.leaked_pages > 0 {
            lines.push((
                format!(
                    "  Warning: {} leaked pages do no harm but are never reused",
                    text::count(report.leaked_pages)
                ),
                Role::Warning,
            ));
        }
    }

    fn failure_lines(&self, lines: &mut Vec<(String, Role)>, error: &SourceError) {
        let name = self
            .summary
            .as_ref()
            .map(|summary| summary.name.clone())
            .unwrap_or_default();
        let verdict = if error.kind == ErrorKind::Corrupt {
            "Check  FAILED"
        } else {
            "Check  did not finish"
        };
        lines.push((verdict.to_owned(), Role::Error));
        lines.push((format!("  {}", error_line(error)), Role::Error));
        if error.kind == ErrorKind::Corrupt {
            lines.push((
                format!(
                    "  Try: eggdb check {name} for the same report on the command line; restore the file from a backup"
                ),
                Role::Plain,
            ));
        }
    }
}
