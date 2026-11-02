use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eggdb_platform::window::{KeyPress, OpenFileDialog};

use crate::clipboard::{Clipboard, MemoryClipboard};
use crate::connect_screen::{ConnectAction, ConnectCard, Segment, handle_key};
use crate::connection::{
    CancelHandle, ConnectFailure, ConnectTarget, Connection, Connector, ResultTable,
    StatementFailure,
};
use crate::catalog::Catalog;
use crate::inspector;
use crate::main_window::{MainWindow, WindowAction};
use crate::object_browser::TreeItem;
use crate::query_tab::{RunRequest, TabAction};

/// How long an open runs before the card shows its progress state, so a quick open never
/// flickers.
pub const PROGRESS_DELAY: Duration = Duration::from_millis(120);
/// How long `Cancelled` stays on the status line.
pub const CANCELLED_SHOWN_FOR: Duration = Duration::from_secs(2);

/// How long a notice such as `Copied 12 cells` stays in the status bar.
pub const NOTICE_SHOWN_FOR: Duration = Duration::from_secs(2);

type RunAnswer = (Connection, Vec<Result<Vec<ResultTable>, StatementFailure>>);

/// What the window around the workspace must do after an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCommand {
    /// Nothing beyond drawing the new frame.
    None,
    /// Show the OS's open-file dialog and give its answer to [`Workspace::file_chosen`].
    ChooseFile(OpenFileDialog),
    /// Quit the client.
    Quit,
}

/// The screen the workspace shows.
#[derive(Debug)]
pub enum Screen {
    /// The connect card, until a database opens.
    Connect(ConnectCard),
    /// The main window on the open database.
    Main(Box<MainWindow>),
}

struct Running {
    request: RunRequest,
    started: Instant,
    cancel: CancelHandle,
    answer: Receiver<RunAnswer>,
}

struct Opening {
    target: ConnectTarget,
    started: Instant,
    answer: Receiver<Result<Connection, ConnectFailure>>,
}

/// The client's state from the connect card to the main window: it turns the card's actions
/// into opens on a [`Connector`], runs each open off the window's thread so `Esc` can stop
/// it, and holds the [`Connection`] the main window works on.
pub struct Workspace {
    screen: Screen,
    connection: Option<Connection>,
    connector: Arc<dyn Connector>,
    opening: Option<Opening>,
    running: Option<Running>,
    status_until: Option<Instant>,
    notice_until: Option<Instant>,
    clipboard: Box<dyn Clipboard>,
    home: Option<PathBuf>,
    current: PathBuf,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Workspace")
            .field("screen", &self.screen)
            .field("connection", &self.connection)
            .field("opening", &self.opening.is_some())
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// The first screen's card on `connector`, resolving typed paths against `home` and
    /// `current`; `file_dialog` says whether the platform has an open-file dialog.
    pub fn new(
        connector: Arc<dyn Connector>,
        home: Option<PathBuf>,
        current: PathBuf,
        file_dialog: bool,
    ) -> Self {
        let mut card = ConnectCard::first_start();
        card.file_dialog = file_dialog;
        Self {
            screen: Screen::Connect(card),
            connection: None,
            connector,
            opening: None,
            running: None,
            status_until: None,
            notice_until: None,
            clipboard: Box::new(MemoryClipboard::default()),
            home,
            current,
        }
    }

    /// The screen to draw.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The connect card, while it is shown.
    pub fn card_mut(&mut self) -> Option<&mut ConnectCard> {
        match &mut self.screen {
            Screen::Connect(card) => Some(card),
            Screen::Main(_) => None,
        }
    }

    /// The open database, once the main window shows it.
    pub fn connection_mut(&mut self) -> Option<&mut Connection> {
        self.connection.as_mut()
    }

    /// Whether an open is running.
    pub fn is_opening(&self) -> bool {
        self.opening.is_some()
    }

    /// Whether a query runs.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The clipboard copies go to.
    pub fn clipboard(&self) -> &dyn Clipboard {
        self.clipboard.as_ref()
    }

