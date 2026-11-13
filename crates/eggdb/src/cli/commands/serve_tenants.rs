use super::serve_databases::{AttachedDatabase, ServedDirectory};
use eggdb_http::{Request, Response};
use eggdb_sql::{DatabaseDirectory, Dialect, KeyValueStore, ServedDatabases, Session};
use std::collections::BTreeSet;
use std::time::Duration;

const REGISTRY: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_tenants; \
CREATE TABLE IF NOT EXISTS eggdb_tenants.registry (name text PRIMARY KEY, state text NOT NULL, quota_bytes bigint, created_at timestamptz NOT NULL DEFAULT now(), changed_at timestamptz NOT NULL DEFAULT now(), deleted_at timestamptz)";
const PAGE_MOST: u32 = 1_000;
const PURGE_EVERY: Duration = Duration::from_secs(60);
pub(super) const DELETED_KEPT_DAYS: u32 = 7;

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn rows(store: &impl KeyValueStore, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut found = Vec::new();
    session
        .execute(sql, &mut |result| {
            if result.columns.is_some() {
                found = result.rows.iter().map(|row| row.iter().map(|value| value.to_text()).collect()).collect();
            }
        })
        .map_err(|error| error.message().to_owned())?;
    let _ = session.close();
    Ok(found)
}

/// The tenants the registry says to refuse: suspended or deleted (D1113); a tenant over its
/// quota is still served, and the registry says so.
pub(super) fn refused_tenants(store: &impl KeyValueStore) -> Result<BTreeSet<String>, String> {
    rows(store, REGISTRY)?;
    Ok(rows(store, "SELECT name FROM eggdb_tenants.registry WHERE state IN ('suspended', 'deleted')")?
        .into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .collect())
}

fn reply(status: u16, body: String) -> Response {
    let mut response = Response::ok("application/json", body.into_bytes());
    response.status = status;
    response
}

fn refused(status: u16, reason: &str) -> Response {
    reply(status, format!("{{\"error\":{}}}", json_string(reason)))
}

fn number_field(body: &str, name: &str) -> Option<i64> {
    let node = eggdb_sql::json::parse_document(body).ok()?;
    let eggdb_sql::json::Json::Object(fields) = node.value else {
        return None;
    };
    fields.into_iter().find(|(field, _)| field == name).and_then(|(_, value)| match value.value {
        eggdb_sql::json::Json::Number(text) => text.parse().ok(),
        _ => None,
    })
}

fn text_field(body: &str, name: &str) -> Option<String> {
    let node = eggdb_sql::json::parse_document(body).ok()?;
    let eggdb_sql::json::Json::Object(fields) = node.value else {
        return None;
    };
    fields.into_iter().find(|(field, _)| field == name).and_then(|(_, value)| match value.value {
        eggdb_sql::json::Json::String(text) => Some(text),
        _ => None,
    })
}

/// The tenant lifecycle API (D58, D1113) over a server keeping a database per tenant.
pub(super) struct TenantApi<'a, S: KeyValueStore> {
    pub(super) registry: &'a S,
    pub(super) served: &'a ServedDirectory<'a>,
    pub(super) password: &'a str,
}

