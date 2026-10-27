use crate::decoder::InputDecoder;
use crate::editor::{Editor, Lines, Outcome};
use crate::terminal::{Event, KeyCode, KeyEvent, Modifiers};
use crate::width::{char_width, text_width};
use eggdb_platform::terminal::{Input, RawTerminal};
use std::collections::VecDeque;
use std::io;
use std::time::Duration;

const ESCAPE_WAIT: Duration = Duration::from_millis(25);
const MOST_HISTORY_ENTRIES: usize = 1000;
const MOST_LISTED_CANDIDATES: usize = 200;
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// A terminal a prompt reads one line from: decoded key events in, bytes out.
pub trait LineTerminal {
    /// The next event, waiting as long as it takes.
    fn next_event(&mut self) -> io::Result<Event>;
    /// Writes `bytes` to the terminal as they are.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// The terminal's width in cells.
    fn cols(&self) -> u16;
}

/// The real terminal in raw mode on the normal screen, through the platform layer's
/// [`RawTerminal::enter_inline`], for as long as one line is read.
#[derive(Debug)]
pub struct InlineTerminal {
    raw: RawTerminal,
    decoder: InputDecoder,
    queue: VecDeque<Event>,
    cols: u16,
}

impl InlineTerminal {
    /// Takes the terminal. Fails with [`io::ErrorKind::Unsupported`] when standard input and
    /// output are not both terminals.
    pub fn open() -> io::Result<Self> {
        let raw = RawTerminal::enter_inline()?;
        let cols = raw.size().map_or(80, |size| size.cols.max(1));
        Ok(Self {
            raw,
            decoder: InputDecoder::new(),
            queue: VecDeque::new(),
            cols,
        })
    }
}

impl LineTerminal for InlineTerminal {
    fn next_event(&mut self) -> io::Result<Event> {
        let mut buffer = [0u8; 4096];
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(event);
            }
            let wait = if self.decoder.has_pending() {
                ESCAPE_WAIT
            } else {
                Duration::from_secs(3600)
            };
            match self.raw.read_input(&mut buffer, wait)? {
                Input::Bytes(count) => {
                    let events = self.decoder.feed(&buffer[..count.min(buffer.len())]);
                    self.queue.extend(events);
                }
                Input::Resized => {
                    if let Ok(size) = self.raw.size() {
                        self.cols = size.cols.max(1);
                    }
                    return Ok(Event::Resize(crate::terminal::Size {
                        cols: self.cols,
                        rows: 1,
                    }));
                }
                Input::Closed => return Ok(Event::Interrupt),
                Input::TimedOut => {
                    let events = self.decoder.flush();
                    self.queue.extend(events);
                }
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.raw.write_all(bytes)
    }

    fn cols(&self) -> u16 {
        self.cols
    }
}

/// How reading a line ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEnd {
    /// `Enter` sent this line.
    Line(String),
    /// `Ctrl-C` cleared the line.
    Cleared,
    /// `Ctrl-D` on an empty line, or the terminal went away.
    EndOfInput,
}

/// What `Tab` may put in place of the word before the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    /// The byte offset where the word being completed starts.
    pub start: usize,
    /// The words that may replace it, in the order to list them.
    pub candidates: Vec<String>,
    /// Whether a single candidate is followed by a space.
    pub add_space: bool,
}

#[derive(Debug)]
struct Search {
    query: String,
    found: Option<usize>,
    draft: String,
}

/// A prompt's line editor: the UI's single-line editor with the readline keys, a history
/// recalled with `Up`, `Down` and `Ctrl-R`, and `Tab` completion.
#[derive(Debug)]
pub struct LineReader {
    history: Vec<String>,
    styled: bool,
    pending: VecDeque<String>,
    carried: String,
}