    /// Sends copies to `clipboard`, as the platform layer's system clipboard.
    pub fn set_clipboard(&mut self, clipboard: Box<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    /// The main window, once it shows.
    pub fn main_window_mut(&mut self) -> Option<&mut MainWindow> {
        match &mut self.screen {
            Screen::Main(window) => Some(window),
            Screen::Connect(_) => None,
        }
    }

    /// Answers a key press.
    pub fn key(&mut self, press: KeyPress, now: Instant) -> WorkspaceCommand {
        if let Screen::Main(window) = &mut self.screen {
            let action = window.key(press, now);
            self.window_action(action, now);
            return WorkspaceCommand::None;
        }
        let Screen::Connect(card) = &mut self.screen else {
            return WorkspaceCommand::None;
        };
        let (next, action) = handle_key(card.clone(), press);
        *card = next;
        self.act(action, now)
    }

    fn window_action(&mut self, action: WindowAction, now: Instant) {
        match action {
            WindowAction::Tab(action) => self.tab_action(action, now),
            WindowAction::Inspect(item) => self.inspect(item),
            WindowAction::RefreshCatalog => self.refresh_catalog(),
        }
    }

    fn inspect(&mut self, item: TreeItem) {
        let (Screen::Main(window), Some(connection)) = (&mut self.screen, self.connection.as_mut()) else {
            return;
        };
        let Some(name) = inspector::sampled_name(&window.tree, item) else {
            return;
        };
        if window.inspector.sample.as_ref().is_some_and(|sample| sample.of == name) {
            return;
        }
        window.inspector.sample = Some(inspector::fetch_sample(connection, &name));
    }

    /// Reads the catalog again and shows it in the tree, keeping its expanded, current and
    /// selected rows; nothing happens while a run holds the connection.
    pub fn refresh_catalog(&mut self) {
        let (Screen::Main(window), Some(connection)) = (&mut self.screen, self.connection.as_mut()) else {
            return;
        };
        if let Ok(catalog) = connection.catalog() {
            window.tree.set_catalog(catalog);
            window.inspector.sample = None;
        }
    }

    fn tab_action(&mut self, action: TabAction, now: Instant) {
        match action {
            TabAction::None => {
                if self.main_window_mut().is_some_and(|window| window.tab.notice.is_some()) {
                    self.notice_until.get_or_insert(now + NOTICE_SHOWN_FOR);
                }
            }
            TabAction::Run(request) => self.start_run(request, now),
            TabAction::Stop => {
                if let Some(running) = &self.running {
                    running.cancel.cancel();
                }
                if let Some(window) = self.main_window_mut() {
                    window.tab.stop_requested();
                }
            }
            TabAction::Copy { text, toast } => {
                self.clipboard.put_text(&text);
                if let Some(window) = self.main_window_mut() {
                    window.tab.notice = Some(toast);
                }
                self.notice_until = Some(now + NOTICE_SHOWN_FOR);
            }
        }
    }

    fn start_run(&mut self, request: RunRequest, now: Instant) {
        let Some(mut connection) = self.connection.take() else {
            return;
        };
        let (send, answer) = mpsc::channel();
        let cancel = CancelHandle::default();
        let worker_cancel = cancel.clone();
        let statements = request.statements.clone();
        let dialect = request.dialect;
        std::thread::spawn(move || {
            let outcomes = connection.run_script(dialect, &statements, &worker_cancel);
            let _ = send.send((connection, outcomes));
        });
        if let Some(window) = self.main_window_mut() {
            window.tab.started();
        }
        self.running = Some(Running {
            request,
            started: now,
            cancel,
            answer,
        });
    }

    fn finish_run(&mut self, running: Running, answer: RunAnswer, now: Instant) {
        let (connection, outcomes) = answer;
        self.connection = Some(connection);
        let elapsed = now.saturating_duration_since(running.started);
        let changed_schema = outcomes
            .iter()
            .zip(&running.request.statements)
            .any(|(outcome, statement)| outcome.is_ok() && changes_schema(statement));
        if let Some(window) = self.main_window_mut() {
            window.tab.finish(&running.request, outcomes, elapsed);
        }
        if changed_schema {
            self.refresh_catalog();
        }
    }

    fn tick_run(&mut self, now: Instant) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        match running.answer.try_recv() {
            Ok(answer) => {
                if let Some(running) = self.running.take() {
                    self.finish_run(running, answer, now);
                }
                true
            }
            Err(TryRecvError::Disconnected) => {
                self.running = None;
                if let Some(window) = self.main_window_mut() {
                    window.tab.running = false;
                    window.tab.stopping = false;
                    window.tab.summary = "Error".to_owned();
                }
                true
            }
            Err(TryRecvError::Empty) => {
                let elapsed = now.saturating_duration_since(running.started);
                self.main_window_mut()
                    .is_some_and(|window| window.tab.show_running(elapsed))
            }
        }
    }

