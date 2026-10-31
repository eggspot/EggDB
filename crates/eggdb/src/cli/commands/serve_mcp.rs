use eggdb::Database;
use eggdb::sql::Store;
use eggdb_mcp::audit::AuditLog;
use eggdb_mcp::{DatabaseOpener, OpenRefusal, Server, ServerConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const MAX_SESSIONS: usize = 64;
const SESSION_HEADER: &str = "mcp-session-id";
const TIME_LIMIT: Duration = Duration::from_secs(5);
const MIN_TOKEN_BYTES: usize = 16;

pub(super) struct SharedOpener<'d> {
    pub(super) db: &'d Database,
}

impl<'d> DatabaseOpener for SharedOpener<'d> {
    type Store = Store<'d>;

    fn open_read_only(&self) -> Result<Store<'d>, OpenRefusal> {
        Ok(Store(self.db))
    }

    fn open_read_write(&self) -> Result<Store<'d>, OpenRefusal> {
        Err(OpenRefusal::NotWritable(
            "the MCP endpoint of eggdb serve is read-only".to_owned(),
        ))
    }

    fn close(&self, _store: Store<'d>) -> Result<(), OpenRefusal> {
        Ok(())
    }
}

pub(super) struct McpEndpoint<'d> {
    opener: &'d SharedOpener<'d>,
    token: String,
    database_name: String,
    audit: PathBuf,
    sessions: Mutex<HashMap<String, Server<'d, SharedOpener<'d>>>>,
    order: Mutex<Vec<String>>,
}

pub(super) fn read_token(path: &std::path::Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read --mcp-token-file {}: {error}", path.display()))?;
    let token = text.lines().next().unwrap_or_default().trim().to_owned();
    if token.len() < MIN_TOKEN_BYTES || !token.is_ascii() || token.contains(char::is_whitespace) {
        return Err(format!(
            "--mcp-token-file {} must hold a token of at least {MIN_TOKEN_BYTES} printable ASCII characters on its first line, such as from: openssl rand -hex 32",
            path.display()
        ));
    }
    Ok(token)
}

fn json_error(status: u16, message: &str) -> eggdb_http::Response {
    eggdb_http::Response {
        status,
        content_type: "application/json".to_owned(),
        body: format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32600,\"message\":\"{message}\"}}}}"
        )
        .into_bytes(),
        headers: Vec::new(),
    }
}

fn session_id() -> String {
    let mut bytes = [0u8; 16];
    if eggdb_platform::secure_random::fill(&mut bytes).is_err() {
        return String::new();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl<'d> McpEndpoint<'d> {
    pub(super) fn new(
        opener: &'d SharedOpener<'d>,
        database_name: &str,
        token: String,
        audit: PathBuf,
    ) -> Self {
        Self {
            opener,
            token,
            database_name: database_name.to_owned(),
            audit,
            sessions: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    fn authorized(&self, request: &eggdb_http::Request) -> bool {
        let Some(given) = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        eggdb_crypto::constant_time_eq(given.trim().as_bytes(), self.token.as_bytes())
    }

    fn new_session(&self) -> Option<(String, Server<'d, SharedOpener<'d>>)> {
        let config = ServerConfig {
            database_name: self.database_name.clone(),
            server_version: eggdb::VERSION.to_owned(),
            time_limit: TIME_LIMIT,
            audit: AuditLog::new(self.audit.clone()),
            allow_write: false,
            tables: None,
            call_gap: Duration::ZERO,
            exit_process: None,
        };
        let server = Server::new(self.opener, config).ok()?;
        let id = session_id();
        (!id.is_empty()).then_some((id, server))
    }

    pub(super) fn answer(&self, request: &eggdb_http::Request) -> eggdb_http::Response {
        if request.path != "/mcp" {
            return eggdb_http::Response::text(404, "not found; MCP is at /mcp");
        }
        if request.header("origin").is_some() {
            return eggdb_http::Response::text(403, "requests from web pages are refused");
        }
        if !self.authorized(request) {
            return eggdb_http::Response::text(401, "a Bearer token is required")
                .with_header("WWW-Authenticate", "Bearer");
        }
        if request.method == "DELETE" {
            if let Some(id) = request.header(SESSION_HEADER) {
                let mut sessions = self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                sessions.remove(id);
            }
            return eggdb_http::Response::text(200, "session ended");
        }
        if request.method != "POST" {
            return eggdb_http::Response::text(405, "this endpoint takes POST (no event streams yet)");
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (id, created) = match request.header(SESSION_HEADER) {
            Some(id) if sessions.contains_key(id) => (id.to_owned(), false),
            Some(_) => return json_error(404, "unknown or ended MCP session"),
            None => {
                let Some((id, server)) = self.new_session() else {
                    return json_error(500, "the MCP session could not start");
                };
                let mut order = self.order.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if order.len() >= MAX_SESSIONS {
                    let oldest = order.remove(0);
                    sessions.remove(&oldest);
                }
                order.push(id.clone());
                sessions.insert(id.clone(), server);
                (id, true)
            }
        };
        let Some(server) = sessions.get_mut(&id) else {
            return json_error(500, "the MCP session vanished");
        };
        let answer = server.handle(&request.body);
        let response = match answer {
            Some(answer) => eggdb_http::Response::ok("application/json", answer),
            None => eggdb_http::Response {
                status: 202,
                content_type: "application/json".to_owned(),
                body: Vec::new(),
                headers: Vec::new(),
            },
        };
        if created {
            response.with_header("Mcp-Session-Id", &id)
        } else {
            response
        }
    }
}
