mod check;
mod edit;
mod modal;

use crate::frame::Frame;
use crate::json::{self, JsonLine, Token};
use crate::source::{KeyRow, Source, SourceError, Start, Summary};
use crate::terminal::{Capabilities, Event, KeyCode, KeyEvent, Size, Terminal};
use crate::text;
use crate::text_view::{self, Segment};
use crate::theme::{Role, Symbol, Theme};
use crate::width::text_width;
use check::CheckRun;
use edit::Edit;
use modal::Modal;
use std::io;
use std::time::{Duration, Instant};

const LARGEST_AUTO_LOAD: u64 = 64 << 20;
const COUNT_STEP: u64 = 10_000;
const SMALLEST: Size = Size { cols: 40, rows: 10 };
const TWO_PANES_FROM_COLS: u16 = 80;
const PAN_STEP: usize = 8;
const MOST_LINES: usize = 1 << 20;
const JSON_TOO_LARGE: &str = "Shown as text: the JSON view takes values up to 4 MiB";
const READ_ONLY_TOAST: &str = "Opened read-only; start without --read-only to edit";
const TOAST_LIFETIME: Duration = Duration::from_secs(8);
const WAIT_FOR_A_WORKER: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Keys,
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Json,
    Hex,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Hex => "hex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Success,
    Error,
}

impl ToastKind {
    fn role(self) -> Role {
        match self {
            Self::Info => Role::Plain,
            Self::Success => Role::Success,
            Self::Error => Role::Error,
        }
    }
}

#[derive(Debug, Clone)]
struct Toast {
    kind: ToastKind,
    text: String,
    shown_at: Instant,
}

#[derive(Debug)]
struct Viewer {
    key: Vec<u8>,
    value_bytes: u64,
    bytes: Option<Vec<u8>>,
    missing: bool,
    forced: Option<Mode>,
    mode: Mode,
    json: Option<Vec<JsonLine>>,
    line_starts: Vec<usize>,
    lines_capped: bool,
    scroll: usize,
    wrap_offset: usize,
    pan: usize,
    wrap: bool,
}

#[derive(Debug)]
struct FilterEdit {
    text: String,
    previous: Vec<u8>,
}

/// How the UI was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiOptions {
    /// What the terminal can show, after `--mono`, `--ascii`, `NO_COLOR` and `TERM`.
    pub capabilities: Capabilities,
}

/// The `eggdb ui` screen: the state of the key list and the value viewer, changed by
/// [`Ui::handle`] and drawn by [`Ui::draw`].
#[derive(Debug)]
pub struct Ui<S> {
    source: S,
    theme: Theme,
    size: Size,
    summary: Result<Summary, SourceError>,
    focus: Pane,
    filter: Vec<u8>,
    filter_edit: Option<FilterEdit>,
    rows: Vec<KeyRow>,
    selected: usize,
    first_index: Option<u64>,
    count: u64,
    count_resume: Option<Vec<u8>>,
    counting: bool,
    viewer: Option<Viewer>,
    toast: Option<Toast>,
    last_message: Option<Toast>,
    help: Option<usize>,
    edit: Option<Edit>,
    modal: Option<Modal>,
    check: Option<CheckRun>,
    quit: bool,
}

impl<S: Source> Ui<S> {
    /// Opens the screen on `source` at `size`: the first keys, the first value, and a count
    /// that runs in steps on [`Ui::tick`].
    pub fn new(source: S, options: UiOptions, size: Size) -> Self {
        let summary = source.summary();
        let mut ui = Self {
            source,
            theme: Theme::new(options.capabilities),
            size,
            summary,
            focus: Pane::Keys,
            filter: Vec::new(),
            filter_edit: None,
            rows: Vec::new(),
            selected: 0,
            first_index: Some(0),
            count: 0,
            count_resume: None,
            counting: true,
            viewer: None,
            toast: None,
            last_message: None,
            help: None,
            edit: None,
            modal: None,
            check: None,
            quit: false,
        };
        ui.home();
        if ui.toast.is_none()
            && let Ok(summary) = &ui.summary
        {
            let access = if summary.read_only {
                "read-only"
            } else {
                "read-write"
            };
            let opened = format!("Opened {} {access}", summary.name);
            ui.info(opened);
        }
        ui
    }

    /// The source the UI reads and writes.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Ends the UI and gives back its source.
    pub fn into_source(self) -> S {
        self.source
    }

    /// Whether the user asked to quit.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether no background work is left: no key count, no write waiting for the writer and
    /// no check running.
    pub fn is_idle(&self) -> bool {
        !self.has_steps() && !self.check_running()
    }

    pub(crate) fn has_steps(&self) -> bool {
        self.counting || self.write_waiting()
    }

    /// Runs one step of background work and expires an old toast; returns whether the screen
    /// changed.
    pub fn tick(&mut self) -> bool {
        self.tick_at(Instant::now())
    }

    /// [`Ui::tick`] as if the time were `now`, which lets tests expire toasts.
    pub fn tick_at(&mut self, now: Instant) -> bool {
        if self.write_waiting() {
            self.retry_write();
            return true;
        }
        let mut changed = false;
        if self.check_running() {
            changed = self.poll_check(now);
        } else if self.counting {
            self.count_step();
            changed = true;
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| now.saturating_duration_since(toast.shown_at) >= TOAST_LIFETIME)
        {
            self.toast = None;
            changed = true;
        }
        changed
    }

    fn count_step(&mut self) {
        match self
            .source
            .count(&self.filter, self.count_resume.as_deref(), COUNT_STEP)
        {
            Ok(step) => {
                self.count += step.counted;
                self.count_resume = step.resume_after;
                self.counting = self.count_resume.is_some();
                if !self.counting && self.first_index.is_none() && self.at_end() {
                    self.first_index = Some(self.count.saturating_sub(self.rows.len() as u64));
                }
            }
            Err(message) => {
                self.counting = false;
                self.error(message);
            }
        }
    }

    /// Runs background work until none is left, waiting for a running check to end.
    pub fn run_until_idle(&mut self) {
        while !self.is_idle() {
            self.tick();
            if self.check_running() {
                std::thread::sleep(WAIT_FOR_A_WORKER);
            }
        }
    }

    /// Handles every event `terminal` has queued, finishes background work, then draws and
    /// presents one frame. Tests drive the UI with this.
    pub fn process<T: Terminal>(&mut self, terminal: &mut T, frame: &mut Frame) -> io::Result<()> {
        while let Some(event) = terminal.next_event(Duration::ZERO)? {
            self.handle(event);
        }
        self.run_until_idle();
        self.draw(frame);
        terminal.present(frame)
    }