impl LineReader {
    /// A reader that starts with `history`, oldest first; `styled` draws the prompt bold and
    /// listings dim.
    pub fn new(history: Vec<String>, styled: bool) -> Self {
        let skip = history.len().saturating_sub(MOST_HISTORY_ENTRIES);
        Self {
            history: history.into_iter().skip(skip).collect(),
            styled,
            pending: VecDeque::new(),
            carried: String::new(),
        }
    }

    /// The history, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Adds `entry` to the history unless it is empty, starts with a space or repeats the
    /// newest entry; returns whether it was added.
    pub fn remember(&mut self, entry: &str) -> bool {
        if entry.trim().is_empty()
            || entry.starts_with(' ')
            || self.history.last().is_some_and(|last| last == entry)
        {
            return false;
        }
        self.history.push(entry.to_owned());
        if self.history.len() > MOST_HISTORY_ENTRIES {
            self.history.remove(0);
        }
        true
    }

    /// Reads one line after `prompt`, asking `complete` what `Tab` offers for the text and
    /// cursor offset it is given.
    pub fn read_line(
        &mut self,
        terminal: &mut dyn LineTerminal,
        prompt: &str,
        complete: &mut dyn FnMut(&str, usize) -> Completion,
    ) -> io::Result<LineEnd> {
        if let Some(line) = self.pending.pop_front() {
            let shown = format!("{}{line}\r\n", self.prompt_text(prompt));
            terminal.write(shown.as_bytes())?;
            return Ok(LineEnd::Line(line));
        }
        let mut editor = Editor::new(std::mem::take(&mut self.carried), Lines::Single);
        let mut recalled = self.history.len();
        let mut draft = String::new();
        let mut last_was_tab = false;
        let mut search: Option<Search> = None;
        self.draw(terminal, prompt, &mut editor, search.as_ref())?;
        loop {
            let event = terminal.next_event()?;
            let key = match event {
                Event::Key(key) => key,
                Event::Paste(text) => {
                    if let Some(line) = self.paste(&mut editor, &text) {
                        self.draw(terminal, prompt, &mut editor, None)?;
                        terminal.write(b"\r\n")?;
                        return Ok(LineEnd::Line(line));
                    }
                    self.draw(terminal, prompt, &mut editor, None)?;
                    continue;
                }
                Event::Interrupt => {
                    terminal.write(b"\r\n")?;
                    return Ok(LineEnd::EndOfInput);
                }
                Event::Resize(_) | Event::PasteTooLarge => {
                    self.draw(terminal, prompt, &mut editor, search.as_ref())?;
                    continue;
                }
            };
            if let Some(mut active) = search.take() {
                match self.search_key(&mut active, key) {
                    SearchStep::Continue => search = Some(active),
                    SearchStep::Keep => {
                        let text = active.found.map_or(active.draft, |index| {
                            self.history.get(index).cloned().unwrap_or_default()
                        });
                        editor = Editor::new(one_line(&text), Lines::Single);
                    }
                    SearchStep::Send => {
                        let text = active.found.map_or(active.draft, |index| {
                            self.history.get(index).cloned().unwrap_or_default()
                        });
                        let mut sent = Editor::new(one_line(&text), Lines::Single);
                        self.draw(terminal, prompt, &mut sent, None)?;
                        terminal.write(b"\r\n")?;
                        return Ok(LineEnd::Line(sent.text().to_owned()));
                    }
                    SearchStep::Restore => editor = Editor::new(active.draft, Lines::Single),
                }
                self.draw(terminal, prompt, &mut editor, search.as_ref())?;
                continue;
            }
            let tab = key.code == KeyCode::Tab && !key.modifiers.ctrl && !key.modifiers.alt;
            let ctrl = key.modifiers.ctrl;
            match key.code {
                KeyCode::Enter => {
                    let line = editor.text().to_owned();
                    let end = editor.text().len();
                    editor.move_to(end);
                    self.draw(terminal, prompt, &mut editor, None)?;
                    terminal.write(b"\r\n")?;
                    return Ok(LineEnd::Line(line));
                }
                KeyCode::Char('c') if ctrl => {
                    let mark = if self.styled {
                        "\x1b[2m^C\x1b[0m"
                    } else {
                        "^C"
                    };
                    terminal.write(format!("{mark}\r\n").as_bytes())?;
                    return Ok(LineEnd::Cleared);
                }
                KeyCode::Char('d') if ctrl => {
                    if editor.text().is_empty() {
                        terminal.write(b"\r\n")?;
                        return Ok(LineEnd::EndOfInput);
                    }
                }
                KeyCode::Char('l') if ctrl => terminal.write(b"\x1b[H\x1b[2J")?,
                KeyCode::Char('w') if ctrl => {
                    editor.delete_word_before();
                }
                KeyCode::Char('r') if ctrl => {
                    search = Some(Search {
                        query: String::new(),
                        found: None,
                        draft: editor.text().to_owned(),
                    });
                }
                KeyCode::Char('b') if key.modifiers.alt => {
                    editor.key(word_key(KeyCode::Left), 1);
                }
                KeyCode::Char('f') if key.modifiers.alt => {
                    editor.key(word_key(KeyCode::Right), 1);
                }
                KeyCode::Up => {
                    if recalled > 0 {
                        if recalled == self.history.len() {
                            draft = editor.text().to_owned();
                        }
                        recalled -= 1;
                        editor = Editor::new(one_line(&self.history[recalled]), Lines::Single);
                    }
                }
                KeyCode::Down => {
                    if recalled < self.history.len() {
                        recalled += 1;
                        let text = self
                            .history
                            .get(recalled)
                            .map_or_else(|| draft.clone(), |entry| one_line(entry));
                        editor = Editor::new(text, Lines::Single);
                    }
                }
                KeyCode::Tab if tab => {
                    let completion = complete(editor.text(), editor.cursor());
                    self.complete(terminal, &mut editor, &completion, last_was_tab)?;
                }
                KeyCode::Esc | KeyCode::F(_) | KeyCode::BackTab | KeyCode::Insert => {}
                KeyCode::PageUp | KeyCode::PageDown => {}
                _ => {
                    if editor.key(key, 1) == Outcome::TooLong {
                        terminal.write(b"\x07")?;
                    }
                }
            }
            last_was_tab = tab;
            self.draw(terminal, prompt, &mut editor, search.as_ref())?;
        }
    }

