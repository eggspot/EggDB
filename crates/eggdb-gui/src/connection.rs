use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;

use eggdb::sql::{
    CancelSignal, Dialect, QueryResult, Session, SqlError, Store, TransactionStatus, Value,
    error_lines,
};
use eggdb::{Database, Error, ErrorKind, Options};
use eggdb_pgwire::client::{
    CancelRequest, ClientError, ConnectOptions, PgClient, QueryRows, SslMode,
};

use crate::dialect::QueryDialect;
use crate::grid::ValueKind;
use crate::resp_client::{RespClient, RespConnectError, RespOptions};

/// The most rows one result keeps for the grid; the rest are counted, not held
/// (`docs/ux/workspace.md` 3.7).
pub const ROW_LIMIT: usize = 100_000;

const USER_TABLES: &str = "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname NOT IN ('pg_catalog', 'information_schema') ORDER BY tablename";
const AUTHENTICATION_CLASS: &str = "28";
const QUERY_CANCELED: &str = "57014";
const NO_ACTIVE_TRANSACTION: &str = "25P01";
const ROLLED_BACK_SAVEPOINT: &str = "eggdb_explain_analyze";
const DATABASE_MISSING: &str = "3D000";

/// What the connect card asks to open: a database file or a server. Its `Debug` shows a
/// password as `***`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectTarget {
    /// A database file on this computer.
    File(FileTarget),
    /// A server speaking the PostgreSQL protocol, such as `eggdb serve`.
    Server(ConnectOptions),
    /// A server speaking RESP, reached with a `redis://` URL: a key-value connection.
    Resp(RespOptions),
}

/// What `Debug` output shows in place of a password.
pub const REDACTED: &str = "***";

/// A database file to open, with the card's two checkboxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    /// The absolute path, resolved from what was typed.
    pub path: PathBuf,
    /// `Create the file if it does not exist`.
    pub create_if_missing: bool,
    /// `Open read-only`.
    pub read_only: bool,
}

/// The connect card's field a failure is about: it takes the focus with its value selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureField {
    /// The File input.
    File,
    /// The server segment's `URL` input.
    Url,
    /// The server segment's protocol menu.
    Protocol,
    /// The Host input.
    Host,
    /// The Database input.
    Database,
    /// The User input.
    User,
    /// The Password input.
    Password,
}

/// Why a database could not be opened, in the words `eggdb` prints (`docs/ux/workspace.md`
/// 1.6): the `Error` line, the `Try:` line when there is a fix, and the field it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFailure {
    /// The engine's SQLSTATE, when it has one (`28P01`).
    pub code: Option<String>,
    /// The message, without the `Error:` prefix.
    pub message: String,
    /// What to try, without the `Try:` prefix.
    pub fix: Option<String>,
    /// The field the failure is about.
    pub field: FailureField,
}

impl ConnectFailure {
    fn new(message: String, fix: Option<String>, field: FailureField) -> Self {
        Self {
            code: None,
            message,
            fix,
            field,
        }
    }

    /// The first line as the card shows it: `Error 28P01: …` with a code, else `Error: …`.
    pub fn error_line(&self) -> String {
        match &self.code {
            Some(code) => format!("Error {code}: {}", self.message),
            None => format!("Error: {}", self.message),
        }
    }

    /// The second line as the card shows it, `Try: …`, when there is a fix.
    pub fn try_line(&self) -> Option<String> {
        self.fix.as_ref().map(|fix| format!("Try: {fix}"))
    }
}

/// One statement's result as text, the way a grid shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultTable {
    /// The column names; empty for a statement that returns no rows.
    pub columns: Vec<String>,
    /// Each column's kind of value, which sets its cells' alignment and drawing.
    pub kinds: Vec<ValueKind>,
    /// Each row's values as text, `None` for NULL, at most [`ROW_LIMIT`] of them.
    pub rows: Vec<Vec<Option<String>>>,
    /// How many rows the statement returned: more than `rows.len()` when the result was cut
    /// at [`ROW_LIMIT`].
    pub row_count: usize,
    /// The command tag, such as `SELECT 3`.
    pub tag: String,
}

/// Why a statement failed: the engine's SQLSTATE, message and hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementFailure {
    /// The SQLSTATE, such as `42P01`; empty when the failure came from the connection itself.
    pub code: String,
    /// The message.
    pub message: String,
    /// The engine's hint, when it has one.
    pub hint: Option<String>,
    /// The byte offset in the statement's text the error is about, when the engine gives one.
    pub position: Option<usize>,
    /// The lines `eggdb sql` prints for it: `ERROR:`, `LINE n:` with its `^`, `SQLSTATE:`.
    pub report: Vec<String>,
}

impl StatementFailure {
    pub(crate) fn from_sql(error: &SqlError, text: &str) -> Self {
        Self {
            code: error.state().code().to_owned(),
            message: error.message().to_owned(),
            hint: error.hint().map(str::to_owned),
            position: error.position().filter(|&position| position <= text.len()),
            report: error_lines(error, text),
        }
    }

    pub(crate) fn cancelled() -> Self {
        let message = "canceling statement due to user request".to_owned();
        Self {
            code: QUERY_CANCELED.to_owned(),
            report: vec![
                format!("ERROR:  {message}"),
                format!("SQLSTATE:  {QUERY_CANCELED}"),
            ],
            message,
            hint: None,
            position: None,
        }
    }

    pub(crate) fn needs_sql() -> Self {
        Self {
            code: String::new(),
            report: vec![QUERY_TABS_NEED_SQL.to_owned()],
            message: QUERY_TABS_NEED_SQL.to_owned(),
            hint: None,
            position: None,
        }
    }

    pub(crate) fn session_lost() -> Self {
        let message = "the tab's session ended; run again to start a new one".to_owned();
        Self {
            code: String::new(),
            report: vec![format!("ERROR:  {message}")],
            message,
            hint: None,
            position: None,
        }
    }

    pub(crate) fn from_engine(error: &eggdb::Error) -> Self {
        let message = error.to_string();
        Self {
            code: String::new(),
            report: vec![format!("ERROR:  {message}")],
            message,
            hint: None,
            position: None,
        }
    }

    pub(crate) fn dialect_in_transaction(open: QueryDialect, wanted: QueryDialect) -> Self {
        let message = transaction_dialect_message(open, wanted);
        Self {
            code: ACTIVE_TRANSACTION.to_owned(),
            report: vec![
                format!("ERROR:  {message}"),
                format!("SQLSTATE:  {ACTIVE_TRANSACTION}"),
            ],
            message,
            hint: None,
            position: None,
        }
    }

    pub(crate) fn from_client(error: &ClientError) -> Self {
        let (code, message) = match error {
            ClientError::Server { code, message } => (code.clone(), message.clone()),
            other => (String::new(), other.to_string()),
        };
        let mut report = vec![format!("ERROR:  {message}")];
        if !code.is_empty() {
            report.push(format!("SQLSTATE:  {code}"));
        }
        Self {
            code,
            message,
            hint: None,
            position: None,
            report,
        }
    }
}

/// What a run in `wanted` answers while the tab's transaction, begun in `open`, is still open.
pub fn transaction_dialect_message(open: QueryDialect, wanted: QueryDialect) -> String {
    format!(
        "this tab's transaction was begun in {}; COMMIT or ROLLBACK it before running {}",
        open.label(),
        wanted.label()
    )
}

enum Stopper {
    File(CancelSignal),
    Server(CancelRequest),
    Socket(std::net::TcpStream),
}

impl Stopper {
    fn stop(self) {
        match self {
            Self::File(signal) => signal.cancel(),
            Self::Socket(socket) => {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
            Self::Server(request) => {
                std::thread::spawn(move || {
                    let _ = request.send();
                });
            }
        }
    }
}

#[derive(Default)]
struct CancelState {
    requested: bool,
    stopper: Option<Stopper>,
}

/// Stops a running [`Connection::run_script`] from another thread: a file's session gets the
/// engine's cancel signal, a server gets PostgreSQL's cancel request. A cancel that comes
/// before the run starts stops it as soon as it starts.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<Mutex<CancelState>>);

impl std::fmt::Debug for CancelHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancelHandle")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelHandle {
    /// Asks the running statement to stop.
    pub fn cancel(&self) {
        let stopper = {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            state.requested = true;
            state.stopper.take()
        };
        if let Some(stopper) = stopper {
            stopper.stop();
        }
    }

    /// Whether [`CancelHandle::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requested
    }

    fn arm(&self, stopper: Stopper) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.requested {
            drop(state);
            stopper.stop();
        } else {
            state.stopper = Some(stopper);
        }
    }

    fn disarm(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopper = None;
    }
}