    fn page(&self) -> usize {
        usize::from(self.size.rows.saturating_sub(4)).max(1)
    }

    fn two_panes(&self) -> bool {
        self.size.cols >= TWO_PANES_FROM_COLS
    }

    fn list_cols(&self) -> u16 {
        if self.two_panes() {
            let two_fifths = u32::from(self.size.cols) * 2 / 5;
            u16::try_from(two_fifths.clamp(28, 60)).unwrap_or(60)
        } else {
            self.size.cols
        }
    }

    fn error(&mut self, error: SourceError) {
        self.toast(
            ToastKind::Error,
            format!("Error ({}): {}", error.kind.name(), error.message),
        );
    }

    fn info(&mut self, message: impl Into<String>) {
        self.toast(ToastKind::Info, message.into());
    }

    fn toast(&mut self, kind: ToastKind, text: String) {
        let toast = Toast {
            kind,
            text,
            shown_at: Instant::now(),
        };
        self.last_message = Some(toast.clone());
        self.toast = Some(toast);
    }

    fn fetch(&mut self, start: Start<'_>, limit: usize) -> Vec<KeyRow> {
        match self.source.keys(&self.filter, start, limit) {
            Ok(mut rows) => {
                if matches!(start, Start::Last | Start::Before(_)) {
                    rows.reverse();
                }
                rows
            }
            Err(message) => {
                self.error(message);
                Vec::new()
            }
        }
    }

    fn at_end(&self) -> bool {
        match self.rows.last() {
            Some(last) => self
                .source
                .keys(&self.filter, Start::After(&last.key), 1)
                .is_ok_and(|rows| rows.is_empty()),
            None => true,
        }
    }

    fn home(&mut self) {
        self.rows = self.fetch(Start::First, self.page());
        self.selected = 0;
        self.first_index = Some(0);
        self.selection_changed();
    }

    fn end(&mut self) {
        self.rows = self.fetch(Start::Last, self.page());
        self.selected = self.rows.len().saturating_sub(1);
        self.first_index =
            (!self.counting).then(|| self.count.saturating_sub(self.rows.len() as u64));
        self.selection_changed();
    }

