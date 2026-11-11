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
const MAX_TIME_LIMIT: Duration = Duration::from_secs(300);
const MIN_TOKEN_BYTES: usize = 16;
const QUOTA_WINDOW: Duration = Duration::from_secs(60);

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
    usage: Mutex<HashMap<usize, (std::time::Instant, u32)>>,
}

#[derive(Clone)]
pub(super) struct AgentToken {
    role: Option<String>,
    token: String,
    time_limit: Duration,
    calls_per_minute: Option<u32>,
    writes: bool,
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
        let (options, words): (Vec<&str>, Vec<&str>) =
            line.split_whitespace().partition(|word| word.contains('='));
        let (role, token) = match words.as_slice() {
            [] if options.is_empty() => continue,
            [token] => (None, *token),
            [role, token] => (Some((*role).to_owned()), *token),
            _ => return Err(wrong(index + 1)),
        };
        let mut time_limit = TIME_LIMIT;
        let mut calls_per_minute = None;
        let mut writes = false;
        for option in options {
            let (name, value) = option.split_once('=').unwrap_or((option, ""));
            let bad = || {
                format!(
                    "--mcp-token-file {} line {}: {name}= takes a whole number (time_limit in seconds up to {}, calls_per_minute at least 1)",
                    path.display(),
                    index + 1,
                    MAX_TIME_LIMIT.as_secs()
                )
            };
            match name {
                "time_limit" => {
                    let seconds: u64 = value.parse().map_err(|_| bad())?;
                    if seconds == 0 || seconds > MAX_TIME_LIMIT.as_secs() {
                        return Err(bad());
                    }
                    time_limit = Duration::from_secs(seconds);
                }
                "writes" => match value {
                    "confirmed" => writes = true,
                    "none" => writes = false,
                    _ => {
                        return Err(format!(
                            "--mcp-token-file {} line {}: writes= takes confirmed or none",
                            path.display(),
                            index + 1
                        ));
                    }
                },
                "calls_per_minute" => {
                    let calls: u32 = value.parse().map_err(|_| bad())?;
                    if calls == 0 {
                        return Err(bad());
                    }
                    calls_per_minute = Some(calls);
                }
                _ => {
                    return Err(format!(
                        "--mcp-token-file {} line {}: unknown option {name}=; use time_limit=, calls_per_minute= or writes=",
                        path.display(),
                        index + 1
                    ));
                }
            }
        }
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
            time_limit,
            calls_per_minute,
            writes,
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
            usage: Mutex::new(HashMap::new()),
        }
    }

    fn agent_index(&self, request: &eggdb_http::Request) -> Option<usize> {
        let given = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))?
            .trim();
        let mut found = None;
        for (index, agent) in self.tokens.iter().enumerate() {
            if eggdb_crypto::constant_time_eq(given.as_bytes(), agent.token.as_bytes()) {
                found = Some(index);
            }
        }
        found
    }

    fn over_quota(&self, index: usize) -> Option<u64> {
        let limit = self.tokens.get(index)?.calls_per_minute?;
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let entry = usage.entry(index).or_insert((now, 0));
        if now.duration_since(entry.0) >= QUOTA_WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= limit {
            let wait = QUOTA_WINDOW.saturating_sub(now.duration_since(entry.0));
            return Some(wait.as_secs().max(1));
        }
        entry.1 += 1;
        None
    }

    fn writes_allowed(&self, role: &Option<String>) -> bool {
        self.tokens
            .iter()
            .find(|agent| &agent.role == role)
            .is_some_and(|agent| agent.writes)
    }

    fn time_limit_of(&self, role: &Option<String>) -> Duration {
        self.tokens
            .iter()
            .find(|agent| &agent.role == role)
            .map_or(TIME_LIMIT, |agent| agent.time_limit)
    }

    fn new_session(&self, role: Option<String>) -> Option<(String, Server<'d, SharedOpener<'d>>)> {
        let config = ServerConfig {
            database_name: self.database_name.clone(),
            server_version: eggdb::VERSION.to_owned(),
            time_limit: self.time_limit_of(&role),
            audit: AuditLog::new(self.audit.clone()),
            allow_write: self.writes_allowed(&role),
            tables: self.tables.clone(),
            call_gap: Duration::ZERO,
            agent_role: role,
            exit_process: None,
            asks_mid_call: false,
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
        let Some(index) = self.agent_index(request) else {
            return eggdb_http::Response::text(401, "a Bearer token is required")
                .with_header("WWW-Authenticate", "Bearer");
        };
        let role = self.tokens[index].role.clone();
        if request.method == "POST"
            && let Some(wait) = self.over_quota(index)
        {
            return json_error(429, "this agent's calls_per_minute quota is spent")
                .with_header("Retry-After", &wait.to_string());
        }
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