/// The query tab a kept session belongs to: each tab runs on its own session, kept from its
/// first run until [`Connection::close_session`], so a `BEGIN` in one run holds for the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct SessionId(pub u64);

/// Each statement's answer, in order, the last one the failure when one failed.
pub type Outcomes = Vec<Result<Vec<ResultTable>, StatementFailure>>;

/// What a SQL run, or `Ctrl/Cmd+T`, answers on a RESP connection (`docs/ux/workspace.md`
/// 10.10).
pub const QUERY_TABS_NEED_SQL: &str = "Query tabs need a SQL connection";

/// SQLSTATE `25001` active_sql_transaction.
pub const ACTIVE_TRANSACTION: &str = "25001";

enum Link {
    File(FileLink),
    Server(ServerLink),
    Resp(RespLink),
}

struct RespLink {
    options: RespOptions,
    client: Option<RespClient>,
}

impl RespLink {
    fn client(&mut self) -> Result<&mut RespClient, StatementFailure> {
        if self.client.is_none() {
            let client = RespClient::connect(&self.options).map_err(|error| {
                let failure = resp_failure(&self.options, &error);
                let mut report = vec![failure.error_line()];
                report.extend(failure.try_line());
                StatementFailure {
                    code: String::new(),
                    message: failure.message,
                    hint: None,
                    position: None,
                    report,
                }
            })?;
            self.client = Some(client);
        }
        self.client
            .as_mut()
            .ok_or_else(StatementFailure::session_lost)
    }

    fn scan(
        &mut self,
        scan: &crate::key_scan::KeySpaceScan,
        cancel: &CancelHandle,
    ) -> Result<crate::key_scan::KeyPage, StatementFailure> {
        let place = self.options.place();
        let client = self.client()?;
        if let Ok(socket) = client.socket() {
            cancel.arm(Stopper::Socket(socket));
        }
        let page = crate::resp_keys::scan_page(client, scan, &place, cancel);
        cancel.disarm();
        if page.is_err() {
            self.drop_client();
        }
        page
    }

    fn catalog(&mut self) -> Result<crate::catalog::Catalog, StatementFailure> {
        let place = self.options.place();
        let catalog = crate::resp_keys::read_catalog(self.client()?, &place);
        if catalog.is_err() {
            self.drop_client();
        }
        catalog
    }

    fn drop_client(&mut self) {
        if let Some(client) = self.client.take() {
            client.close();
        }
    }
}

struct FileLink {
    kept: Option<KeptSessions>,
    database: Arc<Database>,
}

struct ServerLink {
    client: Box<PgClient>,
    options: Box<ConnectOptions>,
    tab_clients: Vec<(SessionId, PgClient)>,
}

impl Drop for ServerLink {
    fn drop(&mut self) {
        for (_, client) in self.tab_clients.drain(..) {
            client.terminate();
        }
    }
}

enum SessionWork {
    Run(SessionRun),
    Close(SessionId, mpsc::Sender<()>),
    Status(SessionId, mpsc::Sender<TransactionStatus>),
}

struct SessionRun {
    session: SessionId,
    dialect: QueryDialect,
    statements: Vec<String>,
    cancel: CancelHandle,
    answer: mpsc::Sender<Outcomes>,
}

struct KeptSessions {
    work: Option<mpsc::Sender<SessionWork>>,
    thread: Option<JoinHandle<()>>,
}

impl KeptSessions {
    fn start(database: &Arc<Database>) -> io::Result<Self> {
        let (work, jobs) = mpsc::channel();
        let database = Arc::clone(database);
        let thread = std::thread::Builder::new()
            .name("eggdb-gui-sessions".to_owned())
            .spawn(move || keep_sessions(&database, &jobs))?;
        Ok(Self {
            work: Some(work),
            thread: Some(thread),
        })
    }

    fn run(
        &self,
        session: SessionId,
        dialect: QueryDialect,
        statements: &[String],
        cancel: &CancelHandle,
    ) -> Outcomes {
        let (answer, answered) = mpsc::channel();
        let run = SessionRun {
            session,
            dialect,
            statements: statements.to_vec(),
            cancel: cancel.clone(),
            answer,
        };
        let sent = self
            .work
            .as_ref()
            .is_some_and(|work| work.send(SessionWork::Run(run)).is_ok());
        if !sent {
            return vec![Err(StatementFailure::session_lost())];
        }
        answered
            .recv()
            .unwrap_or_else(|_| vec![Err(StatementFailure::session_lost())])
    }

    fn close(&self, session: SessionId) {
        let (closed, answered) = mpsc::channel();
        let sent = self
            .work
            .as_ref()
            .is_some_and(|work| work.send(SessionWork::Close(session, closed)).is_ok());
        if sent {
            let _ = answered.recv();
        }
    }
}

impl KeptSessions {
    fn status(&self, session: SessionId) -> TransactionStatus {
        let (answer, answered) = mpsc::channel();
        let sent = self
            .work
            .as_ref()
            .is_some_and(|work| work.send(SessionWork::Status(session, answer)).is_ok());
        if !sent {
            return TransactionStatus::Idle;
        }
        answered.recv().unwrap_or(TransactionStatus::Idle)
    }
}