    fn paste(&mut self, editor: &mut Editor, text: &str) -> Option<String> {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut parts = text.split('\n');
        let first = parts.next().unwrap_or_default();
        editor.paste_on_one_line(first);
        let rest: Vec<&str> = parts.collect();
        let (last, middle) = rest.split_last()?;
        for line in middle {
            self.pending.push_back((*line).to_owned());
        }
        self.carried = (*last).to_owned();
        Some(editor.text().to_owned())
    }

    fn complete(
        &self,
        terminal: &mut dyn LineTerminal,
        editor: &mut Editor,
        completion: &Completion,
        list: bool,
    ) -> io::Result<()> {
        let cursor = editor.cursor();
        let start = completion.start.min(cursor);
        let typed = editor
            .text()
            .get(start..cursor)
            .unwrap_or_default()
            .to_owned();
        match completion.candidates.as_slice() {
            [] => {}
            [only] => {
                let space = if completion.add_space { " " } else { "" };
                editor.replace(start, cursor, &format!("{only}{space}"));
            }
            many => {
                let common = common_prefix(many);
                if common.chars().count() > typed.chars().count() {
                    editor.replace(start, cursor, &common);
                } else if list {
                    terminal.write(b"\r\n")?;
                    let listing = columns(many, usize::from(terminal.cols()));
                    let listing = if self.styled {
                        listing
                    } else {
                        listing.replace(DIM, "").replace(RESET, "")
                    };
                    terminal.write(listing.as_bytes())?;
                }
            }
        }
        Ok(())
    }

