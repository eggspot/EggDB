//! SQL Server's DDL triggers: `CREATE TRIGGER ... ON DATABASE | ALL SERVER FOR <events> AS
//! <batch>`, fired after the DDL statements they name, with `EVENTDATA()` describing the
//! statement; and logon triggers, `ON ALL SERVER FOR LOGON`, fired when a connection signs in.

use crate::ast::{Name, Statement};
use crate::roles::RoleStatement;
use crate::served_databases::DatabaseStatement;
use crate::types::Value;
use std::cell::RefCell;

/// Where a DDL trigger is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DdlTriggerScope {
    Database,
    Server,
}

impl DdlTriggerScope {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Server => "server",
        }
    }
}

/// A DDL trigger statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DdlTriggerCommand {
    Create {
        name: Name,
        scope: DdlTriggerScope,
        events: Vec<Name>,
        body: String,
        replace: bool,
        alter: bool,
    },
    Drop {
        names: Vec<Name>,
        scope: DdlTriggerScope,
        if_exists: bool,
    },
    Enable {
        names: Option<Vec<Name>>,
        scope: DdlTriggerScope,
        enable: bool,
    },
}

impl DdlTriggerCommand {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::Create { alter: true, .. } => "ALTER TRIGGER",
            Self::Create { .. } => "CREATE TRIGGER",
            Self::Drop { .. } => "DROP TRIGGER",
            Self::Enable { enable: true, .. } => "ENABLE TRIGGER",
            Self::Enable { .. } => "DISABLE TRIGGER",
        }
    }
}

pub(crate) const TRIGGERS_TABLE: &str = "eggdb_internal.sqlserver_ddl_triggers";

pub(crate) const CREATE_TRIGGERS_TABLE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_ddl_triggers (name text PRIMARY KEY, scope text NOT NULL, events text NOT NULL, body text NOT NULL, enabled boolean NOT NULL, created timestamp NOT NULL)";