impl<S: KeyValueStore> TenantApi<'_, S> {
    fn row_json(&self, row: &[Option<String>]) -> String {
        let text = |at: usize| row.get(at).cloned().flatten();
        let name = text(0).unwrap_or_default();
        let size = self
            .served
            .tenant_files()
            .and_then(|tenants| std::fs::metadata(tenants.path_of(&name)).ok())
            .map_or(0, |metadata| metadata.len());
        format!(
            "{{\"name\":{},\"state\":{},\"quota_bytes\":{},\"size_bytes\":{size},\"created_at\":{},\"changed_at\":{},\"deleted_at\":{}}}",
            json_string(&name),
            json_string(&text(1).unwrap_or_default()),
            text(2).unwrap_or_else(|| "null".to_owned()),
            json_string(&text(3).unwrap_or_default()),
            json_string(&text(4).unwrap_or_default()),
            text(5).map_or_else(|| "null".to_owned(), |at| json_string(&at)),
        )
    }

    fn tenant(&self, name: &str) -> Result<Option<Vec<Option<String>>>, String> {
        Ok(rows(
            self.registry,
            &format!("SELECT name, state, quota_bytes, created_at, changed_at, deleted_at FROM eggdb_tenants.registry WHERE name = {}", literal(name)),
        )?
        .into_iter()
        .next())
    }

    fn set_state(&self, name: &str, state: &str) -> Result<Response, String> {
        if self.tenant(name)?.is_none() {
            return Ok(refused(404, "no such tenant"));
        }
        let deleted = if state == "deleted" { "now()" } else { "NULL" };
        rows(
            self.registry,
            &format!("UPDATE eggdb_tenants.registry SET state = {}, changed_at = now(), deleted_at = {deleted} WHERE name = {}", literal(state), literal(name)),
        )?;
        if let Some(tenants) = self.served.tenant_files() {
            tenants.set_refused(name, state != "active");
        }
        Ok(match self.tenant(name)? {
            Some(row) => reply(200, self.row_json(&row)),
            None => refused(404, "no such tenant"),
        })
    }

    fn create(&self, body: &str) -> Result<Response, String> {
        let Some(name) = text_field(body, "name") else {
            return Ok(refused(400, "a tenant needs a name"));
        };
        if !eggdb_sql::is_valid_database_name(&name) {
            return Ok(refused(400, "a tenant's name must name a database file: letters, digits, _ and -"));
        }
        if self.tenant(&name)?.is_some() {
            return Ok(refused(409, "the tenant exists"));
        }
        if let Err(error) = self.served.create(&name) {
            return Ok(refused(409, &format!("{error:?}")));
        }
        let quota = number_field(body, "quota_bytes").map_or_else(|| "NULL".to_owned(), |bytes| bytes.max(0).to_string());
        rows(self.registry, &format!("INSERT INTO eggdb_tenants.registry (name, state, quota_bytes) VALUES ({}, 'active', {quota})", literal(&name)))?;
        Ok(match self.tenant(&name)? {
            Some(row) => reply(201, self.row_json(&row)),
            None => refused(500, "the tenant was not recorded"),
        })
    }

    fn list(&self, query: Option<&str>) -> Result<Response, String> {
        let parameters: Vec<(String, String)> = query
            .unwrap_or("")
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .filter_map(|(name, value)| Some((name.to_owned(), eggdb_http::percent_decoded(value)?)))
            .collect();
        let value = |wanted: &str| parameters.iter().find(|(name, _)| name == wanted).map(|(_, value)| value.clone());
        let limit = value("limit").and_then(|limit| limit.parse::<u32>().ok()).unwrap_or(100).clamp(1, PAGE_MOST);
        let after = value("after").map_or_else(String::new, |after| format!("WHERE name > {}", literal(&after)));
        let found = rows(
            self.registry,
            &format!("SELECT name, state, quota_bytes, created_at, changed_at, deleted_at FROM eggdb_tenants.registry {after} ORDER BY name LIMIT {limit}"),
        )?;
        let next = if found.len() == limit as usize {
            found.last().and_then(|row| row.first().cloned().flatten()).map_or_else(|| "null".to_owned(), |name| json_string(&name))
        } else {
            "null".to_owned()
        };
        let items: Vec<String> = found.iter().map(|row| self.row_json(row)).collect();
        Ok(reply(200, format!("{{\"tenants\":[{}],\"next_after\":{next}}}", items.join(","))))
    }

    fn export(&self, name: &str) -> Result<Response, String> {
        let Some(tenants) = self.served.tenant_files() else {
            return Ok(refused(404, "this server keeps no tenant databases"));
        };
        if self.tenant(name)?.is_none() {
            return Ok(refused(404, "no such tenant"));
        }
        let Some(AttachedDatabase::Other(store)) = self.served.attach(name) else {
            return Ok(refused(409, "the tenant is suspended or deleted; resume it to export it"));
        };
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis());
        let path = tenants.path_of(name).with_file_name(format!("{name}-export-{stamp}.eggdb"));
        let report = store.0.backup_to(&path).map_err(|error| error.to_string())?;
        Ok(reply(
            201,
            format!("{{\"export\":{},\"entries\":{}}}", json_string(&path.display().to_string()), report.entries),
        ))
    }

    fn set_quota(&self, name: &str, body: &str) -> Result<Response, String> {
        if self.tenant(name)?.is_none() {
            return Ok(refused(404, "no such tenant"));
        }
        let quota = number_field(body, "quota_bytes").map_or_else(|| "NULL".to_owned(), |bytes| bytes.max(0).to_string());
        rows(self.registry, &format!("UPDATE eggdb_tenants.registry SET quota_bytes = {quota}, changed_at = now() WHERE name = {}", literal(name)))?;
        Ok(match self.tenant(name)? {
            Some(row) => reply(200, self.row_json(&row)),
            None => refused(404, "no such tenant"),
        })
    }

    /// Answers one request of the tenant API.
    pub(super) fn answer(&self, request: &Request) -> Response {
        let given = request.header("authorization").and_then(|value| value.strip_prefix("Bearer ")).unwrap_or("");
        if !eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(given.trim().as_bytes()), &eggdb_crypto::sha256(self.password.as_bytes())) {
            return refused(401, "send the server's password as Authorization: Bearer <password>").with_header("WWW-Authenticate", "Bearer");
        }
        let body = String::from_utf8_lossy(&request.body);
        let parts: Vec<&str> = request.path.trim_matches('/').split('/').collect();
        let outcome = match (request.method.as_str(), parts.as_slice()) {
            ("POST", ["tenants"]) => self.create(&body),
            ("GET", ["tenants"]) => self.list(request.query.as_deref()),
            ("GET", ["tenants", name]) => self.tenant(name).map(|row| match row {
                Some(row) => reply(200, self.row_json(&row)),
                None => refused(404, "no such tenant"),
            }),
            ("POST", ["tenants", name, "suspend"]) => self.set_state(name, "suspended"),
            ("POST", ["tenants", name, "resume"]) => self.set_state(name, "active"),
            ("PUT", ["tenants", name, "quota"]) => self.set_quota(name, &body),
            ("POST", ["tenants", name, "export"]) => self.export(name),
            ("DELETE", ["tenants", name]) => self.set_state(name, "deleted"),
            _ => Ok(refused(404, "the tenant API is /tenants and /tenants/<name>[/suspend|/resume|/quota|/export]")),
        };
        outcome.unwrap_or_else(|message| refused(500, &message))
    }
}