    fn search_key(&self, search: &mut Search, key: KeyEvent) -> SearchStep {
        let ctrl = key.modifiers.ctrl;
        match key.code {
            KeyCode::Char('r') if ctrl => {
                let before = search.found.unwrap_or(self.history.len());
                if let Some(found) = self.find(&search.query, before) {
                    search.found = Some(found);
                }
                SearchStep::Continue
            }
            KeyCode::Char('c') if ctrl => SearchStep::Restore,
            KeyCode::Esc => SearchStep::Restore,
            KeyCode::Enter => SearchStep::Send,
            KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End => SearchStep::Keep,
            KeyCode::Backspace => {
                search.query.pop();
                search.found = self.find(&search.query, self.history.len());
                SearchStep::Continue
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.alt => {
                search.query.push(c);
                let from = search.found.map_or(self.history.len(), |found| found + 1);
                search.found = self.find(&search.query, from);
                SearchStep::Continue
            }
            _ => SearchStep::Continue,
        }
    }

    fn find(&self, query: &str, before: usize) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        self.history[..before.min(self.history.len())]
            .iter()
            .rposition(|entry| entry.contains(query))
    }

    fn prompt_text(&self, prompt: &str) -> String {
        if self.styled {
            format!("{BOLD}{prompt}{RESET}")
        } else {
            prompt.to_owned()
        }
    }

    fn draw(
        &self,
        terminal: &mut dyn LineTerminal,
        prompt: &str,
        editor: &mut Editor,
        search: Option<&Search>,
    ) -> io::Result<()> {
        let cols = usize::from(terminal.cols()).max(2);
        let (prompt, styled_prompt, text, cursor_cells) = match search {
            Some(search) => {
                let failed = !search.query.is_empty() && search.found.is_none();
                let label = format!(
                    "({}reverse-i-search)'{}': ",
                    if failed { "failed " } else { "" },
                    search.query
                );
                let found = search
                    .found
                    .and_then(|index| self.history.get(index))
                    .map(|entry| one_line(entry))
                    .unwrap_or_default();
                (label.clone(), label, found, 0)
            }
            None => {
                let room = cols.saturating_sub(text_width(prompt) + 1).max(1);
                editor.scroll_into_view(1, room);
                let pan = editor.pan();
                (
                    prompt.to_owned(),
                    self.prompt_text(prompt),
                    editor.text().to_owned(),
                    editor.cursor_cells().saturating_sub(pan),
                )
            }
        };
        let room = cols.saturating_sub(text_width(&prompt) + 1).max(1);
        let pan = if search.is_some() { 0 } else { editor.pan() };
        let shown = cells_window(&text, pan, room);
        let mut out = format!("\r{styled_prompt}{shown}\x1b[K\r");
        let to = text_width(&prompt) + if search.is_some() { 0 } else { cursor_cells };
        if to > 0 {
            out.push_str(&format!("\x1b[{to}C"));
        }
        terminal.write(out.as_bytes())
    }
}

enum SearchStep {
    Continue,
    Keep,
    Send,
    Restore,
}

fn word_key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: Modifiers::CTRL,
    }
}

fn one_line(entry: &str) -> String {
    entry.replace('\n', " ")
}

fn cells_window(text: &str, skip: usize, room: usize) -> String {
    let mut shown = String::new();
    let mut at = 0;
    let mut used = 0;
    for character in text.chars() {
        let width = char_width(character);
        if at >= skip {
            if used + width > room {
                break;
            }
            shown.push(character);
            used += width;
        }
        at += width;
    }
    shown
}

fn common_prefix(words: &[String]) -> String {
    let Some(first) = words.first() else {
        return String::new();
    };
    let mut end = first.len();
    for word in &words[1..] {
        end = first
            .char_indices()
            .zip(word.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((index, a), _)| index + a.len_utf8())
            .min(end);
    }
    first[..end].to_owned()
}