/// The event types EggDB fires, each with the groups that contain it.
const EVENT_GROUPS: [(&str, &[&str]); 42] = [
    ("CREATE_LOGIN", &["DDL_LOGIN_EVENTS", "DDL_SERVER_SECURITY_EVENTS"]),
    ("ALTER_LOGIN", &["DDL_LOGIN_EVENTS", "DDL_SERVER_SECURITY_EVENTS"]),
    ("DROP_LOGIN", &["DDL_LOGIN_EVENTS", "DDL_SERVER_SECURITY_EVENTS"]),
    ("CREATE_DATABASE", &["DDL_DATABASE_EVENTS"]),
    ("ALTER_DATABASE", &["DDL_DATABASE_EVENTS"]),
    ("DROP_DATABASE", &["DDL_DATABASE_EVENTS"]),
    ("CREATE_USER", &["DDL_USER_EVENTS", "DDL_DATABASE_SECURITY_EVENTS"]),
    ("ALTER_USER", &["DDL_USER_EVENTS", "DDL_DATABASE_SECURITY_EVENTS"]),
    ("DROP_USER", &["DDL_USER_EVENTS", "DDL_DATABASE_SECURITY_EVENTS"]),
    ("CREATE_ROLE", &["DDL_ROLE_EVENTS", "DDL_DATABASE_SECURITY_EVENTS"]),
    ("ALTER_ROLE", &["DDL_ROLE_EVENTS", "DDL_DATABASE_SECURITY_EVENTS"]),
    ("DROP_ROLE", &["DDL_ROLE_EVENTS", "DDL_DATABASE_SECURITY_EVENTS"]),
    ("CREATE_TABLE", &["DDL_TABLE_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("ALTER_TABLE", &["DDL_TABLE_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("DROP_TABLE", &["DDL_TABLE_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("CREATE_VIEW", &["DDL_VIEW_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("ALTER_VIEW", &["DDL_VIEW_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("DROP_VIEW", &["DDL_VIEW_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("CREATE_INDEX", &["DDL_INDEX_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("ALTER_INDEX", &["DDL_INDEX_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("DROP_INDEX", &["DDL_INDEX_EVENTS", "DDL_TABLE_VIEW_EVENTS"]),
    ("CREATE_PROCEDURE", &["DDL_PROCEDURE_EVENTS"]),
    ("ALTER_PROCEDURE", &["DDL_PROCEDURE_EVENTS"]),
    ("DROP_PROCEDURE", &["DDL_PROCEDURE_EVENTS"]),
    ("CREATE_FUNCTION", &["DDL_FUNCTION_EVENTS"]),
    ("ALTER_FUNCTION", &["DDL_FUNCTION_EVENTS"]),
    ("DROP_FUNCTION", &["DDL_FUNCTION_EVENTS"]),
    ("CREATE_TRIGGER", &["DDL_TRIGGER_EVENTS"]),
    ("ALTER_TRIGGER", &["DDL_TRIGGER_EVENTS"]),
    ("DROP_TRIGGER", &["DDL_TRIGGER_EVENTS"]),
    ("CREATE_SCHEMA", &["DDL_SCHEMA_EVENTS"]),
    ("ALTER_SCHEMA", &["DDL_SCHEMA_EVENTS"]),
    ("DROP_SCHEMA", &["DDL_SCHEMA_EVENTS"]),
    ("CREATE_SEQUENCE", &["DDL_SEQUENCE_EVENTS"]),
    ("ALTER_SEQUENCE", &["DDL_SEQUENCE_EVENTS"]),
    ("DROP_SEQUENCE", &["DDL_SEQUENCE_EVENTS"]),
    ("CREATE_TYPE", &["DDL_TYPE_EVENTS"]),
    ("DROP_TYPE", &["DDL_TYPE_EVENTS"]),
    ("CREATE_SECURITY_POLICY", &["DDL_SECURITY_POLICY_EVENTS"]),
    ("ALTER_SECURITY_POLICY", &["DDL_SECURITY_POLICY_EVENTS"]),
    ("DROP_SECURITY_POLICY", &["DDL_SECURITY_POLICY_EVENTS"]),
    ("GRANT_DATABASE", &["DDL_GDR_DATABASE_EVENTS"]),
];

const LEVEL_GROUPS: [&str; 3] = ["DDL_DATABASE_LEVEL_EVENTS", "DDL_SERVER_LEVEL_EVENTS", "DDL_EVENTS"];

/// The events only a server-scoped trigger (`ON ALL SERVER`) sees, and the groups made only of
/// them; `ALTER_DATABASE` fires at both levels, as in SQL Server.
const SERVER_ONLY_EVENTS: [&str; 5] = ["CREATE_LOGIN", "ALTER_LOGIN", "DROP_LOGIN", "CREATE_DATABASE", "DROP_DATABASE"];
const SERVER_ONLY_GROUPS: [&str; 3] = ["DDL_SERVER_LEVEL_EVENTS", "DDL_LOGIN_EVENTS", "DDL_SERVER_SECURITY_EVENTS"];

/// Whether a trigger on a database may not name `name`: a server-only event or group.
pub(crate) fn needs_server_scope(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    SERVER_ONLY_EVENTS.contains(&name.as_str())
        || SERVER_ONLY_GROUPS.contains(&name.as_str())
        || name == "DDL_DATABASE_EVENTS"
}

/// Whether only server-scoped triggers fire on `event`.
pub(crate) fn is_server_only_event(event: &str) -> bool {
    SERVER_ONLY_EVENTS.contains(&event)
}

/// The command tag of a login, user, role or database statement, which the PostgreSQL event
/// tags leave out: SQL Server's `CREATE LOGIN`, `CREATE USER` and `CREATE ROLE` all parse to a
/// role statement, so the statement's own text tells them apart.
pub(crate) fn security_tag(statement: &Statement, command_text: &str) -> Option<&'static str> {
    let verb = match statement {
        Statement::Database(DatabaseStatement::Create { .. }) => return Some("CREATE DATABASE"),
        Statement::Database(DatabaseStatement::Drop { .. }) => return Some("DROP DATABASE"),
        Statement::Role(RoleStatement::Create { .. }) => 0,
        Statement::Role(RoleStatement::Alter { .. }) => 1,
        Statement::Role(RoleStatement::Drop { .. }) => 2,
        _ => return None,
    };
    let kind = command_text
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_ascii_uppercase();
    let tags = match kind.as_str() {
        "LOGIN" => ["CREATE LOGIN", "ALTER LOGIN", "DROP LOGIN"],
        "USER" => ["CREATE USER", "ALTER USER", "DROP USER"],
        _ => ["CREATE ROLE", "ALTER ROLE", "DROP ROLE"],
    };
    Some(tags[verb])
}

/// The event of a logon trigger, which only a server-scoped trigger may name, alone.
pub(crate) const LOGON_EVENT: &str = "LOGON";

/// Whether `name` is an event type or event group a DDL trigger may name.
pub(crate) fn is_known_event(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name == LOGON_EVENT
        || LEVEL_GROUPS.contains(&name.as_str())
        || name == "REVOKE_DATABASE"
        || name == "DENY_DATABASE"
        || EVENT_GROUPS
            .iter()
            .any(|(event, groups)| *event == name || groups.contains(&name.as_str()))
}

/// Whether a trigger naming `wanted` (an event type or group) fires on `event`.
pub(crate) fn event_matches(wanted: &str, event: &str) -> bool {
    let wanted = wanted.to_ascii_uppercase();
    if wanted == event {
        return true;
    }
    if wanted == LOGON_EVENT || event == LOGON_EVENT {
        return false;
    }
    if wanted == "DDL_DATABASE_LEVEL_EVENTS" {
        return !is_server_only_event(event);
    }
    if LEVEL_GROUPS.contains(&wanted.as_str()) {
        return true;
    }
    if wanted == "DDL_GDR_DATABASE_EVENTS" {
        return matches!(event, "GRANT_DATABASE" | "REVOKE_DATABASE" | "DENY_DATABASE");
    }
    EVENT_GROUPS
        .iter()
        .any(|(found, groups)| *found == event && groups.contains(&wanted.as_str()))
}

/// SQL Server's event type for a statement with the PostgreSQL command tag `tag`.
pub(crate) fn event_type(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "CREATE TABLE" | "CREATE TABLE AS" => "CREATE_TABLE",
        "REVOKE" => "REVOKE_DATABASE",
        "GRANT" => "GRANT_DATABASE",
        "CREATE DOMAIN" => "CREATE_TYPE",
        "DROP DOMAIN" => "DROP_TYPE",
        "CREATE MATERIALIZED VIEW" => "CREATE_VIEW",
        "DROP MATERIALIZED VIEW" => "DROP_VIEW",
        other => {
            let wanted = other.replace(' ', "_");
            return EVENT_GROUPS
                .iter()
                .map(|(event, _)| *event)
                .find(|event| *event == wanted);
        }
    })
}

/// What `EVENTDATA()` describes about the statement that fired a DDL trigger.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventData {
    pub(crate) event_type: &'static str,
    pub(crate) post_time: String,
    pub(crate) spid: u32,
    pub(crate) login: String,
    pub(crate) database: String,
    pub(crate) schema: Option<String>,
    pub(crate) object: Option<String>,
    pub(crate) object_type: Option<&'static str>,
    pub(crate) command_text: String,
}

fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

impl EventData {
    /// The `EVENT_INSTANCE` document SQL Server's `EVENTDATA()` returns.
    pub(crate) fn to_xml(&self) -> String {
        let mut xml = String::from("<EVENT_INSTANCE>");
        let mut element = |name: &str, value: &str| {
            xml.push_str(&format!("<{name}>{}</{name}>", escaped(value)));
        };
        element("EventType", self.event_type);
        element("PostTime", &self.post_time);
        element("SPID", &self.spid.to_string());
        element("ServerName", "eggdb");
        element("LoginName", &self.login);
        element("UserName", crate::sqlserver_catalog::DBO);
        element("DatabaseName", &self.database);
        if let Some(schema) = &self.schema {
            element("SchemaName", schema);
        }
        if let Some(object) = &self.object {
            element("ObjectName", object);
        }
        if let Some(object_type) = self.object_type {
            element("ObjectType", object_type);
        }
        xml.push_str(
            "<TSQLCommand><SetOptions ANSI_NULLS=\"ON\" ANSI_NULL_DEFAULT=\"ON\" ANSI_PADDING=\"ON\" QUOTED_IDENTIFIER=\"ON\" ENCRYPTED=\"FALSE\"/><CommandText>",
        );
        xml.push_str(&escaped(&self.command_text));
        xml.push_str("</CommandText></TSQLCommand></EVENT_INSTANCE>");
        xml
    }
}

/// SQL Server's `ObjectType` for EggDB's object type names.
pub(crate) fn object_type(object_type: &str) -> Option<&'static str> {
    Some(match object_type {
        "table" => "TABLE",
        "view" | "materialized view" => "VIEW",
        "index" => "INDEX",
        "sequence" => "SEQUENCE",
        "function" => "FUNCTION",
        "procedure" => "PROCEDURE",
        "trigger" => "TRIGGER",
        "schema" => "SCHEMA",
        "type" | "domain" => "TYPE",
        _ => return None,
    })
}

thread_local! {
    static CURRENT: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Keeps `EVENTDATA()` answering for one firing, until it is dropped.
pub(crate) struct Firing;

impl Drop for Firing {
    fn drop(&mut self) {
        CURRENT.with(|current| current.borrow_mut().pop());
    }
}

pub(crate) fn enter(event: &EventData) -> Firing {
    CURRENT.with(|current| current.borrow_mut().push(event.to_xml()));
    Firing
}

/// The internal function `EVENTDATA()` parses to.
pub(crate) const EVENT_DATA_FUNCTION: &str = "eggdb_sqlserver_eventdata";

/// `EVENTDATA()`: the firing statement's document inside a DDL trigger, `NULL` elsewhere, as
/// in SQL Server.
pub(crate) fn event_data() -> Value {
    CURRENT.with(|current| {
        current
            .borrow()
            .last()
            .map_or(Value::Null, |xml| Value::Text(xml.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_contain_their_events() {
        assert!(event_matches("DDL_TABLE_EVENTS", "CREATE_TABLE"));
        assert!(event_matches("ddl_table_view_events", "CREATE_INDEX"));
        assert!(event_matches("DDL_DATABASE_LEVEL_EVENTS", "DROP_PROCEDURE"));
        assert!(!event_matches("DDL_VIEW_EVENTS", "CREATE_TABLE"));
        assert!(event_matches("drop_table", "DROP_TABLE"));
        assert!(is_known_event("DDL_INDEX_EVENTS"));
        assert!(!is_known_event("CREATE_UNICORN"));
        assert_eq!(event_type("CREATE PROCEDURE"), Some("CREATE_PROCEDURE"));
        assert_eq!(event_type("COMMENT"), None);
    }

    #[test]
    fn event_data_is_an_event_instance_document() {
        let data = EventData {
            event_type: "CREATE_TABLE",
            post_time: "2026-11-12T10:00:00".to_owned(),
            spid: 51,
            login: "sa".to_owned(),
            database: "master".to_owned(),
            schema: Some("dbo".to_owned()),
            object: Some("T<1>".to_owned()),
            object_type: Some("TABLE"),
            command_text: "CREATE TABLE \"T<1>\" (a int)".to_owned(),
        };
        let xml = data.to_xml();
        assert!(xml.starts_with("<EVENT_INSTANCE><EventType>CREATE_TABLE</EventType>"), "{xml}");
        assert!(xml.contains("<ObjectName>T&lt;1&gt;</ObjectName>"), "{xml}");
        assert_eq!(event_data(), Value::Null);
        let firing = enter(&data);
        assert!(matches!(event_data(), Value::Text(text) if text == xml));
        drop(firing);
        assert_eq!(event_data(), Value::Null);
    }

    fn rows(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut found = String::new();
        session
            .execute(sql, &mut |result| found = crate::tests::render(&result))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        found
    }

    #[test]
    fn a_database_trigger_logs_and_can_refuse_ddl() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::with_dialect(&store, crate::Dialect::SqlServer);
        for sql in [
            "CREATE TABLE ddl_log (event_type nvarchar(100), object_name nvarchar(100))",
            "CREATE TRIGGER log_ddl ON DATABASE FOR DDL_TABLE_EVENTS AS \
             DECLARE @data xml = EVENTDATA(); \
             INSERT INTO ddl_log VALUES (@data.value('(/EVENT_INSTANCE/EventType)[1]', 'nvarchar(100)'), @data.value('(/EVENT_INSTANCE/ObjectName)[1]', 'nvarchar(100)'))",
            "CREATE TABLE orders (id int PRIMARY KEY)",
        ] {
            rows(&mut session, sql);
        }
        let logged = rows(&mut session, "SELECT event_type, object_name FROM ddl_log");
        assert!(logged.contains("CREATE_TABLE"), "{logged}");
        assert!(logged.contains("orders"), "{logged}");
        rows(&mut session, "DISABLE TRIGGER log_ddl ON DATABASE");
        rows(&mut session, "CREATE TABLE quiet (id int)");
        let counted = rows(&mut session, "SELECT count(*) AS n FROM ddl_log");
        assert!(counted.contains("n\n1"), "{counted}");
        rows(&mut session, "ENABLE TRIGGER log_ddl ON DATABASE");
        rows(&mut session, "CREATE TRIGGER no_drops ON DATABASE FOR DROP_TABLE AS PRINT 'no'; ROLLBACK");
        let refused = session.execute("DROP TABLE orders", &mut |_| {});
        assert!(refused.is_err());
        let kept = rows(&mut session, "SELECT count(*) AS n FROM orders");
        assert!(kept.contains("n\n0"), "{kept}");
        rows(&mut session, "DROP TRIGGER no_drops, log_ddl ON DATABASE");
        rows(&mut session, "DROP TRIGGER IF EXISTS no_drops ON DATABASE");
        rows(&mut session, "DROP TABLE orders");
        let unknown = session.execute("CREATE TRIGGER bad ON DATABASE FOR CREATE_UNICORN AS PRINT 'x'", &mut |_| {});
        assert!(unknown.is_err());
    }

    #[test]
    fn server_triggers_see_logins_and_database_triggers_do_not() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::with_dialect(&store, crate::Dialect::SqlServer);
        rows(&mut session, "CREATE TABLE security_log (event_type nvarchar(100), scope nvarchar(10))");
        let misplaced = session.execute("CREATE TRIGGER wrong ON DATABASE FOR CREATE_LOGIN AS PRINT 'x'", &mut |_| {});
        assert_eq!(misplaced.unwrap_err().vendor_code(), Some(1098));
        rows(
            &mut session,
            "CREATE TRIGGER log_logins ON ALL SERVER FOR DDL_LOGIN_EVENTS AS \
             INSERT INTO security_log VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'nvarchar(100)'), 'server')",
        );
        rows(
            &mut session,
            "CREATE TRIGGER log_database ON DATABASE FOR DDL_DATABASE_LEVEL_EVENTS AS \
             INSERT INTO security_log VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'nvarchar(100)'), 'database')",
        );
        rows(&mut session, "CREATE LOGIN auditor WITH PASSWORD = 'Str0ng!pass'");
        rows(&mut session, "CREATE ROLE reporting");
        let logged = rows(&mut session, "SELECT event_type, scope FROM security_log ORDER BY event_type");
        assert!(logged.contains("CREATE_LOGIN\tserver") || logged.contains("CREATE_LOGIN"), "{logged}");
        assert!(logged.contains("CREATE_ROLE"), "{logged}");
        assert!(!logged.contains("CREATE_LOGIN\tdatabase"), "{logged}");
        assert_eq!(logged.matches("CREATE_LOGIN").count(), 1, "{logged}");
    }

    #[test]
    fn logon_triggers_log_and_can_refuse_sign_ins() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::with_dialect(&store, crate::Dialect::SqlServer);
        rows(&mut session, "CREATE TABLE logons (event_type nvarchar(100))");
        let misplaced = session.execute("CREATE TRIGGER wrong ON DATABASE FOR LOGON AS PRINT 'x'", &mut |_| {});
        assert_eq!(misplaced.unwrap_err().vendor_code(), Some(1098));
        rows(
            &mut session,
            "CREATE TRIGGER log_logons ON ALL SERVER FOR LOGON AS \
             INSERT INTO logons VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'nvarchar(100)'))",
        );
        rows(&mut session, "CREATE TABLE plain (id int)");
        let mut arriving = crate::Session::with_dialect(&store, crate::Dialect::SqlServer);
        arriving.fire_login_event_triggers().expect("the logon triggers run");
        let logged = rows(&mut arriving, "SELECT event_type FROM logons");
        assert!(logged.contains("LOGON"), "{logged}");
        assert!(logged.matches("LOGON").count() == 1, "the CREATE TABLE fired no logon trigger: {logged}");
        rows(&mut arriving, "CREATE TRIGGER no_logons ON ALL SERVER FOR LOGON AS ROLLBACK");
        let mut refused = crate::Session::with_dialect(&store, crate::Dialect::SqlServer);
        assert!(refused.fire_login_event_triggers().is_err());
    }
}
