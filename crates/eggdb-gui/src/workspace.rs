use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eggdb_platform::window::{KeyPress, OpenFileDialog};

use crate::connect_screen::{ConnectAction, ConnectCard, handle_key};
use crate::connection::{ConnectFailure, ConnectTarget, Connection, Connector, FileTarget};
use crate::main_window::MainWindow;

/// How long an open runs before the card shows its progress state, so a quick open never
/// flickers.
pub const PROGRESS_DELAY: Duration = Duration::from_millis(120);
/// How long `Cancelled` stays on the status line.
pub const CANCELLED_SHOWN_FOR: Duration = Duration::from_secs(2);

const DIALECT: &str = "PostgreSQL";

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

struct Opening {
    target: FileTarget,
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
    status_until: Option<Instant>,
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
            status_until: None,
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

    /// Answers a key press.
    pub fn key(&mut self, press: KeyPress, now: Instant) -> WorkspaceCommand {
        let Screen::Connect(card) = &mut self.screen else {
            return WorkspaceCommand::None;
        };
        let (next, action) = handle_key(card.clone(), press);
        *card = next;
        self.act(action, now)
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
        let target = card.file_target(self.home.as_deref(), &self.current);
        card.begin_opening();
        let (sender, answer) = mpsc::channel();
        let connector = Arc::clone(&self.connector);
        let request = ConnectTarget::File(target.clone());
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
        let mut changed = false;
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
/// location in the toolbar, its tables in the tree, one empty `Query 1` editor.
pub fn main_window_for(connection: &mut Connection) -> MainWindow {
    MainWindow {
        database: connection.name().to_owned(),
        location: connection.location().to_owned(),
        writable: !connection.read_only(),
        tables: connection.table_names().unwrap_or_default(),
        selected_table: None,
        sql: String::new(),
        columns: Vec::new(),
        rows: Vec::new(),
        elapsed: String::new(),
        dialect: DIALECT.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect_screen::Focus;
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
        assert!(window.writable && window.sql.is_empty());
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
}