    /// Waits for the running query to end, up to `limit`, and takes its outcome; for scripted
    /// runs and tests that render a run's result.
    pub fn wait_for_run(&mut self, limit: Duration) {
        let Some(running) = self.running.take() else {
            return;
        };
        match running.answer.recv_timeout(limit) {
            Ok(answer) => self.finish_run(running, answer, Instant::now()),
            Err(_) => self.running = Some(running),
        }
    }

    /// Puts `text` in the editor, the caret at its end, and runs it as `Ctrl/Cmd+Enter`
    /// (or, with `all`, `Ctrl/Cmd+Shift+Enter`) would; for scripted runs.
    pub fn run_text(&mut self, text: &str, all: bool, now: Instant) {
        let Some(window) = self.main_window_mut() else {
            return;
        };
        window.tab.editor = crate::editor::Editor::new(text);
        if let Some(request) = window.tab.run_request(all) {
            self.start_run(request, now);
        }
    }

    fn act(&mut self, action: ConnectAction, now: Instant) -> WorkspaceCommand {
        match action {
            ConnectAction::Quit => WorkspaceCommand::Quit,
            ConnectAction::Open => {
                self.start_open(now);
                WorkspaceCommand::None
            }
            ConnectAction::Browse => match &self.screen {
                Screen::Connect(card) => WorkspaceCommand::ChooseFile(
                    card.file_dialog_request(self.home.as_deref(), &self.current),
                ),
                Screen::Main(_) => WorkspaceCommand::None,
            },
            ConnectAction::StopOpening => {
                self.opening = None;
                if let Some(card) = self.card_mut() {
                    card.stop_opening();
                }
                self.status_until = Some(now + CANCELLED_SHOWN_FOR);
                WorkspaceCommand::None
            }
            ConnectAction::None
            | ConnectAction::Cancel
            | ConnectAction::ShowLayoutPicker
            | ConnectAction::LoadSampleDataset => WorkspaceCommand::None,
        }
    }

    /// Puts `typed` in the File input and opens it with the checkboxes as set, as typing
    /// it and pressing `Enter` does; for scripted runs.
    pub fn open_typed(&mut self, typed: &str, now: Instant) {
        if let Some(card) = self.card_mut() {
            card.show_segment(Segment::File);
            card.file.set(typed);
        }
        self.start_open(now);
    }

    /// Takes the open-file dialog's answer: the chosen path, or `None` when it was cancelled.
    pub fn file_chosen(&mut self, chosen: Option<&Path>) {
        if let Some(card) = self.card_mut() {
            card.choose_file(chosen);
        }
    }

    /// A drag carrying `paths` came over the window.
    pub fn drag_entered(&mut self, paths: &[PathBuf]) {
        if let Some(card) = self.card_mut() {
            card.drag_entered(paths);
        }
    }

    /// The drag left the window without a drop.
    pub fn drag_left(&mut self) {
        if let Some(card) = self.card_mut() {
            card.drag_over = false;
        }
    }

    /// Files were dropped onto the window: the first one opens at once.
    pub fn dropped(&mut self, paths: &[PathBuf], now: Instant) {
        let action = match self.card_mut() {
            Some(card) => card.drop_files(paths),
            None => ConnectAction::None,
        };
        if action == ConnectAction::Open {
            self.start_open(now);
        }
    }

    fn start_open(&mut self, now: Instant) {
        let Screen::Connect(card) = &mut self.screen else {
            return;
        };
        let target = match card.segment {
            Segment::File => {
                ConnectTarget::File(card.file_target(self.home.as_deref(), &self.current))
            }
            Segment::Server => match card.server.connect_options() {
                Ok(options) => ConnectTarget::Server(options),
                Err(failure) => {
                    card.begin_opening();
                    card.finish_with_failure(&failure);
                    return;
                }
            },
        };
        card.begin_opening();
        let (sender, answer) = mpsc::channel();
        let connector = Arc::clone(&self.connector);
        let request = target.clone();
        std::thread::spawn(move || {
            let _ = sender.send(connector.connect(&request));
        });
        self.opening = Some(Opening {
            target,
            started: now,
            answer,
        });
    }