impl Drop for KeptSessions {
    fn drop(&mut self) {
        drop(self.work.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn keep_sessions(database: &Database, jobs: &mpsc::Receiver<SessionWork>) {
    let store = database.sql_store();
    let mut sessions: Vec<(SessionId, Session<'_>)> = Vec::new();
    while let Ok(job) = jobs.recv() {
        match job {
            SessionWork::Run(run) => {
                let outcomes = run_kept(&store, &mut sessions, &run);
                let _ = run.answer.send(outcomes);
            }
            SessionWork::Close(id, closed) => {
                if let Some(at) = sessions.iter().position(|(kept, _)| *kept == id) {
                    let (_, session) = sessions.swap_remove(at);
                    let _ = session.close();
                }
                let _ = closed.send(());
            }
            SessionWork::Status(id, answer) => {
                let status = sessions
                    .iter()
                    .find(|(kept, _)| *kept == id)
                    .map_or(TransactionStatus::Idle, |(_, session)| {
                        session.transaction_status()
                    });
                let _ = answer.send(status);
            }
        }
    }
    for (_, session) in sessions {
        let _ = session.close();
    }
}

fn run_kept<'s>(
    store: &'s Store<'_>,
    sessions: &mut Vec<(SessionId, Session<'s>)>,
    run: &SessionRun,
) -> Outcomes {
    let wanted = run.dialect.engine();
    let at = match sessions.iter().position(|(id, _)| *id == run.session) {
        Some(at) => at,
        None => {
            sessions.push((run.session, store.session(wanted)));
            sessions.len() - 1
        }
    };
    let Some((_, session)) = sessions.get_mut(at) else {
        return Vec::new();
    };
    if session.dialect() != wanted {
        if session.transaction_status() != TransactionStatus::Idle {
            return vec![Err(StatementFailure::dialect_in_transaction(
                QueryDialect::of_engine(session.dialect()),
                run.dialect,
            ))];
        }
        let old = std::mem::replace(session, store.session(wanted));
        let _ = old.close();
    }
    run_on_session(session, &run.statements, &run.cancel)
}

fn run_on_session(
    session: &mut Session<'_>,
    statements: &[String],
    cancel: &CancelHandle,
) -> Outcomes {
    let mut outcomes = Vec::with_capacity(statements.len());
    let signal = session.cancel_signal();
    signal.clear();
    cancel.arm(Stopper::File(signal));
    for statement in statements {
        if cancel.is_cancelled() {
            outcomes.push(Err(StatementFailure::cancelled()));
            break;
        }
        let mut tables = Vec::new();
        let ran = session.execute(statement, &mut |result| tables.push(file_result(result)));
        let failed = ran.is_err();
        outcomes.push(
            ran.map(|()| tables)
                .map_err(|error| StatementFailure::from_sql(&error, statement)),
        );
        if failed {
            break;
        }
    }
    cancel.disarm();
    session.cancel_signal().clear();
    outcomes
}

fn run_on_client(client: &mut PgClient, statements: &[String], cancel: &CancelHandle) -> Outcomes {
    let mut outcomes = Vec::with_capacity(statements.len());
    if let Some(request) = client.cancel_request() {
        cancel.arm(Stopper::Server(request));
    }
    for statement in statements {
        if cancel.is_cancelled() {
            outcomes.push(Err(StatementFailure::cancelled()));
            break;
        }
        let ran = client
            .simple_query(statement)
            .map(|results| results.into_iter().map(server_result).collect())
            .map_err(|error| StatementFailure::from_client(&error));
        let failed = ran.is_err();
        outcomes.push(ran);
        if failed {
            break;
        }
    }
    cancel.disarm();
    outcomes
}

/// The GUI's link to one database: an open `.eggdb` file or a session on a server. The main
/// window, the query tab, the object browser and the plan view reach the database only
/// through it.
pub struct Connection {
    name: String,
    location: String,
    read_only: bool,
    tls: bool,
    link: Link,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("name", &self.name)
            .field("location", &self.location)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// An empty database in memory named `name`, as a file dropped in the browser opens.
    pub fn in_memory(name: &str) -> Result<Self, ConnectFailure> {
        let database = Database::in_memory_with(Options::new()).map_err(|error| {
            ConnectFailure::new(
                format!("cannot open {name}: {error}"),
                None,
                FailureField::File,
            )
        })?;
        Ok(Self {
            name: file_stem(Path::new(name)),
            location: name.to_owned(),
            read_only: false,
            tls: false,
            link: Link::File(FileLink {
                kept: None,
                database: Arc::new(database),
            }),
        })
    }

    /// The connection's name in the toolbar: the file's stem or the server's database.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where it is: the file's name, or `host:port`.
    pub fn location(&self) -> &str {
        &self.location
    }

    /// Whether the database was opened read-only.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Whether this is a server session.
    pub fn is_server(&self) -> bool {
        matches!(self.link, Link::Server(_) | Link::Resp(_))
    }

    /// Whether this is a RESP server: a key-value connection with no SQL.
    pub fn is_resp(&self) -> bool {
        matches!(self.link, Link::Resp(_))
    }

    /// The status bar's connection text: `Connected · shop.eggdb · read-write`, or
    /// `Connected · db.example.com:5432 · TLS` for a server.
    pub fn status_text(&self) -> String {
        let detail = match (&self.link, self.read_only, self.tls) {
            (Link::Server(_), _, true) => "TLS",
            (Link::Server(_), _, false) => "no TLS",
            (Link::Resp(_), _, _) => "RESP",
            (Link::File(_), true, _) => "read-only",
            (Link::File(_), false, _) => "read-write",
        };
        format!("Connected · {} · {detail}", self.location)
    }

    /// The toast shown for 8 s once it opened: `Opened shop.eggdb` or `Connected to <host>`.
    pub fn toast(&self) -> String {
        match &self.link {
            Link::File(_) => format!("Opened {}", self.location),
            Link::Server(_) | Link::Resp(_) => {
                let host = self
                    .location
                    .rsplit_once(':')
                    .map_or(self.location.as_str(), |(host, _)| host);
                format!("Connected to {host}")
            }
        }
    }

    /// The user's tables, in name order.
    pub fn table_names(&mut self) -> Result<Vec<String>, StatementFailure> {
        match &mut self.link {
            Link::File(file) => {
                let store = file.database.sql_store();
                let mut session = store.session(Dialect::PostgreSql);
                let tables = session
                    .tables()
                    .map_err(|error| StatementFailure::from_sql(&error, ""));
                let _ = session.close();
                let mut names: Vec<String> = tables?.into_iter().map(|table| table.name).collect();
                names.sort();
                Ok(names)
            }
            Link::Server(server) => {
                let results = server
                    .client
                    .simple_query(USER_TABLES)
                    .map_err(|error| StatementFailure::from_client(&error))?;
                Ok(results
                    .into_iter()
                    .flat_map(|result| result.rows)
                    .filter_map(|row| row.into_iter().next().flatten())
                    .collect())
            }
            Link::Resp(_) => Ok(Vec::new()),
        }
    }

    /// The database's catalog as the object browser lists it: tables with their columns and
    /// DDL, views, indexes, sequences and, for a file, its key spaces. A file is read through
    /// the engine's schema, a server through its `pg_catalog` and `information_schema`.
    pub fn catalog(&mut self) -> Result<crate::catalog::Catalog, StatementFailure> {
        match &mut self.link {
            Link::File(file) => crate::catalog::read_file(&file.database),
            Link::Server(server) => crate::catalog::read_server(&mut server.client),
            Link::Resp(resp) => resp.catalog(),
        }
    }

    /// A keys tab's page of `scan`'s key space (`docs/ux/workspace.md` 10.4, 10.10): up to
    /// [`crate::key_scan::KEY_SCAN_LIMIT`] keys starting with its prefix after its start. A
    /// file reads them in key order, in one read transaction and no session; a RESP server
    /// with `SCAN` in its own order, reconnecting first when an earlier exchange dropped, and
    /// `cancel` closes its socket. A SQL server reads no key spaces, so its answer is an empty
    /// page.
    pub fn scan_keys(
        &mut self,
        scan: &crate::key_scan::KeySpaceScan,
        cancel: &CancelHandle,
    ) -> Result<crate::key_scan::KeyPage, StatementFailure> {
        match &mut self.link {
            Link::File(file) => scan.scan_file(&file.database),
            Link::Server(_) => Ok(scan.page(&[])),
            Link::Resp(resp) => resp.scan(scan, cancel),
        }
    }

    /// Runs `sql` (one or more statements in PostgreSQL's dialect) and returns each
    /// statement's result as text.
    pub fn run(&mut self, sql: &str) -> Result<Vec<ResultTable>, StatementFailure> {
        let outcomes = self.run_script(
            QueryDialect::PostgreSql,
            &[sql.to_owned()],
            &CancelHandle::default(),
        );
        outcomes.into_iter().next().unwrap_or(Ok(Vec::new()))
    }

    /// Runs `statements` one after another in `dialect` on one session, so a `BEGIN` in one
    /// holds for the next, and stops at the first that fails: the answer has one entry per
    /// statement that ran, the last one the failure when there was one. `cancel` can stop the
    /// running statement from another thread. A file runs them on a session of their own,
    /// closed after; a server on the client the connect opened, which no tab's kept session
    /// uses, so the catalog, the tree's drops and the inspector's samples never run inside a
    /// tab's transaction.
    pub fn run_script(
        &mut self,
        dialect: QueryDialect,
        statements: &[String],
        cancel: &CancelHandle,
    ) -> Outcomes {
        match &mut self.link {
            Link::File(file) => {
                let store = file.database.sql_store();
                let mut session = store.session(dialect.engine());
                let outcomes = run_on_session(&mut session, statements, cancel);
                let _ = session.close();
                outcomes
            }
            Link::Server(server) => run_on_client(&mut server.client, statements, cancel),
            Link::Resp(_) => vec![Err(StatementFailure::needs_sql())],
        }
    }

    /// Runs `statements` as [`Connection::run_script`] does, on the session `session` keeps
    /// from its first run until [`Connection::close_session`]: a transaction begun in one run
    /// stays open for the next, and other sessions do not see its work until it commits. On a
    /// server each kept session is a client of its own, opened with the connect's options. A
    /// session idle outside a transaction follows `dialect`; one inside a transaction keeps
    /// its dialect and answers a run in another with SQLSTATE `25001`.
    pub fn run_in_session(
        &mut self,
        session: SessionId,
        dialect: QueryDialect,
        statements: &[String],
        cancel: &CancelHandle,
    ) -> Outcomes {
        match &mut self.link {
            Link::File(file) => {
                if file.kept.is_none() {
                    match KeptSessions::start(&file.database) {
                        Ok(kept) => file.kept = Some(kept),
                        Err(_) => return vec![Err(StatementFailure::session_lost())],
                    }
                }
                match &file.kept {
                    Some(kept) => kept.run(session, dialect, statements, cancel),
                    None => vec![Err(StatementFailure::session_lost())],
                }
            }
            Link::Server(server) => match server.client_for(session) {
                Ok(client) => run_on_client(client, statements, cancel),
                Err(failure) => vec![Err(failure)],
            },
            Link::Resp(_) => vec![Err(StatementFailure::needs_sql())],
        }
    }

    /// Ends `session`: an open transaction in it rolls back, and its next run starts a new
    /// session.
    pub fn close_session(&mut self, session: SessionId) {
        match &mut self.link {
            Link::File(file) => {
                if let Some(kept) = &file.kept {
                    kept.close(session);
                }
            }
            Link::Server(server) => server.close(session),
            Link::Resp(_) => {}
        }
    }

    /// Where `session` stands toward transactions: idle, inside a block, or inside a failed
    /// block that only `ROLLBACK` ends. A session that never ran is idle.
    pub fn transaction_status(&self, session: SessionId) -> TransactionStatus {
        match &self.link {
            Link::File(file) => file
                .kept
                .as_ref()
                .map_or(TransactionStatus::Idle, |kept| kept.status(session)),
            Link::Server(server) => server.status(session),
            Link::Resp(_) => TransactionStatus::Idle,
        }
    }

    /// Runs `statement` in `dialect` so that nothing it writes stays, whatever it is: on a
    /// session that already has a transaction open, between `SAVEPOINT` and
    /// `ROLLBACK TO SAVEPOINT` (SQL Server's `SAVE TRANSACTION` and `ROLLBACK TRANSACTION`),
    /// so the session's own work stays; an idle Oracle session, whose `SAVEPOINT` opens the
    /// transaction, between `SAVEPOINT` and `ROLLBACK`; else inside `BEGIN`
    /// (`BEGIN TRANSACTION`) and `ROLLBACK`. The outcomes start with the opening statement's, then the statement's,
    /// then the closing statements'; a session whose transaction already failed gets that
    /// failure and runs nothing. With `session` it runs on that kept session, else as
    /// [`Connection::run_script`] does.
    pub fn run_rolled_back(
        &mut self,
        dialect: QueryDialect,
        statement: &str,
        cancel: &CancelHandle,
    ) -> Outcomes {
        self.run_rolled_back_on(None, dialect, &[statement.to_owned()], cancel)
    }

    /// [`Connection::run_rolled_back`] on the kept session `session`.
    pub fn run_rolled_back_in_session(
        &mut self,
        session: SessionId,
        dialect: QueryDialect,
        statement: &str,
        cancel: &CancelHandle,
    ) -> Outcomes {
        self.run_rolled_back_on(Some(session), dialect, &[statement.to_owned()], cancel)
    }

    /// Runs `run` on the kept session `session`: its `before` statements, then, when they
    /// all succeeded, its statements (rolled back as [`Connection::run_rolled_back`] does
    /// when `run.rolled_back`), then its `after` statements whatever happened, which `cancel`
    /// does not stop. The outcomes are those of every statement that ran, in order.
    pub fn run_bracketed_in_session(
        &mut self,
        session: SessionId,
        dialect: QueryDialect,
        run: &BracketedRun,
        cancel: &CancelHandle,
    ) -> Outcomes {
        let mut outcomes = if run.before.is_empty() {
            Vec::new()
        } else {
            self.run_in_session(session, dialect, &run.before, cancel)
        };
        let ready = outcomes.len() == run.before.len() && outcomes.iter().all(Result::is_ok);
        if ready {
            let ran = if run.rolled_back {
                self.run_rolled_back_on(Some(session), dialect, &run.statements, cancel)
            } else {
                self.run_in_session(session, dialect, &run.statements, cancel)
            };
            outcomes.extend(ran);
        }
        if !run.after.is_empty() {
            outcomes.extend(self.run_in_session(
                session,
                dialect,
                &run.after,
                &CancelHandle::default(),
            ));
        }
        outcomes
    }

    fn run_on(
        &mut self,
        session: Option<SessionId>,
        dialect: QueryDialect,
        statements: &[String],
        cancel: &CancelHandle,
    ) -> Outcomes {
        match session {
            Some(session) => self.run_in_session(session, dialect, statements, cancel),
            None => self.run_script(dialect, statements, cancel),
        }
    }

    fn run_rolled_back_on(
        &mut self,
        session: Option<SessionId>,
        dialect: QueryDialect,
        statements: &[String],
        cancel: &CancelHandle,
    ) -> Outcomes {
        let undo = CancelHandle::default();
        let words = UndoWords::of(dialect);
        let keeps_transactions = session.is_some() || matches!(self.link, Link::Server(_));
        let idle = session
            .is_some_and(|session| self.transaction_status(session) == TransactionStatus::Idle);
        let savepoint_opens_transaction = dialect == QueryDialect::Oracle;
        if keeps_transactions && (!idle || savepoint_opens_transaction) {
            let mut opened = self.run_on(session, dialect, &[words.savepoint()], &undo);
            match opened.pop() {
                Some(Ok(tables)) => {
                    let closing = if idle {
                        vec![words.rollback.to_owned()]
                    } else {
                        words.back_to_savepoint()
                    };
                    let mut outcomes = vec![Ok(tables)];
                    outcomes.extend(self.run_on(session, dialect, statements, cancel));
                    outcomes.extend(self.run_on(session, dialect, &closing, &undo));
                    return outcomes;
                }
                Some(Err(failure)) if failure.code != NO_ACTIVE_TRANSACTION => {
                    return vec![Err(failure)];
                }
                _ => {}
            }
        }
        let mut script = Vec::with_capacity(statements.len() + 2);
        script.push(words.begin.to_owned());
        script.extend(statements.iter().cloned());
        script.push(words.rollback.to_owned());
        let mut outcomes = self.run_on(session, dialect, &script, cancel);
        let began = outcomes.first().is_some_and(Result::is_ok);
        if began && outcomes.len() < script.len() && keeps_transactions {
            outcomes.extend(self.run_on(session, dialect, &[words.rollback.to_owned()], &undo));
        }
        outcomes
    }
}

/// Statements run with set-up before and clean-up after them, as `Explain` runs in SQL
/// Server's dialect: `SET SHOWPLAN_ALL ON`, the statement, then `SET SHOWPLAN_ALL OFF` even
/// when the statement failed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BracketedRun {
    /// Run first; the statements run only when all of these succeed.
    pub before: Vec<String>,
    /// The statements.
    pub statements: Vec<String>,
    /// Run last, whatever happened before.
    pub after: Vec<String>,
    /// Whether the statements run so that nothing they write stays.
    pub rolled_back: bool,
}

struct UndoWords {
    begin: &'static str,
    rollback: &'static str,
    sqlserver: bool,
}

impl UndoWords {
    fn of(dialect: QueryDialect) -> Self {
        if dialect == QueryDialect::SqlServer {
            Self {
                begin: "BEGIN TRANSACTION",
                rollback: "ROLLBACK",
                sqlserver: true,
            }
        } else {
            Self {
                begin: "BEGIN",
                rollback: "ROLLBACK",
                sqlserver: false,
            }
        }
    }

    fn savepoint(&self) -> String {
        if self.sqlserver {
            format!("SAVE TRANSACTION {ROLLED_BACK_SAVEPOINT}")
        } else {
            format!("SAVEPOINT {ROLLED_BACK_SAVEPOINT}")
        }
    }

    fn back_to_savepoint(&self) -> Vec<String> {
        if self.sqlserver {
            vec![format!("ROLLBACK TRANSACTION {ROLLED_BACK_SAVEPOINT}")]
        } else {
            vec![
                format!("ROLLBACK TO SAVEPOINT {ROLLED_BACK_SAVEPOINT}"),
                format!("RELEASE SAVEPOINT {ROLLED_BACK_SAVEPOINT}"),
            ]
        }
    }
}

impl ServerLink {
    fn new(client: PgClient, options: &ConnectOptions) -> Self {
        let first_used_tls = client.tls_version().is_some();
        Self {
            client: Box::new(client),
            options: Box::new(tab_client_options(options, first_used_tls)),
            tab_clients: Vec::new(),
        }
    }

    fn client_for(&mut self, session: SessionId) -> Result<&mut PgClient, StatementFailure> {
        let at = match self.tab_clients.iter().position(|(id, _)| *id == session) {
            Some(at) => at,
            None => {
                let client = PgClient::connect(&self.options)
                    .map_err(|error| StatementFailure::from_client(&error))?;
                self.tab_clients.push((session, client));
                self.tab_clients.len() - 1
            }
        };
        match self.tab_clients.get_mut(at) {
            Some((_, client)) => Ok(client),
            None => Err(StatementFailure::session_lost()),
        }
    }

    fn status(&self, session: SessionId) -> TransactionStatus {
        let client = self
            .tab_clients
            .iter()
            .find(|(id, _)| *id == session)
            .map(|(_, client)| client);
        match client.map(PgClient::ready_status) {
            Some(b'T') => TransactionStatus::InBlock,
            Some(b'E') => TransactionStatus::Failed,
            _ => TransactionStatus::Idle,
        }
    }

    fn close(&mut self, session: SessionId) {
        if let Some(at) = self.tab_clients.iter().position(|(id, _)| *id == session) {
            let (_, client) = self.tab_clients.swap_remove(at);
            client.terminate();
        }
    }
}

pub(crate) fn file_result(result: QueryResult) -> ResultTable {
    let columns: Vec<String> = result
        .columns
        .unwrap_or_default()
        .into_iter()
        .map(|column| column.name)
        .collect();
    let kinds = (0..columns.len())
        .map(|column| {
            result
                .rows
                .iter()
                .find_map(|row| row.get(column).and_then(value_kind))
                .unwrap_or(ValueKind::Text)
        })
        .collect();
    let row_count = result.rows.len();
    ResultTable {
        columns,
        kinds,
        rows: result
            .rows
            .iter()
            .take(ROW_LIMIT)
            .map(|row| row.iter().map(|value| value.to_text()).collect())
            .collect(),
        row_count,
        tag: result.tag,
    }
}

fn value_kind(value: &Value) -> Option<ValueKind> {
    Some(match value {
        Value::Null => return None,
        Value::SmallInt(_)
        | Value::Integer(_)
        | Value::BigInt(_)
        | Value::Real(_)
        | Value::Double(_)
        | Value::Numeric(_)
        | Value::Oid(_) => ValueKind::Number,
        Value::Boolean(_) => ValueKind::Boolean,
        Value::Json(_) | Value::Jsonb(_) | Value::Array(_) => ValueKind::Json,
        Value::Bytea(_) => ValueKind::Blob,
        _ => ValueKind::Text,
    })
}

/// The kind of value a PostgreSQL type OID holds, for a server's result columns.
pub fn type_oid_kind(oid: u32) -> ValueKind {
    match oid {
        20 | 21 | 23 | 26 | 700 | 701 | 790 | 1700 => ValueKind::Number,
        16 => ValueKind::Boolean,
        17 => ValueKind::Blob,
        114 | 3802 | 199 | 1000..=1041 | 1115 | 1182 | 1183 | 1185 | 1231 | 3807 => ValueKind::Json,
        _ => ValueKind::Text,
    }
}

fn server_result(result: QueryRows) -> ResultTable {
    let row_count = result.rows.len();
    let kinds = result
        .column_types
        .iter()
        .map(|&oid| type_oid_kind(oid))
        .chain(std::iter::repeat(ValueKind::Text))
        .take(result.columns.len())
        .collect();
    let mut rows = result.rows;
    rows.truncate(ROW_LIMIT);
    ResultTable {
        columns: result.columns,
        kinds,
        rows,
        row_count,
        tag: result.tag,
    }
}

/// Opens what the connect card asks for. The client's own [`EngineConnector`] opens files with
/// the engine and servers with its PostgreSQL client; tests give the client a stand-in.
pub trait Connector: Send + Sync {
    /// Opens `target`, or says why it could not, in the card's words.
    fn connect(&self, target: &ConnectTarget) -> Result<Connection, ConnectFailure>;
}

/// The engine itself: files through [`Database::open_with`], servers through the PostgreSQL
/// protocol client.
#[derive(Debug, Clone, Copy, Default)]
pub struct EngineConnector;

impl Connector for EngineConnector {
    fn connect(&self, target: &ConnectTarget) -> Result<Connection, ConnectFailure> {
        match target {
            ConnectTarget::File(file) => open_file(file),
            ConnectTarget::Server(options) => connect_server(options),
            ConnectTarget::Resp(options) => connect_resp(options),
        }
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn open_file(target: &FileTarget) -> Result<Connection, ConnectFailure> {
    let path = &target.path;
    let shown = path.display().to_string();
    if path.is_dir() {
        return Err(not_an_eggdb_file(&shown));
    }
    let creates = target.create_if_missing && !target.read_only;
    if creates && !path.exists() && !path.parent().is_some_and(Path::is_dir) {
        return Err(ConnectFailure::new(
            format!("cannot create {shown}: the directory does not exist"),
            Some("create the directory first, or choose another path with Browse…".to_owned()),
            FailureField::File,
        ));
    }
    let options = Options::new()
        .create_if_missing(creates)
        .read_only(target.read_only);
    let database = Database::open_with(path, options)
        .map_err(|error| file_failure(&shown, &error, creates))?;
    Ok(Connection {
        name: file_stem(path),
        location: file_name(path),
        read_only: target.read_only,
        tls: false,
        link: Link::File(FileLink {
            kept: None,
            database: Arc::new(database),
        }),
    })
}

fn not_an_eggdb_file(shown: &str) -> ConnectFailure {
    ConnectFailure::new(
        format!("{shown} is not an EggDB file"),
        Some("open an .eggdb file, or restore this file with eggdb restore".to_owned()),
        FailureField::File,
    )
}

fn file_failure(shown: &str, error: &Error, creates: bool) -> ConnectFailure {
    let (message, fix) = match error.kind() {
        ErrorKind::NotFound if creates => (
            format!("cannot create {shown}: the directory does not exist"),
            Some("create the directory first, or choose another path with Browse…".to_owned()),
        ),
        ErrorKind::NotFound => (
            format!("cannot open {shown}: no such file"),
            Some(format!(
                "check \"Create the file if it does not exist\", or eggdb create {shown}"
            )),
        ),
        ErrorKind::NotADatabase => return not_an_eggdb_file(shown),
        ErrorKind::Locked => (
            format!("{shown} is open in another process"),
            Some("close the other program, or check \"Open read-only\"".to_owned()),
        ),
        ErrorKind::PermissionDenied => (
            format!("cannot open {shown}: permission denied"),
            Some("check the file's permissions, or check \"Open read-only\"".to_owned()),
        ),
        _ => (format!("cannot open {shown}: {error}"), None),
    };
    ConnectFailure::new(message, fix, FailureField::File)
}

fn tab_client_options(options: &ConnectOptions, first_used_tls: bool) -> ConnectOptions {
    let mut tab_options = options.clone();
    if first_used_tls {
        tab_options.ssl_mode = tab_options.ssl_mode.max(SslMode::Require);
    }
    tab_options
}

fn connect_server(options: &ConnectOptions) -> Result<Connection, ConnectFailure> {
    let client = PgClient::connect(options).map_err(|error| server_failure(options, &error))?;
    let tls = client.tls_version().is_some();
    Ok(Connection {
        name: options.database.clone(),
        location: format!("{}:{}", options.host, options.port),
        read_only: false,
        tls,
        link: Link::Server(ServerLink::new(client, options)),
    })
}

fn connect_resp(options: &RespOptions) -> Result<Connection, ConnectFailure> {
    let client = RespClient::connect(options).map_err(|error| resp_failure(options, &error))?;
    Ok(Connection {
        name: options.host.clone(),
        location: options.place(),
        read_only: false,
        tls: false,
        link: Link::Resp(RespLink {
            options: options.clone(),
            client: Some(client),
        }),
    })
}

/// The card's failure lines for a RESP connection that `error` stopped: the network's lines
/// as for any server, and the server's own line when it refused the password.
pub fn resp_failure(options: &RespOptions, error: &RespConnectError) -> ConnectFailure {
    let place = options.place();
    let network = || Some("check the host name and your network".to_owned());
    match error {
        RespConnectError::Io(io_error) if io_error.kind() == io::ErrorKind::ConnectionRefused => {
            ConnectFailure::new(
                format!("cannot connect to {place}: connection refused"),
                Some("check that the server is running and the port is open".to_owned()),
                FailureField::Host,
            )
        }
        RespConnectError::Io(io_error)
            if matches!(
                io_error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            ConnectFailure::new(
                format!("cannot connect to {place}: timed out after 10 s"),
                network(),
                FailureField::Host,
            )
        }
        RespConnectError::Io(io_error) => ConnectFailure::new(
            format!("cannot connect to {place}: {io_error}"),
            network(),
            FailureField::Host,
        ),
        RespConnectError::Protocol(message) => ConnectFailure::new(
            format!("cannot connect to {place}: {message}"),
            Some("check that the port is a RESP server's".to_owned()),
            FailureField::Host,
        ),
        RespConnectError::Refused(line) => ConnectFailure::new(
            line.clone(),
            Some("check the user name and password".to_owned()),
            FailureField::Password,
        ),
    }
}

/// The failure shown when a `redis://` connection asks for TLS, which the client does not
/// speak over RESP yet: it never connects with less protection than was asked for.
pub fn resp_tls_failure() -> ConnectFailure {
    ConnectFailure::new(
        "the client cannot connect over RESP with TLS yet".to_owned(),
        Some("uncheck \"Use TLS\" only on a network you trust".to_owned()),
        FailureField::Host,
    )
}

/// The card's failure lines for a connection that `error` stopped (`docs/ux/workspace.md`
/// 1.6's table).
pub fn server_failure(options: &ConnectOptions, error: &ClientError) -> ConnectFailure {
    let place = format!("{}:{}", options.host, options.port);
    let network = || Some("check the host name and your network".to_owned());
    match error {
        ClientError::Io(io_error) if io_error.kind() == io::ErrorKind::ConnectionRefused => {
            ConnectFailure::new(
                format!("cannot connect to {place}: connection refused"),
                Some("check that the server is running and the port is open".to_owned()),
                FailureField::Host,
            )
        }
        ClientError::TimedOut => ConnectFailure::new(
            format!("cannot connect to {place}: timed out after 10 s"),
            network(),
            FailureField::Host,
        ),
        ClientError::Io(io_error) => ConnectFailure::new(
            format!("cannot connect to {place}: {io_error}"),
            network(),
            FailureField::Host,
        ),
        ClientError::Tls(message) => {
            let message = if message.contains("does not support TLS") {
                "the server does not offer TLS".to_owned()
            } else {
                message.clone()
            };
            ConnectFailure::new(
                format!("cannot connect to {place}: {message}"),
                Some("uncheck \"Use TLS\" only on a network you trust".to_owned()),
                FailureField::Host,
            )
        }
        ClientError::Authentication(message) => ConnectFailure::new(
            message.clone(),
            Some("check the user name and password".to_owned()),
            FailureField::Password,
        ),
        ClientError::Server { code, message } => {
            let (fix, field) = match code.as_str() {
                state if state.starts_with(AUTHENTICATION_CLASS) => (
                    Some("check the user name and password".to_owned()),
                    FailureField::Password,
                ),
                DATABASE_MISSING => (
                    Some("check the database name, or leave it empty for the default".to_owned()),
                    FailureField::Database,
                ),
                _ => (None, FailureField::Host),
            };
            ConnectFailure {
                code: Some(code.clone()),
                message: message.clone(),
                fix,
                field,
            }
        }
        ClientError::Protocol(message) => ConnectFailure::new(
            format!("cannot connect to {place}: {message}"),
            None,
            FailureField::Host,
        ),
    }
}

/// The failure shown when the server segment's URL does not parse.
pub fn url_failure() -> ConnectFailure {
    ConnectFailure::new(
        "the URL is not a database URL".to_owned(),
        Some("postgresql://user@host:5432/database".to_owned()),
        FailureField::Url,
    )
}

/// The failure shown when a server URL asks for the server's certificate to be checked
/// (`sslmode=verify-ca`, `sslmode=verify-full` or `sslrootcert`), which the client cannot do
/// yet: it never connects with a weaker check than the URL asked for.
pub fn certificate_verification_failure() -> ConnectFailure {
    ConnectFailure::new(
        "the client cannot verify server certificates yet".to_owned(),
        Some(
            "sslmode=require, which encrypts the connection without checking the certificate"
                .to_owned(),
        ),
        FailureField::Url,
    )
}

/// The failure shown when the protocol menu names a protocol the client cannot connect over
/// yet (`label` is the menu's text, `MySQL protocol`).
pub fn unsupported_protocol_failure(label: &str) -> ConnectFailure {
    ConnectFailure::new(
        format!("the client cannot connect over {label} yet"),
        Some("choose \"PostgreSQL protocol\", which eggdb serve always speaks".to_owned()),
        FailureField::Protocol,
    )
}

/// The path a typed File value means: surrounding whitespace cut, a leading `~` expanded to
/// `home`, and a relative path resolved against `current`.
pub fn resolve_typed_path(typed: &str, home: Option<&Path>, current: &Path) -> PathBuf {
    let typed = typed.trim();
    let expanded = match (typed, home) {
        ("~", Some(home)) => home.to_path_buf(),
        (_, Some(home)) if typed.starts_with("~/") || typed.starts_with("~\\") => {
            home.join(typed.get(2..).unwrap_or_default())
        }
        _ => PathBuf::from(typed),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        current.join(expanded)
    }
}

/// The user's home directory from the environment (`HOME`, or `USERPROFILE` on Windows).
pub fn home_directory() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The TLS mode the server segment's `Use TLS` checkbox means.
pub fn ssl_mode(use_tls: bool) -> SslMode {
    if use_tls {
        SslMode::Require
    } else {
        SslMode::Disable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "eggdb-gui-connection-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&directory);
        directory
    }

    fn file(path: PathBuf, create: bool, read_only: bool) -> ConnectTarget {
        ConnectTarget::File(FileTarget {
            path,
            create_if_missing: create,
            read_only,
        })
    }

    fn failure(target: &ConnectTarget) -> ConnectFailure {
        EngineConnector.connect(target).unwrap_err()
    }

    #[test]
    fn typed_paths_cut_whitespace_expand_the_home_and_resolve_against_the_current_directory() {
        let home = Path::new("/home/ada");
        let current = Path::new("/work");
        assert_eq!(
            resolve_typed_path("  ~/data/shop.eggdb ", Some(home), current),
            PathBuf::from("/home/ada/data/shop.eggdb")
        );
        assert_eq!(resolve_typed_path("~", Some(home), current), home);
        assert_eq!(
            resolve_typed_path("shop.eggdb", Some(home), current),
            PathBuf::from("/work/shop.eggdb")
        );
        assert_eq!(
            resolve_typed_path("~/x.eggdb", None, current),
            PathBuf::from("/work/~/x.eggdb")
        );
    }

    fn count_of(connection: &mut Connection, table: &str) -> Option<String> {
        connection
            .run(&format!("SELECT count(*) FROM {table}"))
            .ok()?
            .pop()?
            .rows
            .pop()?
            .pop()?
    }

    #[test]
    fn rolled_back_runs_leave_no_change_whatever_the_statement_starts_with() {
        let path = scratch("rolled-back").join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let mut connection = EngineConnector
            .connect(&file(path.clone(), true, false))
            .unwrap();
        connection.run("CREATE TABLE t (id integer)").unwrap();
        connection.run("INSERT INTO t VALUES (1), (2)").unwrap();
        let cancel = CancelHandle::default();
        for statement in [
            "/* audit */ DELETE FROM t",
            "EXPLAIN (ANALYZE, BUFFERS) -- note\nDELETE FROM t",
            "CREATE TABLE copy AS SELECT * FROM t",
            "EXPLAIN (ANALYZE, BUFFERS) CREATE TABLE copy AS SELECT * FROM t",
        ] {
            let outcomes = connection.run_rolled_back(QueryDialect::PostgreSql, statement, &cancel);
            assert!(outcomes.first().is_some_and(Result::is_ok), "{statement}");
            assert_eq!(count_of(&mut connection, "t").as_deref(), Some("2"));
            assert!(
                connection.run("SELECT * FROM copy").is_err(),
                "{statement} left its table"
            );
        }
    }

    #[test]
    fn bracketed_runs_explain_in_sql_server_and_oracle_and_leave_nothing_behind() {
        let path = scratch("bracketed").join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let mut connection = EngineConnector.connect(&file(path, true, false)).unwrap();
        connection
            .run("CREATE TABLE t (id integer PRIMARY KEY, b integer)")
            .unwrap();
        connection
            .run("INSERT INTO t VALUES (1, 10), (2, 20)")
            .unwrap();
        let cancel = CancelHandle::default();
        let tab = SessionId(3);
        let showplan = BracketedRun {
            before: vec!["SET SHOWPLAN_ALL ON".to_owned()],
            statements: vec!["SELECT * FROM t WHERE id = 1".to_owned()],
            after: vec!["SET SHOWPLAN_ALL OFF".to_owned()],
            rolled_back: false,
        };
        let outcomes =
            connection.run_bracketed_in_session(tab, QueryDialect::SqlServer, &showplan, &cancel);
        assert_eq!(outcomes.len(), 3, "{outcomes:?}");
        let plan = outcomes[1]
            .as_ref()
            .ok()
            .and_then(|tables| tables.last())
            .cloned();
        assert!(
            plan.is_some_and(|table| table.columns.iter().any(|column| column == "NodeId")),
            "{outcomes:?}"
        );
        let failing = BracketedRun {
            statements: vec!["SELECT * FROM missing".to_owned()],
            ..showplan.clone()
        };
        let outcomes =
            connection.run_bracketed_in_session(tab, QueryDialect::SqlServer, &failing, &cancel);
        assert!(outcomes.last().is_some_and(Result::is_ok), "{outcomes:?}");
        let ran = connection.run_in_session(
            tab,
            QueryDialect::SqlServer,
            &["SELECT b FROM t WHERE id = 2".to_owned()],
            &cancel,
        );
        let rows = ran
            .last()
            .and_then(|outcome| outcome.as_ref().ok())
            .and_then(|tables| tables.last())
            .map(|table| table.rows.clone());
        assert_eq!(
            rows,
            Some(vec![vec![Some("20".to_owned())]]),
            "showplan is off again"
        );
        let profile = BracketedRun {
            before: vec!["SET STATISTICS PROFILE ON".to_owned()],
            statements: vec!["DELETE FROM t WHERE id = 1".to_owned()],
            after: vec!["SET STATISTICS PROFILE OFF".to_owned()],
            rolled_back: true,
        };
        let outcomes =
            connection.run_bracketed_in_session(tab, QueryDialect::SqlServer, &profile, &cancel);
        assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
        assert_eq!(count_of(&mut connection, "t").as_deref(), Some("2"));
        let oracle = BracketedRun {
            statements: vec![
                "EXPLAIN PLAN FOR SELECT * FROM t".to_owned(),
                "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)".to_owned(),
            ],
            rolled_back: true,
            ..BracketedRun::default()
        };
        let outcomes = connection.run_bracketed_in_session(
            SessionId(4),
            QueryDialect::Oracle,
            &oracle,
            &cancel,
        );
        assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
        let display = outcomes[2]
            .as_ref()
            .ok()
            .and_then(|tables| tables.last())
            .cloned();
        assert!(
            display.is_some_and(|table| table.columns == ["PLAN_TABLE_OUTPUT"]),
            "{outcomes:?}"
        );
        assert_eq!(
            connection.transaction_status(SessionId(4)),
            TransactionStatus::Idle
        );
    }

    #[test]
    fn a_kept_session_holds_its_transaction_between_runs_until_it_closes() {
        let path = scratch("kept").join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let mut connection = EngineConnector.connect(&file(path, true, false)).unwrap();
        connection.run("CREATE TABLE t (id integer)").unwrap();
        let cancel = CancelHandle::default();
        let tab = SessionId(1);
        let run = |connection: &mut Connection, session: SessionId, sql: &str| {
            connection.run_in_session(
                session,
                QueryDialect::PostgreSql,
                &[sql.to_owned()],
                &cancel,
            )
        };
        assert!(run(&mut connection, tab, "BEGIN").iter().all(Result::is_ok));
        assert!(
            run(&mut connection, tab, "INSERT INTO t VALUES (1)")
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(count_of(&mut connection, "t").as_deref(), Some("0"));
        let mysql = connection.run_in_session(
            tab,
            QueryDialect::MySql,
            &["SELECT 1".to_owned()],
            &CancelHandle::default(),
        );
        let refused = mysql.last().and_then(|outcome| outcome.as_ref().err());
        assert_eq!(
            refused.map(|failure| failure.code.as_str()),
            Some(ACTIVE_TRANSACTION)
        );
        assert_eq!(
            refused.map(|failure| failure.message.as_str()),
            Some(
                "this tab's transaction was begun in PostgreSQL; COMMIT or ROLLBACK it before running MySQL"
            )
        );
        let seen = run(&mut connection, tab, "SELECT count(*) FROM t");
        let rows = seen
            .last()
            .and_then(|outcome| outcome.as_ref().ok())
            .and_then(|tables| tables.last())
            .map(|table| table.rows.clone());
        assert_eq!(rows, Some(vec![vec![Some("1".to_owned())]]));
        assert!(
            run(&mut connection, tab, "COMMIT")
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(count_of(&mut connection, "t").as_deref(), Some("1"));
        assert!(run(&mut connection, tab, "BEGIN").iter().all(Result::is_ok));
        assert!(
            run(&mut connection, tab, "INSERT INTO t VALUES (2)")
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(
            connection.transaction_status(tab),
            TransactionStatus::InBlock
        );
        connection.close_session(tab);
        assert_eq!(connection.transaction_status(tab), TransactionStatus::Idle);
        assert_eq!(count_of(&mut connection, "t").as_deref(), Some("1"));
        let other = run(&mut connection, SessionId(2), "INSERT INTO t VALUES (3)");
        assert!(other.iter().all(Result::is_ok), "{other:?}");
        assert_eq!(count_of(&mut connection, "t").as_deref(), Some("2"));
    }

    #[test]
    fn a_cancelled_run_leaves_the_kept_session_ready_for_the_next_one() {
        let path = scratch("kept-cancel").join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let mut connection = EngineConnector.connect(&file(path, true, false)).unwrap();
        let cancelled = CancelHandle::default();
        cancelled.cancel();
        let tab = SessionId(7);
        let stopped = connection.run_in_session(
            tab,
            QueryDialect::PostgreSql,
            &["SELECT 1".to_owned()],
            &cancelled,
        );
        assert_eq!(
            stopped
                .last()
                .and_then(|outcome| outcome.as_ref().err())
                .map(|failure| failure.code.as_str()),
            Some(QUERY_CANCELED)
        );
        let next = connection.run_in_session(
            tab,
            QueryDialect::PostgreSql,
            &["SELECT 1".to_owned()],
            &CancelHandle::default(),
        );
        assert!(next.iter().all(Result::is_ok), "{next:?}");
    }

    struct TestServer {
        port: u16,
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    const SERVER_PASSWORD: &str = "correct horse";

    fn start_server() -> TestServer {
        let address = "127.0.0.1:0".parse().unwrap();
        let listener = eggdb_pgwire::listen_on_loopback(address).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let config = eggdb_pgwire::ServerConfig {
            user: "ada".to_owned(),
            verifier: eggdb_pgwire::ScramVerifier::derive(
                SERVER_PASSWORD.as_bytes(),
                b"0123456789abcdef",
                4096,
            ),
            database: "shop".to_owned(),
            max_connections: 8,
            authentication_timeout: std::time::Duration::from_secs(5),
            idle_timeout: None,
            memory_budget: Arc::default(),
            auth_rules: None,
            tls: None,
        };
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let database = Database::in_memory_with(Options::new()).unwrap();
            let store = database.sql_store();
            let stopped = move || thread_stop.load(std::sync::atomic::Ordering::Acquire);
            let _ = eggdb_pgwire::serve(&store, &listener, &config, &stopped, &|_: &str| {});
        });
        TestServer { port, stop }
    }

    fn a_tab_s_search_path_never_reaches_a_drop(connection: &mut Connection) {
        connection.run("CREATE SCHEMA app").unwrap();
        connection
            .run("CREATE TABLE app.orders (id integer)")
            .unwrap();
        connection.run("CREATE TABLE orders (id integer)").unwrap();
        let tab = SessionId(1);
        let in_tab = |connection: &mut Connection, sql: &str| {
            connection.run_in_session(
                tab,
                QueryDialect::PostgreSql,
                &[sql.to_owned()],
                &CancelHandle::default(),
            )
        };
        let set = in_tab(connection, "SET search_path = app, public");
        assert!(set.iter().all(Result::is_ok), "{set:?}");
        let dropped = connection.run_script(
            QueryDialect::PostgreSql,
            &["DROP TABLE orders;".to_owned()],
            &CancelHandle::default(),
        );
        assert!(dropped.iter().all(Result::is_ok), "{dropped:?}");
        let kept = in_tab(connection, "SELECT count(*) FROM app.orders");
        assert!(kept.iter().all(Result::is_ok), "{kept:?}");
        let gone = in_tab(connection, "SELECT count(*) FROM public.orders");
        assert!(gone.last().is_some_and(Result::is_err), "{gone:?}");
        connection.close_session(tab);
    }

    #[test]
    fn a_drop_on_a_file_runs_on_a_session_no_tab_uses() {
        let path = scratch("drop-session").join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let mut connection = EngineConnector.connect(&file(path, true, false)).unwrap();
        a_tab_s_search_path_never_reaches_a_drop(&mut connection);
    }

    #[test]
    fn a_drop_on_a_server_runs_on_a_session_no_tab_uses() {
        let running = start_server();
        let options = ConnectOptions {
            password: Some(SERVER_PASSWORD.to_owned()),
            ..server(running.port)
        };
        let mut connection = EngineConnector
            .connect(&ConnectTarget::Server(options))
            .unwrap();
        a_tab_s_search_path_never_reaches_a_drop(&mut connection);
    }

    #[test]
    fn on_a_server_a_drop_outside_the_tabs_stays_after_a_tab_s_transaction_rolls_back() {
        let running = start_server();
        let options = ConnectOptions {
            password: Some(SERVER_PASSWORD.to_owned()),
            ..server(running.port)
        };
        let mut connection = EngineConnector
            .connect(&ConnectTarget::Server(options))
            .unwrap();
        connection.run("CREATE TABLE x (id integer)").unwrap();
        connection.run("CREATE TABLE y (id integer)").unwrap();
        let tab = SessionId(1);
        let other_tab = SessionId(2);
        let run = |connection: &mut Connection, session: SessionId, sql: &str| {
            connection.run_in_session(
                session,
                QueryDialect::PostgreSql,
                &[sql.to_owned()],
                &CancelHandle::default(),
            )
        };
        assert!(
            run(&mut connection, tab, "BEGIN READ ONLY")
                .iter()
                .all(Result::is_ok)
        );
        assert!(
            run(&mut connection, other_tab, "SELECT 1")
                .iter()
                .all(Result::is_ok)
        );
        assert!(
            run(&mut connection, tab, "SELECT * FROM missing")
                .last()
                .is_some_and(Result::is_err)
        );
        assert_eq!(
            connection.transaction_status(tab),
            TransactionStatus::Failed
        );
        assert_eq!(
            connection.transaction_status(other_tab),
            TransactionStatus::Idle
        );
        connection.run("DROP TABLE x").unwrap();
        assert!(connection.catalog().is_ok());
        assert_eq!(connection.table_names().unwrap(), vec!["y".to_owned()]);
        connection.close_session(tab);
        connection.close_session(other_tab);
        assert_eq!(connection.transaction_status(tab), TransactionStatus::Idle);
        assert_eq!(connection.table_names().unwrap(), vec!["y".to_owned()]);
        let again = run(&mut connection, tab, "SELECT count(*) FROM y");
        assert!(again.iter().all(Result::is_ok), "{again:?}");
    }

    #[test]
    fn a_created_file_opens_read_write_and_reopens_read_only() {
        let path = scratch("create").join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let mut created = EngineConnector
            .connect(&file(path.clone(), true, false))
            .unwrap();
        assert_eq!(created.name(), "shop");
        assert_eq!(created.location(), "shop.eggdb");
        assert_eq!(created.status_text(), "Connected · shop.eggdb · read-write");
        assert_eq!(created.toast(), "Opened shop.eggdb");
        created.run("CREATE TABLE orders (id integer)").unwrap();
        created.run("CREATE TABLE customers (id integer)").unwrap();
        let tables = created.run("SELECT 1 AS one").unwrap();
        assert_eq!(
            tables.last().map(|table| table.columns.clone()),
            Some(vec!["one".to_owned()])
        );
        assert_eq!(created.table_names().unwrap(), vec!["customers", "orders"]);
        drop(created);
        let reader = EngineConnector
            .connect(&file(path.clone(), false, true))
            .unwrap();
        assert!(reader.read_only());
        assert_eq!(reader.status_text(), "Connected · shop.eggdb · read-only");
        drop(reader);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_without_create_says_the_spec_lines() {
        let path = scratch("missing").join("nothing.eggdb");
        let shown = path.display().to_string();
        let error = failure(&file(path, false, false));
        assert_eq!(
            error.error_line(),
            format!("Error: cannot open {shown}: no such file")
        );
        assert_eq!(
            error.try_line(),
            Some(format!(
                "Try: check \"Create the file if it does not exist\", or eggdb create {shown}"
            ))
        );
        assert_eq!(error.field, FailureField::File);
    }

    #[test]
    fn a_missing_directory_with_create_says_the_spec_lines() {
        let path = scratch("no-directory").join("absent").join("shop.eggdb");
        let shown = path.display().to_string();
        let error = failure(&file(path, true, false));
        assert_eq!(
            error.error_line(),
            format!("Error: cannot create {shown}: the directory does not exist")
        );
        assert_eq!(
            error.try_line().as_deref(),
            Some("Try: create the directory first, or choose another path with Browse…")
        );
    }

    #[test]
    fn a_file_of_another_kind_and_a_directory_are_not_eggdb_files() {
        let directory = scratch("other-kind");
        let path = directory.join("notes.txt");
        std::fs::write(&path, "these are notes, not database pages. ".repeat(600)).unwrap();
        for target in [path.clone(), directory.clone()] {
            let shown = target.display().to_string();
            let error = failure(&file(target, false, false));
            assert_eq!(
                error.error_line(),
                format!("Error: {shown} is not an EggDB file")
            );
            assert_eq!(
                error.try_line().as_deref(),
                Some("Try: open an .eggdb file, or restore this file with eggdb restore")
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_open_for_writing_elsewhere_is_locked() {
        let path = scratch("locked").join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let holder = EngineConnector
            .connect(&file(path.clone(), true, false))
            .unwrap();
        let shown = path.display().to_string();
        let error = failure(&file(path.clone(), false, false));
        assert_eq!(
            error.error_line(),
            format!("Error: {shown} is open in another process")
        );
        assert_eq!(
            error.try_line().as_deref(),
            Some("Try: close the other program, or check \"Open read-only\"")
        );
        drop(holder);
        let _ = std::fs::remove_file(&path);
    }

    fn server(port: u16) -> ConnectOptions {
        ConnectOptions {
            host: "127.0.0.1".to_owned(),
            port,
            user: "ada".to_owned(),
            database: "shop".to_owned(),
            password: None,
            ssl_mode: SslMode::Disable,
            ssl_root_cert: None,
            replication: false,
        }
    }

    #[test]
    fn tab_clients_after_a_tls_connect_require_tls_and_keep_its_certificate_checks() {
        let preferred = ConnectOptions {
            ssl_mode: SslMode::Prefer,
            ..server(5432)
        };
        assert_eq!(
            tab_client_options(&preferred, true).ssl_mode,
            SslMode::Require
        );
        assert_eq!(
            tab_client_options(&preferred, false).ssl_mode,
            SslMode::Prefer
        );
        let disabled = server(5432);
        assert_eq!(
            tab_client_options(&disabled, false).ssl_mode,
            SslMode::Disable
        );
        for verified in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let options = ConnectOptions {
                ssl_mode: verified,
                ssl_root_cert: Some("/etc/eggdb/root.pem".into()),
                ..server(5432)
            };
            let tab = tab_client_options(&options, true);
            assert_eq!(tab.ssl_mode, verified);
            assert_eq!(tab.ssl_root_cert, options.ssl_root_cert);
            assert_eq!(tab.host, options.host);
            assert_eq!(tab.user, options.user);
        }
    }

    #[test]
    fn a_refused_port_says_connection_refused_and_is_about_the_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = failure(&ConnectTarget::Server(server(port)));
        assert_eq!(
            error.error_line(),
            format!("Error: cannot connect to 127.0.0.1:{port}: connection refused")
        );
        assert_eq!(
            error.try_line().as_deref(),
            Some("Try: check that the server is running and the port is open")
        );
        assert_eq!(error.field, FailureField::Host);
    }

    #[test]
    fn server_errors_keep_their_code_and_point_at_their_field() {
        let options = server(5433);
        let authentication = server_failure(
            &options,
            &ClientError::Server {
                code: "28P01".to_owned(),
                message: "password authentication failed for user \"ada\"".to_owned(),
            },
        );
        assert_eq!(
            authentication.error_line(),
            "Error 28P01: password authentication failed for user \"ada\""
        );
        assert_eq!(authentication.field, FailureField::Password);
        let unauthorized = server_failure(
            &options,
            &ClientError::Server {
                code: "28000".to_owned(),
                message: "no pg_hba.conf entry for user \"ada\"".to_owned(),
            },
        );
        assert_eq!(unauthorized.field, FailureField::Password);
        assert_eq!(
            unauthorized.try_line().as_deref(),
            Some("Try: check the user name and password")
        );
        let missing = server_failure(
            &options,
            &ClientError::Server {
                code: "3D000".to_owned(),
                message: "database \"shop\" does not exist".to_owned(),
            },
        );
        assert_eq!(
            missing.try_line().as_deref(),
            Some("Try: check the database name, or leave it empty for the default")
        );
        assert_eq!(missing.field, FailureField::Database);
        let tls = server_failure(
            &options,
            &ClientError::Tls(
                "the server does not support TLS and the URL says sslmode=require".to_owned(),
            ),
        );
        assert_eq!(
            tls.error_line(),
            "Error: cannot connect to 127.0.0.1:5433: the server does not offer TLS"
        );
        let timed_out = server_failure(&options, &ClientError::TimedOut);
        assert_eq!(
            timed_out.error_line(),
            "Error: cannot connect to 127.0.0.1:5433: timed out after 10 s"
        );
        assert_eq!(
            url_failure().try_line().as_deref(),
            Some("Try: postgresql://user@host:5432/database")
        );
    }

    #[test]
    fn an_in_memory_connection_is_named_after_the_dropped_file() {
        let mut connection = Connection::in_memory("shop.eggdb").unwrap();
        assert_eq!(connection.name(), "shop");
        assert!(!connection.is_server());
        assert_eq!(connection.table_names().unwrap(), Vec::<String>::new());
    }
}
