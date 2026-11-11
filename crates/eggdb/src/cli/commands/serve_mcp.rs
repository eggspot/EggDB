use eggdb::Database;
use eggdb::sql::Store;
use eggdb_mcp::audit::AuditLog;
use eggdb_mcp::{DatabaseOpener, OpenRefusal, Server, ServerConfig};
use eggdb_sql::VisibleTables;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

type McpSession<'d> = (Option<String>, Server<'d, SharedOpener<'d>>);

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
    tokens: Vec<AgentToken>,
    tables: Option<VisibleTables>,
    database_name: String,
    audit: PathBuf,
    sessions: Mutex<HashMap<String, McpSession<'d>>>,
    order: Mutex<Vec<String>>,
}

#[derive(Clone)]
pub(super) struct AgentToken {
    role: Option<String>,
    token: String,
}

pub(super) fn read_tokens(path: &std::path::Path) -> Result<Vec<AgentToken>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read --mcp-token-file {}: {error}", path.display()))?;
    let wrong = |line: usize| {
        format!(
            "--mcp-token-file {} line {line}: write a token of at least {MIN_TOKEN_BYTES} printable ASCII characters (such as from: openssl rand -hex 32), alone or after the stored role the agent runs as",
            path.display()
        )
    };
    let mut tokens = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let (role, token) = match words.as_slice() {
            [] => continue,
            [token] => (None, *token),
            [role, token] => (Some((*role).to_owned()), *token),
            _ => return Err(wrong(index + 1)),
        };
        if token.len() < MIN_TOKEN_BYTES || !token.is_ascii() {
            return Err(wrong(index + 1));
        }
        if tokens.iter().any(|known: &AgentToken| known.token == token) {
            return Err(format!(
                "--mcp-token-file {} line {}: this token is already on an earlier line; give each agent its own token",
                path.display(),
                index + 1
            ));
        }
        tokens.push(AgentToken {
            role,
            token: token.to_owned(),
        });
    }
    if tokens.is_empty() {
        return Err(wrong(1));
    }
    Ok(tokens)
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
        tokens: Vec<AgentToken>,
        tables: Option<VisibleTables>,
        audit: PathBuf,
    ) -> Self {
        Self {
            opener,
            tokens,
            tables,
            database_name: database_name.to_owned(),
            audit,
            sessions: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    fn agent(&self, request: &eggdb_http::Request) -> Option<Option<String>> {
        let given = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))?
            .trim();
        let mut found = None;
        for agent in &self.tokens {
            if eggdb_crypto::constant_time_eq(given.as_bytes(), agent.token.as_bytes()) {
                found = Some(agent.role.clone());
            }
        }
        found
    }

    fn new_session(&self, role: Option<String>) -> Option<(String, Server<'d, SharedOpener<'d>>)> {
        let config = ServerConfig {
            database_name: self.database_name.clone(),
            server_version: eggdb::VERSION.to_owned(),
            time_limit: TIME_LIMIT,
            audit: AuditLog::new(self.audit.clone()),
            allow_write: false,
            tables: self.tables.clone(),
            call_gap: Duration::ZERO,
            agent_role: role,
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
        let Some(role) = self.agent(request) else {
            return eggdb_http::Response::text(401, "a Bearer token is required")
                .with_header("WWW-Authenticate", "Bearer");
        };
        if request.method == "DELETE" {
            if let Some(id) = request.header(SESSION_HEADER) {
                let mut sessions = self
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if sessions.get(id).is_some_and(|(owner, _)| *owner == role) {
                    sessions.remove(id);
                }
            }
            return eggdb_http::Response::text(200, "session ended");
        }
        if request.method != "POST" {
            return eggdb_http::Response::text(
                405,
                "this endpoint takes POST; EggDB sends no server-initiated messages, so GET opens no event stream",
            )
            .with_header("Allow", "POST, DELETE");
        }
        let accepted = request
            .header("accept")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let streamed = accepted.contains("text/event-stream")
            && !accepted.contains("application/json")
            && !accepted.contains("*/*");
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (id, created) = match request.header(SESSION_HEADER) {
            Some(id) if sessions.get(id).is_some_and(|(owner, _)| *owner == role) => {
                (id.to_owned(), false)
            }
            Some(_) => return json_error(404, "unknown or ended MCP session"),
            None => {
                let Some((id, server)) = self.new_session(role.clone()) else {
                    return json_error(500, "the MCP session could not start");
                };
                let mut order = self
                    .order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if order.len() >= MAX_SESSIONS {
                    let oldest = order.remove(0);
                    sessions.remove(&oldest);
                }
                order.push(id.clone());
                sessions.insert(id.clone(), (role, server));
                (id, true)
            }
        };
        let Some((_, server)) = sessions.get_mut(&id) else {
            return json_error(500, "the MCP session vanished");
        };
        let answer = server.handle(&request.body);
        let response = match answer {
            Some(answer) if streamed => {
                let mut event = b"event: message\ndata: ".to_vec();
                event.extend(answer.into_bytes().into_iter().filter(|byte| *byte != b'\n'));
                event.extend_from_slice(b"\n\n");
                eggdb_http::Response::ok("text/event-stream", event)
                    .with_header("Cache-Control", "no-cache")
            }
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

#[cfg(test)]
mod tests {
    use super::read_tokens;

    #[test]
    fn a_token_given_twice_is_refused() {
        let folder = std::env::temp_dir().join(format!("eggdb-mcp-tokens-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("tokens");
        let token = "0123456789abcdef0123456789abcdef";
        std::fs::write(&file, format!("reporter {token}\n{token}\n")).unwrap();
        let refused = read_tokens(&file).err().unwrap_or_default();
        assert!(refused.contains("line 2"), "{refused}");
        std::fs::write(&file, format!("reporter {token}\nanalyst {token}x\n")).unwrap();
        assert_eq!(read_tokens(&file).map(|tokens| tokens.len()), Ok(2));
    }
}