/// Every minute until `stop`, removes the databases of tenants deleted more than
/// [`DELETED_KEPT_DAYS`] ago, and marks tenants over their quota (D1113).
pub(super) fn keep_tenants(registry: &impl KeyValueStore, served: &ServedDirectory<'_>, stop: &(dyn Fn() -> bool + Sync)) {
    let mut waited = PURGE_EVERY;
    while !stop() {
        if waited >= PURGE_EVERY {
            waited = Duration::ZERO;
            let expired = rows(
                registry,
                &format!("SELECT name FROM eggdb_tenants.registry WHERE state = 'deleted' AND deleted_at < now() - interval '{DELETED_KEPT_DAYS} days'"),
            )
            .unwrap_or_default();
            for name in expired.into_iter().filter_map(|row| row.into_iter().next().flatten()) {
                if let Some(tenants) = served.tenant_files() {
                    tenants.set_refused(&name, false);
                }
                let dropped = served.drop_database(&name);
                if let Some(tenants) = served.tenant_files() {
                    tenants.set_refused(&name, true);
                }
                if dropped.is_ok() {
                    let _ = rows(registry, &format!("DELETE FROM eggdb_tenants.registry WHERE name = {}", literal(&name)));
                }
            }
            if let Some(tenants) = served.tenant_files() {
                let quotas = rows(registry, "SELECT name, quota_bytes, state FROM eggdb_tenants.registry WHERE quota_bytes IS NOT NULL AND state IN ('active', 'over_quota')").unwrap_or_default();
                for row in quotas {
                    let (Some(name), Some(quota)) = (row.first().cloned().flatten(), row.get(1).cloned().flatten().and_then(|quota| quota.parse::<u64>().ok())) else {
                        continue;
                    };
                    let size = std::fs::metadata(tenants.path_of(&name)).map_or(0, |metadata| metadata.len());
                    let state = if size > quota { "over_quota" } else { "active" };
                    if row.get(2).cloned().flatten().as_deref() != Some(state) {
                        let _ = rows(registry, &format!("UPDATE eggdb_tenants.registry SET state = {}, changed_at = now() WHERE name = {}", literal(state), literal(&name)));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        waited += Duration::from_millis(500);
    }
}
