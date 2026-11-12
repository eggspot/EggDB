use crate::json::{number, path, text};
use eggdb_sql::json::Node;

pub(crate) const DEFAULT_IMAGE: &str = "ghcr.io/eggspot/eggdb:latest";
pub(crate) const DEFAULT_STORAGE: &str = "10Gi";
pub(crate) const DEFAULT_PORT: i64 = 5432;

/// What an `EggDB` resource asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EggDbSpec {
    pub(crate) name: String,
    pub(crate) namespace: String,
    pub(crate) uid: String,
    pub(crate) image: String,
    pub(crate) storage: String,
    pub(crate) storage_class: Option<String>,
    pub(crate) port: i64,
    pub(crate) mysql_port: Option<i64>,
    pub(crate) password_secret: String,
    pub(crate) password_key: String,
    pub(crate) tls_secret: Option<String>,
    pub(crate) cpu: Option<String>,
    pub(crate) memory: Option<String>,
}

impl EggDbSpec {
    pub(crate) fn read(resource: &Node) -> Result<Self, String> {
        let name = text(resource, &["metadata", "name"]).ok_or("the resource has no name")?;
        let namespace = text(resource, &["metadata", "namespace"]).ok_or("the resource has no namespace")?;
        let uid = text(resource, &["metadata", "uid"]).ok_or("the resource has no uid")?;
        let spec = path(resource, &["spec"]).ok_or_else(|| format!("{name} has no spec"))?;
        let password_secret = text(spec, &["passwordSecret", "name"])
            .ok_or_else(|| format!("{name}: spec.passwordSecret.name is required"))?;
        let port = number(spec, &["port"]).unwrap_or(DEFAULT_PORT);
        if !(1..=65535).contains(&port) {
            return Err(format!("{name}: spec.port must be between 1 and 65535"));
        }
        Ok(Self {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
            uid: uid.to_owned(),
            image: text(spec, &["image"]).unwrap_or(DEFAULT_IMAGE).to_owned(),
            storage: text(spec, &["storage", "size"]).unwrap_or(DEFAULT_STORAGE).to_owned(),
            storage_class: text(spec, &["storage", "className"]).map(str::to_owned),
            port,
            mysql_port: number(spec, &["mysqlPort"]).filter(|port| (1..=65535).contains(port)),
            password_secret: password_secret.to_owned(),
            password_key: text(spec, &["passwordSecret", "key"]).unwrap_or("password").to_owned(),
            tls_secret: text(spec, &["tlsSecret", "name"]).map(str::to_owned),
            cpu: text(spec, &["resources", "cpu"]).map(str::to_owned),
            memory: text(spec, &["resources", "memory"]).map(str::to_owned),
        })
    }
}