fn columns(words: &[String], cols: usize) -> String {
    let shown = &words[..words.len().min(MOST_LISTED_CANDIDATES)];
    let width = shown.iter().map(|word| text_width(word)).max().unwrap_or(0) + 2;
    let per_row = (cols / width.max(1)).max(1);
    let mut out = String::new();
    for row in shown.chunks(per_row) {
        for word in row {
            out.push_str(word);
            out.push_str(&" ".repeat(width - text_width(word)));
        }
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        out.push_str("\r\n");
    }
    if words.len() > shown.len() {
        out.push_str(&format!(
            "{DIM}\u{2026} and {} more{RESET}\r\n",
            words.len() - shown.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Completion, LineEnd, LineReader, LineTerminal, common_prefix};
    use crate::terminal::{Event, KeyCode, KeyEvent, Modifiers};
    use std::collections::VecDeque;
    use std::io;

    struct Script {
        events: VecDeque<Event>,
        written: Vec<u8>,
    }

    impl Script {
        fn new(events: Vec<Event>) -> Self {
            Self {
                events: events.into(),
                written: Vec::new(),
            }
        }
    }

    impl LineTerminal for Script {
        fn next_event(&mut self) -> io::Result<Event> {
            Ok(self.events.pop_front().unwrap_or(Event::Interrupt))
        }
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.written.extend_from_slice(bytes);
            Ok(())
        }
        fn cols(&self) -> u16 {
            40
        }
    }

    fn keys(text: &str) -> Vec<Event> {
        text.chars()
            .map(|c| Event::Key(KeyEvent::plain(KeyCode::Char(c))))
            .collect()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::plain(code))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: Modifiers::CTRL,
        })
    }

    fn alt(c: char) -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: Modifiers::ALT,
        })
    }

    fn read(reader: &mut LineReader, events: Vec<Event>) -> LineEnd {
        read_completing(reader, events, &mut |_, _| Completion::default())
    }

    fn read_completing(
        reader: &mut LineReader,
        events: Vec<Event>,
        complete: &mut dyn FnMut(&str, usize) -> Completion,
    ) -> LineEnd {
        let mut script = Script::new(events);
        reader
            .read_line(&mut script, "app=> ", complete)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn line(text: &str) -> LineEnd {
        LineEnd::Line(text.to_owned())
    }

    #[test]
    fn typing_and_enter_send_the_line() {
        let mut reader = LineReader::new(Vec::new(), false);
        let mut events = keys("select 1;");
        events.push(key(KeyCode::Enter));
        assert_eq!(read(&mut reader, events), line("select 1;"));
    }

    #[test]
    fn the_readline_keys_move_and_delete() {
        let mut reader = LineReader::new(Vec::new(), false);
        let mut events = keys("one two three");
        events.extend([ctrl('w'), ctrl('a'), alt('f'), ctrl('k')]);
        events.extend(keys("!"));
        events.extend([ctrl('e'), alt('b'), ctrl('u'), key(KeyCode::Enter)]);
        assert_eq!(read(&mut reader, events), line("one!"));
        let mut events = keys("abc");
        events.extend([key(KeyCode::Home), key(KeyCode::Delete), key(KeyCode::End)]);
        events.extend([key(KeyCode::Backspace), key(KeyCode::Enter)]);
        assert_eq!(read(&mut reader, events), line("b"));
    }

    #[test]
    fn ctrl_c_clears_and_ctrl_d_ends_only_an_empty_line() {
        let mut reader = LineReader::new(Vec::new(), false);
        let mut events = keys("half");
        events.push(ctrl('c'));
        assert_eq!(read(&mut reader, events), LineEnd::Cleared);
        let mut events = keys("x");
        events.extend([ctrl('d'), key(KeyCode::Enter)]);
        assert_eq!(read(&mut reader, events), line("x"));
        assert_eq!(read(&mut reader, vec![ctrl('d')]), LineEnd::EndOfInput);
    }

    #[test]
    fn up_and_down_recall_history_and_keep_the_draft() {
        let mut reader = LineReader::new(vec!["first".into(), "second\nline".into()], false);
        let mut events = keys("draft");
        events.extend([key(KeyCode::Up), key(KeyCode::Enter)]);
        assert_eq!(read(&mut reader, events), line("second line"));
        let mut events = keys("draft");
        events.extend([key(KeyCode::Up), key(KeyCode::Up), key(KeyCode::Down)]);
        events.extend([key(KeyCode::Down), key(KeyCode::Enter)]);
        assert_eq!(read(&mut reader, events), line("draft"));
    }

    #[test]
    fn remember_skips_empty_spaced_and_repeated_entries() {
        let mut reader = LineReader::new(Vec::new(), false);
        assert!(reader.remember("a"));
        assert!(!reader.remember("a"));
        assert!(!reader.remember(" secret"));
        assert!(!reader.remember("  "));
        assert!(reader.remember("b"));
        assert_eq!(reader.history(), ["a", "b"]);
    }

    #[test]
    fn ctrl_r_finds_the_newest_match_and_older_ones() {
        let history = vec!["select 1;".into(), "put a 1".into(), "select 2;".into()];
        let mut reader = LineReader::new(history, false);
        let mut events = vec![ctrl('r')];
        events.extend(keys("sel"));
        events.push(key(KeyCode::Enter));
        assert_eq!(read(&mut reader, events), line("select 2;"));
        let mut events = vec![ctrl('r')];
        events.extend(keys("sel"));
        events.extend([ctrl('r'), key(KeyCode::End), key(KeyCode::Enter)]);
        assert_eq!(read(&mut reader, events), line("select 1;"));
        let mut events = keys("kept");
        events.push(ctrl('r'));
        events.extend(keys("zzz"));
        events.extend([key(KeyCode::Esc), key(KeyCode::Enter)]);
        assert_eq!(read(&mut reader, events), line("kept"));
    }

    #[test]
    fn tab_completes_one_candidate_and_the_common_prefix_of_many() {
        let mut reader = LineReader::new(Vec::new(), false);
        let mut complete = |text: &str, cursor: usize| {
            let start = text[..cursor].rfind(' ').map_or(0, |at| at + 1);
            let word = &text[start..cursor];
            Completion {
                start,
                candidates: ["users", "user_roles", "orders"]
                    .iter()
                    .filter(|name| name.starts_with(word))
                    .map(|name| (*name).to_owned())
                    .collect(),
                add_space: true,
            }
        };
        let mut events = keys("from o");
        events.extend([key(KeyCode::Tab), key(KeyCode::Enter)]);
        assert_eq!(
            read_completing(&mut reader, events, &mut complete),
            line("from orders ")
        );
        let mut events = keys("from u");
        events.extend([key(KeyCode::Tab), key(KeyCode::Tab), key(KeyCode::Enter)]);
        let mut script = Script::new(events);
        let end = reader.read_line(&mut script, "> ", &mut complete);
        assert_eq!(end.ok(), Some(line("from user")));
        let written = String::from_utf8_lossy(&script.written).into_owned();
        assert!(
            written.contains("\r\nusers       user_roles\r\n"),
            "{written}"
        );
    }

    #[test]
    fn a_paste_with_line_breaks_sends_its_lines_one_by_one() {
        let mut reader = LineReader::new(Vec::new(), false);
        let events = vec![Event::Paste("select 1\nfrom t\nwhere".into())];
        assert_eq!(read(&mut reader, events), line("select 1"));
        assert_eq!(read(&mut reader, Vec::new()), line("from t"));
        let events = vec![key(KeyCode::Enter)];
        assert_eq!(read(&mut reader, events), line("where"));
    }

    #[test]
    fn the_common_prefix_respects_characters() {
        let words = vec!["éa".to_owned(), "éb".to_owned()];
        assert_eq!(common_prefix(&words), "é");
        assert_eq!(common_prefix(&["x".to_owned()]), "x");
    }
}