    fn shift_first(&mut self, by: i64) {
        self.first_index = self
            .first_index
            .and_then(|first| first.checked_add_signed(by));
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        } else if let Some(last) = self.rows.last().map(|row| row.key.clone()) {
            let more = self.fetch(Start::After(&last), 1);
            if more.is_empty() {
                return;
            }
            self.rows.extend(more);
            if self.rows.len() > self.page() {
                self.rows.remove(0);
                self.shift_first(1);
            }
            self.selected = self.rows.len() - 1;
        }
        self.selection_changed();
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        } else if let Some(first) = self.rows.first().map(|row| row.key.clone()) {
            let more = self.fetch(Start::Before(&first), 1);
            if more.is_empty() {
                return;
            }
            self.rows.splice(0..0, more);
            self.rows.truncate(self.page());
            self.shift_first(-1);
            self.selected = 0;
        }
        self.selection_changed();
    }

    fn page_down(&mut self) {
        let page = self.page();
        let Some(last) = self.rows.last().map(|row| row.key.clone()) else {
            return;
        };
        let more = self.fetch(Start::After(&last), page);
        if more.is_empty() {
            self.selected = self.rows.len().saturating_sub(1);
        } else {
            let mut combined = std::mem::take(&mut self.rows);
            combined.extend(more);
            let skip = page.min(combined.len().saturating_sub(page));
            self.rows = combined.split_off(skip);
            self.rows.truncate(page);
            self.shift_first(skip as i64);
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
        self.selection_changed();
    }

    fn page_up(&mut self) {
        let page = self.page();
        let Some(first) = self.rows.first().map(|row| row.key.clone()) else {
            return;
        };
        let more = self.fetch(Start::Before(&first), page);
        if more.is_empty() {
            self.selected = 0;
        } else {
            let added = more.len();
            let mut combined = more;
            combined.append(&mut self.rows);
            combined.truncate(page);
            self.rows = combined;
            self.shift_first(-(added as i64));
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
        self.selection_changed();
    }

    fn refit(&mut self) {
        let page = self.page();
        if self.rows.len() > page {
            let drop = (self.selected + 1).saturating_sub(page);
            self.rows.drain(..drop);
            self.selected -= drop;
            self.shift_first(drop as i64);
            self.rows.truncate(page);
        }
        if self.rows.len() < page {
            if let Some(last) = self.rows.last().map(|row| row.key.clone()) {
                let more = self.fetch(Start::After(&last), page - self.rows.len());
                self.rows.extend(more);
            }
            if let Some(first) = self.rows.first().map(|row| row.key.clone())
                && self.rows.len() < page
            {
                let more = self.fetch(Start::Before(&first), page - self.rows.len());
                let added = more.len();
                self.rows.splice(0..0, more);
                self.selected += added;
                self.shift_first(-(added as i64));
            }
        }
    }

    fn restart_count(&mut self) {
        self.count = 0;
        self.count_resume = None;
        self.counting = true;
    }

    fn reload(&mut self) {
        self.reread_window();
        self.restart_count();
    }

    fn reread_window(&mut self) {
        self.summary = self.source.summary();
        let anchor = self.rows.first().map(|row| row.key.clone());
        let selected_key = self.rows.get(self.selected).map(|row| row.key.clone());
        self.rows = match &anchor {
            Some(anchor) => self.fetch(Start::AtOrAfter(anchor), self.page()),
            None => self.fetch(Start::First, self.page()),
        };
        self.selected = selected_key
            .and_then(|key| self.rows.iter().position(|row| row.key >= key))
            .unwrap_or_else(|| self.rows.len().saturating_sub(1));
        self.refit();
        self.viewer = None;
        self.selection_changed();
    }

    fn selection_changed(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            self.viewer = None;
            return;
        };
        if self
            .viewer
            .as_ref()
            .is_some_and(|viewer| viewer.key == row.key)
        {
            return;
        }
        let key = row.key.clone();
        let value_bytes = row.value_bytes;
        let mut viewer = Viewer {
            key,
            value_bytes,
            bytes: None,
            missing: false,
            forced: None,
            mode: Mode::Text,
            json: None,
            line_starts: Vec::new(),
            lines_capped: false,
            scroll: 0,
            wrap_offset: 0,
            pan: 0,
            wrap: false,
        };
        if value_bytes <= LARGEST_AUTO_LOAD {
            self.load_value(&mut viewer);
        }
        self.viewer = Some(viewer);
    }

    fn load_value(&mut self, viewer: &mut Viewer) {
        match self.source.value(&viewer.key) {
            Ok(Some(bytes)) => {
                viewer.value_bytes = bytes.len() as u64;
                let (starts, capped) = line_starts(&bytes);
                viewer.line_starts = starts;
                viewer.lines_capped = capped;
                viewer.bytes = Some(bytes);
                self.choose_mode(viewer);
            }
            Ok(None) => viewer.missing = true,
            Err(message) => self.error(message),
        }
    }

    fn choose_mode(&mut self, viewer: &mut Viewer) {
        let Some(bytes) = viewer.bytes.as_deref() else {
            return;
        };
        viewer.json = None;
        let too_big_for_json = bytes.len() > json::LARGEST_JSON;
        viewer.mode = match viewer.forced {
            Some(Mode::Json) if too_big_for_json => {
                self.info(JSON_TOO_LARGE);
                Mode::Text
            }
            Some(Mode::Json) => match json::pretty(bytes) {
                Ok(lines) => {
                    viewer.json = Some(lines);
                    Mode::Json
                }
                Err(error) => {
                    self.toast(ToastKind::Error, format!("Error: {}", error.message()));
                    Mode::Text
                }
            },
            Some(mode) => mode,
            None => {
                let text = std::str::from_utf8(bytes).ok();
                let has_controls = bytes
                    .iter()
                    .any(|&b| (b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r')) || b == 0x7f);
                match text {
                    _ if bytes.is_empty() => Mode::Text,
                    None => Mode::Hex,
                    Some(_) if has_controls => Mode::Hex,
                    Some(text) if too_big_for_json && text.trim_start().starts_with(['{', '[']) => {
                        self.info(JSON_TOO_LARGE);
                        Mode::Text
                    }
                    Some(text) if text.trim_start().starts_with(['{', '[']) => {
                        match json::pretty(bytes) {
                            Ok(lines) => {
                                viewer.json = Some(lines);
                                Mode::Json
                            }
                            Err(_) => Mode::Text,
                        }
                    }
                    Some(_) => Mode::Text,
                }
            }
        };
        viewer.scroll = 0;
        viewer.wrap_offset = 0;
        viewer.pan = 0;
        if viewer.mode == Mode::Text && viewer.lines_capped {
            self.info(format!(
                "Showing the first {} lines; v shows the whole value as hex",
                text::count(MOST_LINES as u64)
            ));
        }
    }

    /// Changes the state for one event.
    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Interrupt => self.quit = true,
            Event::Resize(size) => {
                let hex_per_row = self.hex_per_row();
                self.size = size;
                if !self.check_running() {
                    self.refit();
                    self.selection_changed();
                    self.keep_scrolls_in_range(hex_per_row);
                }
            }
            Event::Paste(text) => {
                if self.modal.is_some() || self.help.is_some() || self.check.is_some() {
                    return;
                }
                if self.edit.is_some() {
                    self.edit_paste(&text);
                } else if let Some(edit) = self.filter_edit.as_mut() {
                    edit.text
                        .push_str(&text::literal(&text.replace(['\n', '\r'], "")));
                    self.apply_filter();
                }
            }
            Event::Key(key) => self.key(key),
        }
        self.follow_editor_cursor();
    }

    fn keep_scrolls_in_range(&mut self, previous_hex_per_row: usize) {
        if let Some(scroll) = self.help {
            let visible = usize::from(self.size.rows.saturating_sub(2));
            self.help = Some(scroll.min(help_lines(self).len().saturating_sub(visible)));
        }
        let Some(mut viewer) = self.viewer.take() else {
            return;
        };
        if viewer.mode == Mode::Hex {
            let first_byte = viewer.scroll.saturating_mul(previous_hex_per_row);
            viewer.scroll = first_byte / self.hex_per_row();
        }
        if !self.wraps(&viewer) {
            let last_top = self
                .viewer_lines(&viewer)
                .saturating_sub(self.viewer_rows());
            viewer.scroll = viewer.scroll.min(last_top);
        }
        self.viewer = Some(viewer);
    }

    fn key(&mut self, key: KeyEvent) {
        self.toast = None;
        let ctrl = key.modifiers.ctrl;
        if ctrl && key.code == KeyCode::Char('c') {
            self.ctrl_c();
            return;
        }
        if self.size.cols < SMALLEST.cols || self.size.rows < SMALLEST.rows {
            if key.code == KeyCode::Char('q') && !self.has_unsaved_edit() {
                self.quit = true;
            }
            return;
        }
        if self.help.is_some() {
            self.help_key(key.code);
            return;
        }
        if self.check.is_some() {
            self.check_key(key.code);
            return;
        }
        if self.modal.is_some() {
            self.modal_key(key);
            return;
        }
        if self.edit.is_some() {
            self.edit_key(key);
            return;
        }
        if self.filter_edit.is_some() {
            self.filter_key(key);
            return;
        }
        if ctrl || key.modifiers.alt {
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.help = Some(0),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Pane::Keys => Pane::Value,
                    Pane::Value => Pane::Keys,
                };
            }
            KeyCode::Char('e' | 'a' | 'd') if self.read_only() => self.info(READ_ONLY_TOAST),
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Char('d') => self.start_delete(),
            KeyCode::Char('c') => self.start_check(),
            KeyCode::Char('m') => self.show_last_message(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('v') => self.cycle_mode(),
            _ => match self.focus {
                Pane::Keys => self.list_key(key.code),
                Pane::Value => self.viewer_key(key.code),
            },
        }
    }

    fn read_only(&self) -> bool {
        self.summary.as_ref().is_ok_and(|summary| summary.read_only)
    }

    fn ctrl_c(&mut self) {
        if self.has_unsaved_edit() && !self.modal.as_ref().is_some_and(Modal::asks_to_quit) {
            self.modal = Some(Modal::quit_question());
        } else {
            self.quit = true;
        }
    }

    fn list_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::Home | KeyCode::Char('g') => self.home(),
            KeyCode::End | KeyCode::Char('G') => self.end(),
            KeyCode::Enter | KeyCode::Right if !self.rows.is_empty() => self.focus = Pane::Value,
            KeyCode::Char('/') => {
                self.filter_edit = Some(FilterEdit {
                    text: String::new(),
                    previous: self.filter.clone(),
                });
            }
            _ => {}
        }
    }

    fn viewer_key(&mut self, code: KeyCode) {
        let visible = self.viewer_rows();
        let Some(mut viewer) = self.viewer.take() else {
            if code == KeyCode::Esc {
                self.focus = Pane::Keys;
            }
            return;
        };
        if self.wraps(&viewer) && self.wrapped_key(&mut viewer, code, visible) {
            self.viewer = Some(viewer);
            return;
        }
        let last_top = self.viewer_lines(&viewer).saturating_sub(visible);
        match code {
            KeyCode::Esc => self.focus = Pane::Keys,
            KeyCode::Enter if viewer.bytes.is_none() && !viewer.missing => {
                self.load_value(&mut viewer);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                viewer.scroll = (viewer.scroll + 1).min(last_top);
            }
            KeyCode::Up | KeyCode::Char('k') => viewer.scroll = viewer.scroll.saturating_sub(1),
            KeyCode::PageDown => viewer.scroll = (viewer.scroll + visible).min(last_top),
            KeyCode::PageUp => viewer.scroll = viewer.scroll.saturating_sub(visible),
            KeyCode::Home | KeyCode::Char('g') => viewer.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => viewer.scroll = last_top,
            KeyCode::Right if !viewer.wrap && viewer.mode != Mode::Hex => {
                viewer.pan += PAN_STEP;
            }
            KeyCode::Left => viewer.pan = viewer.pan.saturating_sub(PAN_STEP),
            KeyCode::Char('w') if viewer.mode == Mode::Text => {
                viewer.wrap = !viewer.wrap;
                viewer.pan = 0;
                viewer.wrap_offset = 0;
                viewer.scroll = viewer.scroll.min(last_top);
            }
            _ => {}
        }
        self.viewer = Some(viewer);
    }

    fn cycle_mode(&mut self) {
        let Some(mut viewer) = self.viewer.take() else {
            return;
        };
        viewer.forced = match viewer.forced {
            None => Some(Mode::Text),
            Some(Mode::Text) => Some(Mode::Json),
            Some(Mode::Json) => Some(Mode::Hex),
            Some(Mode::Hex) => None,
        };
        self.choose_mode(&mut viewer);
        self.viewer = Some(viewer);
    }

    fn help_key(&mut self, code: KeyCode) {
        let total = help_lines(self).len();
        let visible = usize::from(self.size.rows.saturating_sub(2));
        let last_top = total.saturating_sub(visible);
        let Some(scroll) = self.help.as_mut() else {
            return;
        };
        match code {
            KeyCode::Esc | KeyCode::Char('?' | 'q') => self.help = None,
            KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(last_top),
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            KeyCode::PageDown => *scroll = (*scroll + visible).min(last_top),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(visible),
            KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
            KeyCode::End | KeyCode::Char('G') => *scroll = last_top,
            _ => {}
        }
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter => self.filter_edit = None,
            KeyCode::Esc => {
                let restore = if edit.text.is_empty() {
                    Vec::new()
                } else {
                    edit.previous.clone()
                };
                self.filter_edit = None;
                if restore != self.filter {
                    self.filter = restore;
                    self.filter_changed();
                }
            }
            KeyCode::Backspace => {
                if edit.text.pop().is_none() {
                    self.filter_edit = None;
                } else {
                    self.apply_filter();
                }
            }
            KeyCode::Char(c) if !key.modifiers.ctrl && !key.modifiers.alt => {
                edit.text.push(c);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn apply_filter(&mut self) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        let filter = text::unescaped(&edit.text);
        if filter != self.filter {
            self.filter = filter;
            self.filter_changed();
        }
    }

    fn filter_changed(&mut self) {
        self.restart_count();
        self.viewer = None;
        self.home();
    }

    fn viewer_rows(&self) -> usize {
        usize::from(self.size.rows.saturating_sub(6))
    }

    fn viewer_cols(&self) -> u16 {
        if self.two_panes() {
            self.size.cols - self.list_cols() - 1
        } else {
            self.size.cols
        }
    }

    fn hex_per_row(&self) -> usize {
        if self.viewer_cols() >= 76 { 16 } else { 8 }
    }

    fn viewer_lines(&self, viewer: &Viewer) -> usize {
        match viewer.mode {
            Mode::Text => viewer.line_starts.len(),
            Mode::Json => viewer.json.as_ref().map_or(0, Vec::len),
            Mode::Hex => viewer
                .bytes
                .as_ref()
                .map_or(0, |bytes| bytes.len().div_ceil(self.hex_per_row())),
        }
    }

    /// Draws the whole screen into `frame`, resizing it to the UI's size first.
    pub fn draw(&self, frame: &mut Frame) {
        if frame.size != self.size {
            *frame = Frame::new(self.size);
        }
        frame.clear();
        if self.size.cols < SMALLEST.cols || self.size.rows < SMALLEST.rows {
            self.draw_too_small(frame);
            return;
        }
        if let Some(scroll) = self.help {
            self.draw_help(frame, scroll);
            return;
        }
        self.draw_header(frame);
        if let Some(check) = &self.check {
            self.draw_check(frame, check);
            self.draw_status(frame);
            self.draw_hints(frame);
            return;
        }
        let list_cols = self.list_cols();
        if self.two_panes() {
            self.draw_list(frame, 0, list_cols);
            let line = self.theme.symbol(Symbol::VerticalLine);
            for row in 1..self.size.rows - 2 {
                frame.put(list_cols, row, line, self.theme.style(Role::Plain), 1, '|');
            }
            self.draw_value_pane(frame, list_cols + 1, self.viewer_cols());
        } else if self.focus == Pane::Keys {
            self.draw_list(frame, 0, self.size.cols);
        } else {
            self.draw_value_pane(frame, 0, self.size.cols);
        }
        self.draw_status(frame);
        self.draw_hints(frame);
        if let Some(modal) = &self.modal {
            self.draw_modal(frame, modal);
        }
    }

    fn draw_value_pane(&self, frame: &mut Frame, col: u16, width: u16) {
        if !self.draw_value_editor(frame, col, width) {
            self.draw_viewer(frame, col, width);
        }
    }

    fn put(&self, frame: &mut Frame, col: u16, row: u16, text: &str, role: Role, room: u16) -> u16 {
        frame.put(
            col,
            row,
            text,
            self.theme.style(role),
            room,
            self.theme.ellipsis(),
        )
    }

    fn put_right(&self, frame: &mut Frame, end: u16, row: u16, text: &str, role: Role) {
        let width = u16::try_from(text_width(text)).unwrap_or(u16::MAX);
        let col = end.saturating_sub(width);
        self.put(frame, col, row, text, role, end - col);
    }

    fn draw_too_small(&self, frame: &mut Frame) {
        let lines = [
            format!(
                "EggDB needs at least {} x {}.",
                SMALLEST.cols, SMALLEST.rows
            ),
            format!("This one is {} x {}.", self.size.cols, self.size.rows),
            "Make it larger, or press q.".to_owned(),
        ];
        let top = (self.size.rows / 2).saturating_sub(1);
        for (offset, line) in lines.iter().enumerate() {
            let width = u16::try_from(text_width(line)).unwrap_or(u16::MAX);
            let col = self.size.cols.saturating_sub(width) / 2;
            self.put(
                frame,
                col,
                top + offset as u16,
                line,
                Role::Plain,
                self.size.cols - col,
            );
        }
    }

    fn draw_header(&self, frame: &mut Frame) {
        let cols = self.size.cols;
        let header = self.theme.style(Role::Header);
        frame.fill(0, 0, cols, header);
        let dot = format!(" {} ", self.theme.symbol(Symbol::MiddleDot));
        let keys = format!(
            "{}{} keys",
            text::count(self.count),
            if self.counting { "+" } else { "" }
        );
        let (name, mut fields, access) = match &self.summary {
            Ok(summary) => {
                let mut fields = vec![
                    text::file_size(summary.file_bytes),
                    format!("{} pages", text::count(summary.page_count)),
                    keys,
                ];
                if let Some(time) = &summary.last_commit {
                    fields.push(time.clone());
                }
                let access = if summary.read_only { "RO" } else { "RW" };
                (summary.name.as_str(), fields, access)
            }
            Err(_) => ("", vec![keys], "RO"),
        };
        if !self.two_panes() {
            fields.retain(|field| field.ends_with(" keys"));
        }
        if let Some(running) = self.check.as_ref().and_then(CheckRun::running_label) {
            fields = vec![running];
        }
        let left = " EggDB  ";
        let left_width = text_width(left);
        let name_width = text_width(name);
        let fixed = left_width + name_width.min(16) + 1;
        let mut right = String::new();
        while !fields.is_empty() {
            right = format!("{}  {} ", fields.join(&dot), access);
            if fixed + text_width(&right) <= usize::from(cols) {
                break;
            }
            fields.remove(0);
            right = format!(" {access} ");
        }
        if fields.is_empty() {
            right = format!(" {access} ");
        }
        let right_width = u16::try_from(text_width(&right)).unwrap_or(cols);
        self.put(frame, 0, 0, left, Role::Header, cols);
        let name_room = usize::from(cols.saturating_sub(right_width + 8 + 2));
        let shown_name = if name_width > name_room {
            let mut tail: Vec<char> = Vec::new();
            let mut used = 1;
            for c in name.chars().rev() {
                let width = crate::width::char_width(c);
                if used + width > name_room {
                    break;
                }
                used += width;
                tail.push(c);
            }
            tail.push(self.theme.ellipsis());
            tail.iter().rev().collect()
        } else {
            name.to_owned()
        };
        self.put(
            frame,
            8,
            0,
            &shown_name,
            Role::Header,
            cols.saturating_sub(8),
        );
        self.put_right(frame, cols, 0, &right, Role::Header);
    }

    fn title_role(&self, pane: Pane) -> Role {
        if self.focus == pane {
            Role::FocusedTitle
        } else {
            Role::Title
        }
    }

    fn pane_name(&self, pane: Pane, name: &str) -> String {
        if self.theme.plain() && self.focus == pane {
            format!("[{name}]")
        } else {
            name.to_owned()
        }
    }

    fn position(&self) -> String {
        let dash = self.theme.symbol(Symbol::EnDash);
        let total = format!(
            "{}{}",
            text::count(self.count),
            if self.counting { "+" } else { "" }
        );
        match (self.first_index, self.rows.len()) {
            (_, 0) => format!("0 of {total}"),
            (Some(first), shown) => format!(
                "{}{dash}{} of {total}",
                text::count(first + 1),
                text::count(first + shown as u64)
            ),
            (None, _) => format!("?{dash}? of {total}"),
        }
    }

    fn draw_list(&self, frame: &mut Frame, col: u16, width: u16) {
        let role = self.title_role(Pane::Keys);
        frame.fill(col, 1, width, self.theme.style(role));
        if !self.draw_key_entry(frame, col, width) {
            let mut title = format!(" {}", self.pane_name(Pane::Keys, "Keys"));
            let mut cursor = None;
            if let Some(edit) = &self.filter_edit {
                title.push_str(" /");
                title.push_str(&edit.text);
                let at = col + u16::try_from(text_width(&title)).unwrap_or(width);
                cursor = Some((at.min(col + width - 1), 1));
            } else if !self.filter.is_empty() {
                title.push_str(" /");
                title.push_str(&text::escaped(&self.filter));
            }
            let position = format!("{} ", self.position());
            let position_width = u16::try_from(text_width(&position)).unwrap_or(width);
            self.put(
                frame,
                col,
                1,
                &title,
                role,
                width.saturating_sub(position_width + 1),
            );
            self.put_right(frame, col + width, 1, &position, role);
            frame.cursor = cursor;
        }
        let size_cells = 6u16;
        if self.rows.is_empty() {
            let message = if self.filter.is_empty() {
                format!(
                    "empty database {} press a to add a key",
                    self.theme.symbol(Symbol::EmDash)
                )
            } else {
                format!("no keys start with \"{}\"", text::escaped(&self.filter))
            };
            self.put(
                frame,
                col + 2,
                2,
                &message,
                Role::Dim,
                width.saturating_sub(2),
            );
            return;
        }
        for (index, row) in self.rows.iter().enumerate().take(self.page()) {
            let y = 2 + index as u16;
            let selected = index == self.selected;
            let role = if selected {
                Role::Selected
            } else {
                Role::Plain
            };
            if selected {
                frame.fill(col, y, width, self.theme.style(role));
            }
            let marker = if selected { "> " } else { "  " };
            self.put(frame, col, y, marker, role, 2);
            let key_room = width.saturating_sub(2 + 1 + size_cells);
            self.put(frame, col + 2, y, &text::escaped(&row.key), role, key_room);
            let size_role = if selected { Role::Selected } else { Role::Dim };
            self.put_right(
                frame,
                col + width,
                y,
                &text::short_size(row.value_bytes),
                size_role,
            );
        }
    }

    fn draw_viewer(&self, frame: &mut Frame, col: u16, width: u16) {
        let role = self.title_role(Pane::Value);
        frame.fill(col, 1, width, self.theme.style(role));
        let Some(viewer) = &self.viewer else {
            self.put(
                frame,
                col,
                1,
                &format!(" {}", self.pane_name(Pane::Value, "Value")),
                role,
                width,
            );
            return;
        };
        let dot = self.theme.symbol(Symbol::MiddleDot);
        let mode = match (viewer.forced, viewer.bytes.is_some()) {
            (_, false) => String::new(),
            (None, true) => format!("  {} (auto) {dot}", viewer.mode.name()),
            (Some(_), true) => format!("  {} {dot}", viewer.mode.name()),
        };
        let title = format!(
            " {}{mode} {}",
            self.pane_name(Pane::Value, "Value"),
            text::value_size(viewer.value_bytes)
        );
        let visible = self.viewer_rows();
        let position = match (viewer.mode, viewer.bytes.as_ref()) {
            (_, None) => String::new(),
            (Mode::Hex, Some(bytes)) => {
                let per_row = self.hex_per_row();
                let first = viewer.scroll * per_row;
                let last = ((viewer.scroll + visible) * per_row).min(bytes.len());
                if bytes.is_empty() {
                    "bytes 0 of 0 ".to_owned()
                } else {
                    format!(
                        "bytes {}{}{} of {} ",
                        text::count(first as u64),
                        self.theme.symbol(Symbol::EnDash),
                        text::count(last.saturating_sub(1) as u64),
                        text::count(bytes.len() as u64)
                    )
                }
            }
            (_, Some(_)) => {
                let total = self.viewer_lines(viewer);
                let last_line = if self.wraps(viewer) {
                    self.wrapped_rows(viewer, visible)
                        .last()
                        .map_or(viewer.scroll, |&(line, _, _)| line)
                        + 1
                } else {
                    viewer.scroll + visible
                };
                format!(
                    "line {}{}{}/{}{} ",
                    text::count((viewer.scroll + 1).min(total) as u64),
                    self.theme.symbol(Symbol::EnDash),
                    text::count(last_line.min(total) as u64),
                    text::count(total as u64),
                    if viewer.lines_capped { "+" } else { "" }
                )
            }
        };
        let position_width = u16::try_from(text_width(&position)).unwrap_or(width);
        self.put(
            frame,
            col,
            1,
            &title,
            role,
            width.saturating_sub(position_width + 1),
        );
        self.put_right(frame, col + width, 1, &position, role);
        self.put(
            frame,
            col + 1,
            2,
            &text::escaped(&viewer.key),
            Role::Plain,
            width.saturating_sub(1),
        );
        let rule = self
            .theme
            .symbol(Symbol::HorizontalLine)
            .repeat(usize::from(width));
        self.put(frame, col, 3, &rule, Role::Plain, width);
        let room = width.saturating_sub(1);
        let content_col = col + 1;
        let Some(bytes) = viewer.bytes.as_deref() else {
            let message = if viewer.missing {
                "This key no longer exists; press r to re-read".to_owned()
            } else {
                format!(
                    "{} value {} press Enter to load (takes {} of memory)",
                    text::short_size(viewer.value_bytes),
                    self.theme.symbol(Symbol::EnDash),
                    text::file_size(viewer.value_bytes)
                )
            };
            self.put(frame, content_col, 4, &message, Role::Dim, room);
            return;
        };
        if bytes.is_empty() {
            self.put(frame, content_col, 4, "(empty)", Role::Dim, room);
            return;
        }
        let rows: Vec<Vec<Segment>> = match viewer.mode {
            Mode::Text => self.text_rows(viewer, bytes, usize::from(room), visible),
            Mode::Json => viewer.json.as_ref().map_or_else(Vec::new, |lines| {
                lines
                    .iter()
                    .skip(viewer.scroll)
                    .take(visible)
                    .map(|line| json_segments(bytes, *line))
                    .collect()
            }),
            Mode::Hex => self.hex_rows(viewer, bytes, visible),
        };
        let pan = if viewer.mode == Mode::Hex || viewer.wrap {
            0
        } else {
            viewer.pan
        };
        for (index, segments) in rows.iter().enumerate().take(visible) {
            self.draw_segments(frame, content_col, 4 + index as u16, room, segments, pan);
        }
    }

    fn text_rows(
        &self,
        viewer: &Viewer,
        bytes: &[u8],
        room: usize,
        visible: usize,
    ) -> Vec<Vec<Segment>> {
        if self.wraps(viewer) {
            return self
                .wrapped_rows(viewer, visible)
                .into_iter()
                .map(|(line, from, to)| {
                    let line = line_bytes(viewer, bytes, line);
                    text_view::segments(line.get(from..to).unwrap_or_default())
                })
                .collect();
        }
        (viewer.scroll..viewer.line_starts.len())
            .take(visible)
            .map(|line| {
                text_view::prefix_segments(line_bytes(viewer, bytes, line), viewer.pan + room + 1)
            })
            .collect()
    }

    fn wraps(&self, viewer: &Viewer) -> bool {
        viewer.wrap && viewer.mode == Mode::Text && viewer.bytes.is_some()
    }

    fn wrap_room(&self) -> usize {
        usize::from(self.viewer_cols().saturating_sub(1)).max(1)
    }

    fn wrapped_rows(&self, viewer: &Viewer, limit: usize) -> Vec<(usize, usize, usize)> {
        let bytes = viewer.bytes.as_deref().unwrap_or_default();
        let room = self.wrap_room();
        let mut rows = Vec::new();
        let mut line = viewer.scroll;
        let mut from = viewer.wrap_offset;
        while rows.len() < limit && line < viewer.line_starts.len() {
            let content = line_bytes(viewer, bytes, line);
            let to = text_view::row_end(content, from, room);
            rows.push((line, from, to));
            if to >= content.len() {
                line += 1;
                from = 0;
            } else {
                from = to;
            }
        }
        rows
    }

    fn wrap_down(&self, viewer: &mut Viewer) -> bool {
        let visible = self.viewer_rows();
        let rows = self.wrapped_rows(viewer, visible + 1);
        match rows.get(1) {
            Some(&(line, from, _)) if rows.len() > visible => {
                viewer.scroll = line;
                viewer.wrap_offset = from;
                true
            }
            _ => false,
        }
    }

    fn wrap_up(&self, viewer: &mut Viewer) -> bool {
        let bytes = viewer.bytes.as_deref().unwrap_or_default();
        let room = self.wrap_room();
        if viewer.wrap_offset > 0 {
            let content = line_bytes(viewer, bytes, viewer.scroll);
            viewer.wrap_offset = text_view::row_start_before(content, viewer.wrap_offset, room);
            return true;
        }
        if viewer.scroll == 0 {
            return false;
        }
        viewer.scroll -= 1;
        let content = line_bytes(viewer, bytes, viewer.scroll);
        viewer.wrap_offset = text_view::row_start_before(content, content.len(), room);
        true
    }

    fn wrapped_key(&self, viewer: &mut Viewer, code: KeyCode, visible: usize) -> bool {
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.wrap_down(viewer);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.wrap_up(viewer);
            }
            KeyCode::PageDown => {
                for _ in 0..visible {
                    if !self.wrap_down(viewer) {
                        break;
                    }
                }
            }
            KeyCode::PageUp => {
                for _ in 0..visible {
                    if !self.wrap_up(viewer) {
                        break;
                    }
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                viewer.scroll = 0;
                viewer.wrap_offset = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                viewer.scroll = viewer.line_starts.len().saturating_sub(1);
                let bytes = viewer.bytes.as_deref().unwrap_or_default();
                let content = line_bytes(viewer, bytes, viewer.scroll);
                viewer.wrap_offset =
                    text_view::row_start_before(content, content.len(), self.wrap_room());
                for _ in 1..visible {
                    if !self.wrap_up(viewer) {
                        break;
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn hex_rows(&self, viewer: &Viewer, bytes: &[u8], visible: usize) -> Vec<Vec<Segment>> {
        let per_row = self.hex_per_row();
        let gutter = self.theme.symbol(Symbol::VerticalLine);
        bytes
            .chunks(per_row)
            .enumerate()
            .skip(viewer.scroll)
            .take(visible)
            .map(|(row, chunk)| {
                let mut hex = String::new();
                for index in 0..per_row {
                    match chunk.get(index) {
                        Some(byte) => hex.push_str(&format!("{byte:02x} ")),
                        None => hex.push_str("   "),
                    }
                    if index == 7 && per_row == 16 {
                        hex.push(' ');
                    }
                }
                let ascii: String = chunk
                    .iter()
                    .map(|&b| {
                        if (0x20..0x7f).contains(&b) {
                            char::from(b)
                        } else {
                            '.'
                        }
                    })
                    .collect();
                vec![
                    (format!("{:08x}  ", row * per_row), Role::HexOffset),
                    (hex, Role::Plain),
                    (gutter.to_owned(), Role::HexOffset),
                    (ascii, Role::Plain),
                ]
            })
            .collect()
    }

    fn draw_segments(
        &self,
        frame: &mut Frame,
        col: u16,
        row: u16,
        room: u16,
        segments: &[Segment],
        pan: usize,
    ) {
        let mut skip = pan;
        let mut at = col;
        let end = col + room;
        let total: usize = segments.iter().map(|(text, _)| text_width(text)).sum();
        let cut = total.saturating_sub(pan) > usize::from(room);
        let limit = if cut { end.saturating_sub(1) } else { end };
        'outer: for (text, role) in segments {
            for c in text.chars() {
                let width = crate::width::char_width(c);
                if skip > 0 {
                    skip = skip.saturating_sub(width);
                    continue;
                }
                if width == 0 {
                    if at > col {
                        frame.attach_mark(at - 1, row, c);
                    }
                    continue;
                }
                if at + width as u16 > limit {
                    break 'outer;
                }
                let mut buffer = [0u8; 4];
                at += self.put(
                    frame,
                    at,
                    row,
                    c.encode_utf8(&mut buffer),
                    *role,
                    limit - at,
                );
            }
        }
        if cut {
            let ellipsis = self.theme.symbol(Symbol::Ellipsis);
            self.put(frame, at, row, ellipsis, Role::Plain, 1);
        }
    }

    fn draw_status(&self, frame: &mut Frame) {
        let Some(toast) = &self.toast else {
            return;
        };
        let row = self.size.rows - 2;
        let room = self.size.cols.saturating_sub(2);
        let first_line = toast.text.lines().next().unwrap_or_default();
        let one_line = first_line.len() == toast.text.len();
        if one_line && text_width(first_line) <= usize::from(room) {
            self.put(frame, 1, row, first_line, toast.kind.role(), room);
            return;
        }
        let cut = text_width(first_line) + 6 > usize::from(room);
        let more = if cut {
            " (m)".to_owned()
        } else {
            format!(" {} (m)", self.theme.symbol(Symbol::Ellipsis))
        };
        let more_width = u16::try_from(text_width(&more)).unwrap_or(room);
        let used = self.put(
            frame,
            1,
            row,
            first_line,
            toast.kind.role(),
            room.saturating_sub(more_width),
        );
        self.put(frame, 1 + used, row, &more, Role::Dim, more_width);
    }

    fn hints(&self) -> Vec<Hint> {
        let up_down = self.theme.symbol(Symbol::UpDown);
        let left_right = self.theme.symbol(Symbol::LeftRight);
        let writable = !self.read_only();
        if let Some(hints) = self.check_hints() {
            return hints;
        }
        if let Some(modal) = &self.modal {
            return modal.hints();
        }
        if let Some(hints) = self.edit_hints() {
            return hints;
        }
        if self.filter_edit.is_some() {
            return vec![
                Hint::new("Enter", "keep filter"),
                Hint::new("Esc", "restore"),
                Hint::new("Backspace", "delete"),
                Hint::new("\\xNN", "a byte"),
            ];
        }
        if self.focus == Pane::Keys {
            vec![
                Hint::new(up_down, "move"),
                Hint::new("Enter", "open"),
                Hint::new("/", "filter"),
                Hint::new("e", "edit").available(writable && !self.rows.is_empty()),
                Hint::new("a", "add").available(writable),
                Hint::new("d", "delete").available(writable && !self.rows.is_empty()),
                Hint::new("c", "check"),
                Hint::new("?", "help"),
                Hint::new("q", "quit"),
            ]
        } else {
            vec![
                Hint::new(up_down, "scroll"),
                Hint::new(left_right, "pan"),
                Hint::new("v", "view"),
                Hint::new("w", "wrap"),
                Hint::new("e", "edit").available(writable && self.viewer.is_some()),
                Hint::new("d", "delete").available(writable && self.viewer.is_some()),
                Hint::new("Esc", "keys"),
                Hint::new("?", "help"),
                Hint::new("q", "quit"),
            ]
        }
    }

    fn draw_hints(&self, frame: &mut Frame) {
        let mut hints = self.hints();
        if !self.theme.shows_dim() {
            hints.retain(|hint| hint.available);
        }
        let room = usize::from(self.size.cols.saturating_sub(1));
        let width = |hints: &[Hint]| {
            hints
                .iter()
                .map(|hint| text_width(&hint.key) + 1 + text_width(hint.label))
                .sum::<usize>()
                + 2 * hints.len().saturating_sub(1)
        };
        while hints.len() > 1 && width(&hints) > room {
            let help_at = hints.len().saturating_sub(2);
            hints.remove(help_at.saturating_sub(1));
        }
        let row = self.size.rows - 1;
        let mut col = 1u16;
        for (index, hint) in hints.iter().enumerate() {
            let end = self.size.cols;
            if index > 0 {
                col = col.saturating_add(2);
            }
            let key_role = if hint.available {
                Role::Plain
            } else {
                Role::Dim
            };
            col += self.put(
                frame,
                col,
                row,
                &hint.key,
                key_role,
                end.saturating_sub(col),
            );
            col += 1;
            col += self.put(
                frame,
                col,
                row,
                hint.label,
                Role::Dim,
                end.saturating_sub(col),
            );
        }
    }

    fn draw_help(&self, frame: &mut Frame, scroll: usize) {
        let lines = help_lines(self);
        let visible = usize::from(self.size.rows.saturating_sub(2));
        let header = self.theme.style(Role::Header);
        frame.fill(0, 0, self.size.cols, header);
        self.put(frame, 0, 0, " EggDB ui help", Role::Header, self.size.cols);
        let last = (scroll + visible).min(lines.len());
        let position = format!(
            "{}{}{}/{} ",
            scroll + 1,
            self.theme.symbol(Symbol::EnDash),
            last,
            lines.len()
        );
        self.put_right(frame, self.size.cols, 0, &position, Role::Header);
        for (index, line) in lines.iter().skip(scroll).take(visible).enumerate() {
            self.put(
                frame,
                1,
                2 + index as u16,
                line,
                Role::Plain,
                self.size.cols.saturating_sub(1),
            );
        }
    }
}

#[derive(Debug, Clone)]
struct Hint {
    key: String,
    label: &'static str,
    available: bool,
}

impl Hint {
    fn new(key: &str, label: &'static str) -> Self {
        Self {
            key: key.to_owned(),
            label,
            available: true,
        }
    }

    fn available(self, available: bool) -> Self {
        Self { available, ..self }
    }
}

const HELP_EDITING_TOPIC: &str = "Editing";

fn help_editing_line<S: Source>(ui: &Ui<S>) -> usize {
    help_lines(ui)
        .iter()
        .position(|line| line.starts_with(HELP_EDITING_TOPIC))
        .unwrap_or(0)
}

fn help_lines<S: Source>(ui: &Ui<S>) -> Vec<String> {
    let up_down = if ui.theme.unicode() {
        "↑↓"
    } else {
        "Up/Down"
    };
    let left_right = if ui.theme.unicode() {
        "←→"
    } else {
        "Left/Right"
    };
    let mut lines = vec![
        format!("Move        {up_down} or j/k   PgUp/PgDn   Home/End or g/G   {left_right} scroll"),
        "Panes       Tab or Shift-Tab switch   Enter open the value   Esc back to keys".to_owned(),
        "Find        /  filter keys by prefix (bytes; \\xNN for a byte)   Esc restores".to_owned(),
        "View        v  text > json > hex > auto      w  wrap long lines (text)".to_owned(),
        "Change      e  edit value   a  add key   d  delete key (asks d again)".to_owned(),
        "Check       c  verify every page and show stats     r  re-read".to_owned(),
        "Messages    m  show the last message in full".to_owned(),
        "Quit        q, or Ctrl-C (asks before discarding an edit)".to_owned(),
        String::new(),
        format!(
            "{HELP_EDITING_TOPIC}     Enter confirms a one-line value, Ctrl-S confirms any value"
        ),
        "            Alt-Enter makes it multi-line   Esc cancels   F1 this help".to_owned(),
        "            Ctrl-Z/Ctrl-Y undo/redo   Ctrl-U/Ctrl-K delete to line start/end".to_owned(),
        "            Values are edited as bytes: \\xNN is one byte, \\\\ a backslash,".to_owned(),
        "            \\n a line break in a one-line editor; a paste is taken as it is".to_owned(),
        "Every change is one transaction, written to disk before the screen redraws.".to_owned(),
        "Nothing is changed until you confirm. There is no undo after a commit.".to_owned(),
    ];
    if ui.read_only() {
        lines.push(
            "Opened read-only: e, a and d are off; start without --read-only to edit.".to_owned(),
        );
    }
    lines.extend([
        String::new(),
        "Terminal    at least 40x10; two panes from 80 columns".to_owned(),
        "            NO_COLOR, --mono and --ascii change colours and box characters".to_owned(),
    ]);
    if let Ok(summary) = &ui.summary {
        lines.push(format!(
            "Database    {}   page size {}   keys up to {} B   values up to {}",
            summary.name,
            text::file_size(summary.page_size as u64),
            text::count(summary.max_key_bytes as u64),
            text::file_size(summary.max_value_bytes)
        ));
        lines.push(format!(
            "            the editor holds {} of text   page cache {}",
            text::file_size(crate::editor::LONGEST_TEXT as u64),
            text::file_size(summary.cache_bytes)
        ));
    }
    lines.push(format!(
        "Version     EggDB {}   docs github.com/eggspot/EggDB/blob/main/docs/ux/ui.md",
        env!("CARGO_PKG_VERSION")
    ));
    lines
}

fn line_bytes<'a>(viewer: &Viewer, bytes: &'a [u8], line: usize) -> &'a [u8] {
    let Some(&start) = viewer.line_starts.get(line) else {
        return &[];
    };
    let end = viewer.line_starts.get(line + 1).map_or_else(
        || bytes.len() - usize::from(bytes.ends_with(b"\n")),
        |next| next - 1,
    );
    let content = bytes.get(start..end.max(start)).unwrap_or_default();
    content.strip_suffix(b"\r").unwrap_or(content)
}

fn line_starts(bytes: &[u8]) -> (Vec<usize>, bool) {
    let mut starts = vec![0];
    for (index, _) in bytes.iter().enumerate().filter(|(_, b)| **b == b'\n') {
        if index + 1 >= bytes.len() {
            break;
        }
        if starts.len() == MOST_LINES {
            return (starts, true);
        }
        starts.push(index + 1);
    }
    (starts, false)
}

fn json_segments(bytes: &[u8], line: JsonLine) -> Vec<Segment> {
    let mut segments = vec![(" ".repeat(line.indent as usize * 2), Role::Plain)];
    for (token, part) in json::line_parts(bytes, line) {
        let role = match token {
            Token::Punctuation => Role::Plain,
            Token::Key => Role::JsonKey,
            Token::String => Role::JsonString,
            Token::Number => Role::JsonNumber,
            Token::Literal => Role::JsonLiteral,
        };
        segments.push((text::unprintables_escaped(part), role));
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::{MOST_LINES, line_starts};

    #[test]
    fn lines_start_after_each_line_break() {
        assert_eq!(line_starts(b"a\nbc\n"), (vec![0, 2], false));
        assert_eq!(line_starts(b"a\n\nb"), (vec![0, 2, 3], false));
        assert_eq!(line_starts(b""), (vec![0], false));
    }

    #[test]
    fn the_line_index_is_capped() {
        let many = vec![b'\n'; MOST_LINES + 10];
        let (starts, capped) = line_starts(&many);
        assert!(capped);
        assert_eq!(starts.len(), MOST_LINES);
    }
}
