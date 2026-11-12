use eggdb_http::{ClientResponse, HttpTarget, request};
use eggdb_sql::json::{Node, parse_document};
use std::time::Duration;

const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const TIMEOUT: Duration = Duration::from_secs(30);
const MOST_RESPONSE_BYTES: usize = 32 << 20;
const FIELD_MANAGER: &str = "eggdb-operator";

/// The Kubernetes API server, reached from inside the cluster with the pod's service account.
#[derive(Debug, Clone)]
pub(crate) struct Kubernetes {
    target: HttpTarget,
    token: String,
    pub(crate) namespace: String,
}

impl Kubernetes {
    pub(crate) fn in_cluster() -> Result<Self, String> {
        let read = |name: &str| {
            std::fs::read_to_string(format!("{SERVICE_ACCOUNT}/{name}"))
                .map(|text| text.trim().to_owned())
                .map_err(|error| format!("cannot read the service account's {name}: {error}"))
        };
        let token = read("token")?;
        let namespace = std::env::var("WATCH_NAMESPACE")
            .ok()
            .filter(|namespace| !namespace.is_empty())
            .map_or_else(|| read("namespace"), Ok)?;
        let host = std::env::var("KUBERNETES_SERVICE_HOST").unwrap_or_else(|_| "kubernetes.default.svc".to_owned());
        let port = std::env::var("KUBERNETES_SERVICE_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(443);
        Ok(Self {
            target: HttpTarget {
                host: if host.contains(':') { format!("[{host}]") } else { host },
                port,
                path: "/".to_owned(),
                tls: true,
            },
            token,
            namespace,
        })
    }

    fn call(&self, method: &str, path: &str, content_type: Option<&str>, body: &[u8]) -> Result<ClientResponse, String> {
        let mut headers = vec![
            ("Authorization".to_owned(), format!("Bearer {}", self.token)),
            ("Accept".to_owned(), "application/json".to_owned()),
        ];
        if let Some(content_type) = content_type {
            headers.push(("Content-Type".to_owned(), content_type.to_owned()));
        }
        let response = request(&self.target, method, path, &headers, body, TIMEOUT, MOST_RESPONSE_BYTES)
            .map_err(|error| format!("{method} {path}: {error}"))?;
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "{method} {path}: HTTP {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ));
        }
        Ok(response)
    }

    pub(crate) fn get(&self, path: &str) -> Result<Node, String> {
        let response = self.call("GET", path, None, &[])?;
        let text = String::from_utf8(response.body).map_err(|_| format!("GET {path}: the answer is not UTF-8"))?;
        parse_document(&text).map_err(|error| format!("GET {path}: {}", error.message))
    }

    pub(crate) fn apply(&self, path: &str, manifest: &str) -> Result<(), String> {
        let path = format!("{path}?fieldManager={FIELD_MANAGER}&force=true");
        self.call("PATCH", &path, Some("application/apply-patch+yaml"), manifest.as_bytes())
            .map(drop)
    }

    pub(crate) fn merge_status(&self, path: &str, status: &str) -> Result<(), String> {
        let body = format!("{{\"status\": {status}}}");
        self.call("PATCH", &format!("{path}/status"), Some("application/merge-patch+json"), body.as_bytes())
            .map(drop)
    }
}