    /// Moves time on to `now`: takes a finished open's answer, shows the progress state
    /// after [`PROGRESS_DELAY`] and clears `Cancelled` after [`CANCELLED_SHOWN_FOR`].
    /// Returns whether the screen changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = self.tick_run(now);
        if self.notice_until.is_some_and(|until| now >= until) {
            self.notice_until = None;
            if let Some(window) = self.main_window_mut() {
                window.tab.notice = None;
            }
            changed = true;
        }
        if self.status_until.is_some_and(|until| now >= until) {
            self.status_until = None;
            if let Some(card) = self.card_mut() {
                card.status = None;
            }
            changed = true;
        }
        let Some(opening) = &self.opening else {
            return changed;
        };
        match opening.answer.try_recv() {
            Ok(answer) => {
                self.finish(answer);
                true
            }
            Err(TryRecvError::Disconnected) => {
                self.opening = None;
                if let Some(card) = self.card_mut() {
                    card.busy = false;
                    card.progress = None;
                }
                true
            }
            Err(TryRecvError::Empty) => {
                let late = now.saturating_duration_since(opening.started) >= PROGRESS_DELAY;
                let target = opening.target.clone();
                match self.card_mut() {
                    Some(card) if late && card.progress.is_none() => {
                        card.show_progress(&target);
                        true
                    }
                    _ => changed,
                }
            }
        }
    }

    /// Waits for the running open to end, up to `limit`, and takes its answer; for scripted
    /// runs that render the outcome of one open.
    pub fn wait_for_open(&mut self, limit: Duration) {
        let Some(opening) = &self.opening else {
            return;
        };
        match opening.answer.recv_timeout(limit) {
            Ok(answer) => self.finish(answer),
            Err(_) => self.opening = None,
        }
    }

    fn finish(&mut self, answer: Result<Connection, ConnectFailure>) {
        self.opening = None;
        match answer {
            Ok(mut connection) => {
                self.screen = Screen::Main(Box::new(main_window_for(&mut connection)));
                self.connection = Some(connection);
            }
            Err(failure) => {
                if let Some(card) = self.card_mut() {
                    card.finish_with_failure(&failure);
                }
            }
        }
    }
}

/// The main window as it first shows `connection` (`docs/ux/workspace.md` 1.7): its name and
/// location in the toolbar, its catalog in the object tree, one empty `Query 1` editor.
pub fn main_window_for(connection: &mut Connection) -> MainWindow {
    let writable = !connection.read_only();
    let catalog = connection
        .catalog()
        .unwrap_or_else(|_| Catalog::of_tables(connection.table_names().unwrap_or_default()));
    MainWindow::with_catalog(connection.name(), connection.location(), writable, catalog)
}

/// Whether `statement` may change what the object tree lists: a `CREATE`, `ALTER`, `DROP`
/// or `RENAME`, or SQL Server's `sp_rename`.
pub fn changes_schema(statement: &str) -> bool {
    let first: String = statement
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    match first.to_ascii_uppercase().as_str() {
        "CREATE" | "ALTER" | "DROP" | "RENAME" => true,
        "EXEC" | "EXECUTE" | "SP_RENAME" => statement.to_ascii_lowercase().contains("sp_rename"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect_screen::{Focus, Segment};
    use crate::connection::{EngineConnector, FailureField};
    use eggdb_platform::window::Key;

    #[derive(Debug)]
    struct SlowConnector(Duration);

    impl Connector for SlowConnector {
        fn connect(&self, target: &ConnectTarget) -> Result<Connection, ConnectFailure> {
            std::thread::sleep(self.0);
            match target {
                ConnectTarget::File(file) => Connection::in_memory(
                    &file.path.file_name().unwrap_or_default().to_string_lossy(),
                ),
                ConnectTarget::Server(_) => Err(crate::connection::url_failure()),
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("eggdb-gui-workspace-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        directory
    }

    fn workspace(connector: Arc<dyn Connector>, current: PathBuf) -> Workspace {
        Workspace::new(connector, Some(PathBuf::from("/home/nobody")), current, true)
    }

    fn type_text(workspace: &mut Workspace, text: &str, now: Instant) {
        for character in text.chars() {
            workspace.key(KeyPress::plain(Key::Character(character)), now);
        }
    }

    fn card(workspace: &mut Workspace) -> ConnectCard {
        workspace.card_mut().cloned().expect("the card is shown")
    }

    #[test]
    fn a_typed_relative_path_and_enter_open_the_main_window_on_it() {
        let directory = scratch("typed");
        let path = directory.join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        type_text(&mut workspace, "  shop.eggdb ", now);
        assert_eq!(workspace.key(KeyPress::plain(Key::Enter), now), WorkspaceCommand::None);
        assert!(card(&mut workspace).busy);
        workspace.wait_for_open(Duration::from_secs(10));
        let Screen::Main(window) = workspace.screen() else {
            panic!("the main window replaces the card");
        };
        assert_eq!(window.database, "shop");
        assert_eq!(window.location, "shop.eggdb");
        assert!(window.writable && window.tab.editor.text().is_empty());
        let connection = workspace.connection_mut().unwrap();
        assert_eq!(connection.status_text(), "Connected · shop.eggdb · read-write");
        assert!(path.exists(), "Create was checked at first start");
        drop(workspace);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_with_create_unchecked_shows_the_error_lines_and_selects_the_input() {
        let directory = scratch("missing");
        let path = directory.join("absent.eggdb");
        let _ = std::fs::remove_file(&path);
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory);
        type_text(&mut workspace, &path.display().to_string(), now);
        workspace.key(KeyPress::plain(Key::Tab), now);
        workspace.key(KeyPress::plain(Key::Tab), now);
        assert_eq!(card(&mut workspace).focus, Focus::CreateIfMissing);
        workspace.key(KeyPress::plain(Key::Space), now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        workspace.wait_for_open(Duration::from_secs(10));
        let shown = card(&mut workspace);
        let error = shown.error.clone().expect("the error lines are shown");
        let path_text = path.display().to_string();
        assert_eq!(error.line, format!("Error: cannot open {path_text}: no such file"));
        assert_eq!(
            error.fix,
            Some(format!(
                "Try: check \"Create the file if it does not exist\", or eggdb create {path_text}"
            ))
        );
        assert_eq!(shown.focus, Focus::FileInput);
        assert_eq!(shown.file.selection(), Some((0, path_text.chars().count())));
        assert!(!shown.busy && !path.exists());
        workspace.key(KeyPress::plain(Key::Enter), now);
        assert_eq!(card(&mut workspace).error, None, "a second attempt clears the lines");
    }

    #[test]
    fn a_slow_open_shows_progress_after_120_ms_and_escape_cancels_it() {
        let now = Instant::now();
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::from_secs(1))),
            scratch("slow"),
        );
        workspace.key(KeyPress::plain(Key::Enter), now);
        assert!(!workspace.tick(now + Duration::from_millis(50)));
        assert_eq!(card(&mut workspace).progress, None);
        assert!(workspace.tick(now + Duration::from_millis(200)));
        let shown = card(&mut workspace);
        assert_eq!(shown.progress.as_deref(), Some("Opening quickstart.eggdb…"));
        assert!(shown.disabled());
        let stopped = now + Duration::from_millis(500);
        workspace.key(KeyPress::plain(Key::Character('x')), stopped);
        assert_eq!(card(&mut workspace).file.value(), "~/eggdb/quickstart.eggdb");
        workspace.key(KeyPress::plain(Key::Escape), stopped);
        let shown = card(&mut workspace);
        assert!(!shown.busy && !shown.disabled() && !workspace.is_opening());
        assert_eq!(shown.status.as_deref(), Some("Cancelled"));
        assert_eq!(shown.focus, Focus::FileInput);
        assert!(workspace.tick(stopped + CANCELLED_SHOWN_FOR));
        assert_eq!(card(&mut workspace).status, None);
    }

    #[test]
    fn browse_asks_for_eggdb_files_and_its_answer_fills_the_input() {
        let directory = scratch("browse");
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        workspace.key(KeyPress::plain(Key::Tab), now);
        let WorkspaceCommand::ChooseFile(dialog) = workspace.key(KeyPress::plain(Key::Enter), now)
        else {
            panic!("Browse… asks for the dialog");
        };
        assert_eq!(dialog.filters[0].patterns, vec!["*.eggdb".to_owned()]);
        assert_eq!(dialog.filters[1].name, "All files");
        assert_eq!(dialog.start_in, Some(PathBuf::from("/home/nobody")));
        workspace.file_chosen(None);
        assert_eq!(card(&mut workspace).focus, Focus::Browse);
        let chosen = directory.join("chosen.eggdb");
        workspace.file_chosen(Some(&chosen));
        let shown = card(&mut workspace);
        assert_eq!(shown.file.value(), chosen.display().to_string());
        assert_eq!(shown.file.selection(), None);
        assert_eq!(shown.focus, Focus::FileInput);
        let mut card = shown;
        card.file_dialog = false;
        assert!(!card.focus_order().contains(&Focus::Browse));
    }

    #[test]
    fn dropping_files_opens_the_first_and_ignores_directories() {
        let directory = scratch("drop");
        let first = directory.join("dropped.eggdb");
        let _ = std::fs::remove_file(&first);
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        workspace.drag_entered(std::slice::from_ref(&directory));
        assert!(!card(&mut workspace).drag_over, "a directory shows no ring");
        workspace.dropped(std::slice::from_ref(&directory), now);
        assert!(!workspace.is_opening());
        workspace.drag_entered(std::slice::from_ref(&first));
        assert!(card(&mut workspace).drag_over);
        workspace.drag_left();
        assert!(!card(&mut workspace).drag_over);
        let others = [first.clone(), directory.join("b.eggdb"), directory.join("c.eggdb")];
        workspace.dropped(&others, now);
        assert_eq!(
            card(&mut workspace).status.as_deref(),
            Some("Opened the first of 3 files")
        );
        workspace.wait_for_open(Duration::from_secs(10));
        let Screen::Main(window) = workspace.screen() else {
            panic!("the drop opens the file");
        };
        assert_eq!(window.location, "dropped.eggdb");
        drop(workspace);
        let _ = std::fs::remove_file(&first);
    }

    #[test]
    fn a_failure_about_the_file_points_at_the_file_input() {
        let mut card = ConnectCard::first_start();
        card.focus = Focus::Open;
        card.begin_opening();
        card.finish_with_failure(&ConnectFailure {
            code: Some("28P01".to_owned()),
            message: "password authentication failed for user \"ada\"".to_owned(),
            fix: None,
            field: FailureField::File,
        });
        let error = card.error.clone().unwrap();
        assert_eq!(
            error.line,
            "Error 28P01: password authentication failed for user \"ada\""
        );
        assert_eq!(error.fix, None);
        assert_eq!(card.focus, Focus::FileInput);
    }

    #[test]
    fn a_slow_connect_shows_connecting_to_the_host_and_port() {
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::from_millis(400))),
            scratch("server"),
        );
        let start = Instant::now();
        if let Some(card) = workspace.card_mut() {
            card.show_segment(Segment::Server);
            card.focus = Focus::Url;
        }
        type_text(&mut workspace, "eggdb://ada@db.example.com/shop", start);
        workspace.key(KeyPress::plain(Key::Enter), start);
        assert!(workspace.is_opening());
        workspace.tick(start + PROGRESS_DELAY);
        let shown = card(&mut workspace);
        assert_eq!(
            shown.progress.as_deref(),
            Some("Connecting to db.example.com:5432…")
        );
        assert!(shown.disabled());
        workspace.wait_for_open(Duration::from_secs(5));
        let failed = card(&mut workspace);
        assert_eq!(failed.focus, Focus::Url);
        assert_eq!(
            failed.error.map(|error| error.line).as_deref(),
            Some("Error: the URL is not a database URL")
        );
    }

    #[test]
    fn typing_a_path_after_the_server_segment_opens_the_file() {
        let directory = scratch("back-to-file");
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        if let Some(card) = workspace.card_mut() {
            card.show_segment(Segment::Server);
        }
        workspace.open_typed("back.eggdb", Instant::now());
        workspace.wait_for_open(Duration::from_secs(10));
        assert!(matches!(workspace.screen(), Screen::Main(_)));
        let _ = std::fs::remove_file(directory.join("back.eggdb"));
    }
}
