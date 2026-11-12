//! Oracle's DDL and database event triggers: `CREATE TRIGGER ... {BEFORE | AFTER} <events> ON
//! {SCHEMA | DATABASE} <PL/SQL block>`, with the `ora_*` event attribute functions.

use crate::ast::Name;
use crate::types::Value;
use std::cell::RefCell;

/// The DDL events a system trigger may name; `DDL` stands for all of them.
pub(crate) const DDL_EVENTS: [&str; 9] =
    ["create", "alter", "drop", "truncate", "rename", "grant", "revoke", "comment", "ddl"];

/// The database events EggDB fires.
pub(crate) const DATABASE_EVENTS: [&str; 3] = ["logon", "servererror", "logoff"];

/// Database events Oracle has that EggDB does not fire yet.
pub(crate) const UNSUPPORTED_EVENTS: [&str; 9] = [
    "startup",
    "shutdown",
    "suspend",
    "db_role_change",
    "clone",
    "unplug",
    "set",
    "analyze",
    "audit",
];

pub(crate) fn is_system_event(word: &str) -> bool {
    DDL_EVENTS.contains(&word)
        || DATABASE_EVENTS.contains(&word)
        || UNSUPPORTED_EVENTS.contains(&word)
        || matches!(word, "noaudit" | "associate" | "disassociate")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemTiming {
    Before,
    After,
}

impl SystemTiming {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Before => "BEFORE",
            Self::After => "AFTER",
        }
    }
}

/// What a system trigger is defined on: one schema's user, or the whole database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemScope {
    Schema(Option<String>),
    Database,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateSystemTrigger {
    pub(crate) name: Name,
    pub(crate) replace: bool,
    pub(crate) timing: SystemTiming,
    pub(crate) events: Vec<String>,
    pub(crate) scope: SystemScope,
    pub(crate) enabled: bool,
    pub(crate) body: String,
}

pub(crate) const TRIGGERS_TABLE: &str = "eggdb_internal.oracle_system_triggers";

pub(crate) const CREATE_TRIGGERS_TABLE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_system_triggers (name text PRIMARY KEY, timing text NOT NULL, events text NOT NULL, owner text, body text NOT NULL, enabled boolean NOT NULL, created timestamp NOT NULL)";

/// Oracle's event name (`ora_sysevent`) for a statement with the PostgreSQL command tag
/// `tag`: its first word.
pub(crate) fn ddl_event(tag: &str) -> Option<&'static str> {
    let first = tag.split(' ').next().unwrap_or_default();
    DDL_EVENTS
        .iter()
        .copied()
        .find(|event| *event != "ddl" && event.eq_ignore_ascii_case(first))
}

/// Whether a trigger naming `events` fires on `event`.
pub(crate) fn covers(events: &[String], event: &str) -> bool {
    events
        .iter()
        .any(|wanted| wanted == event || (wanted == "ddl" && DDL_EVENTS.contains(&event)))
}

/// What the `ora_*` attribute functions answer inside a firing system trigger.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SystemEvent {
    pub(crate) sysevent: String,
    pub(crate) object_name: Option<String>,
    pub(crate) object_owner: Option<String>,
    pub(crate) object_type: Option<String>,
    pub(crate) login_user: String,
    pub(crate) server_error: Option<(i64, String)>,
}

thread_local! {
    static CURRENT: RefCell<Vec<SystemEvent>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct Firing;

impl Drop for Firing {
    fn drop(&mut self) {
        CURRENT.with(|current| current.borrow_mut().pop());
    }
}

pub(crate) fn enter(event: SystemEvent) -> Firing {
    CURRENT.with(|current| current.borrow_mut().push(event));
    Firing
}

/// The internal function the bare `ora_*` attribute names parse to, with the attribute's name
/// as its argument.
pub(crate) const ATTRIBUTE_FUNCTION: &str = "eggdb_oracle_event_attribute";

/// The attribute names read without parentheses.
pub(crate) const ATTRIBUTES: [&str; 6] = [
    "ora_sysevent",
    "ora_dict_obj_name",
    "ora_dict_obj_owner",
    "ora_dict_obj_type",
    "ora_login_user",
    "ora_database_name",
];

/// The functions with an argument: `ora_server_error(position)` and
/// `ora_server_error_msg(position)`.
pub(crate) const SERVER_ERROR: &str = "ora_server_error";
pub(crate) const SERVER_ERROR_MESSAGE: &str = "ora_server_error_msg";

fn text(value: Option<&String>) -> Value {
    value.map_or(Value::Null, |text| Value::Text(text.clone()))
}

/// The value of an event attribute; `NULL` outside a firing system trigger, as Oracle's are
/// outside their events.
pub(crate) fn attribute(name: &str, values: &[Value]) -> Value {
    CURRENT.with(|current| {
        let current = current.borrow();
        let Some(event) = current.last() else {
            return Value::Null;
        };
        let first_error = values
            .first()
            .and_then(Value::as_integer)
            .filter(|position| *position == 1)
            .and(event.server_error.as_ref());
        match name {
            "ora_sysevent" => Value::Text(event.sysevent.clone()),
            "ora_dict_obj_name" => text(event.object_name.as_ref()),
            "ora_dict_obj_owner" => text(event.object_owner.as_ref()),
            "ora_dict_obj_type" => text(event.object_type.as_ref()),
            "ora_login_user" => Value::Text(event.login_user.clone()),
            "ora_database_name" => Value::Text("EGGDB".to_owned()),
            SERVER_ERROR => first_error.map_or(Value::Integer(0), |(code, _)| {
                Value::Integer(i32::try_from(*code).unwrap_or(0))
            }),
            SERVER_ERROR_MESSAGE => first_error.map_or(Value::Null, |(_, message)| Value::Text(message.clone())),
            _ => Value::Null,
        }
    })
}

/// Oracle's `ora_dict_obj_type` for EggDB's object type names.
pub(crate) fn object_type(object_type: &str) -> Option<&'static str> {
    Some(match object_type {
        "table" => "TABLE",
        "view" | "materialized view" => "VIEW",
        "index" => "INDEX",
        "sequence" => "SEQUENCE",
        "function" => "FUNCTION",
        "procedure" => "PROCEDURE",
        "trigger" => "TRIGGER",
        "schema" => "USER",
        "type" | "domain" => "TYPE",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_covers_the_ddl_events_only() {
        let all = vec!["ddl".to_owned()];
        assert!(covers(&all, "create"));
        assert!(covers(&all, "truncate"));
        assert!(!covers(&all, "logon"));
        assert!(covers(&["drop".to_owned()], "drop"));
        assert_eq!(ddl_event("CREATE TABLE"), Some("create"));
        assert_eq!(ddl_event("REFRESH MATERIALIZED VIEW"), None);
    }

    #[test]
    fn attributes_answer_inside_a_firing_only() {
        assert_eq!(attribute("ora_sysevent", &[]), Value::Null);
        let firing = enter(SystemEvent {
            sysevent: "DROP".to_owned(),
            object_name: Some("EMP".to_owned()),
            object_owner: Some("SCOTT".to_owned()),
            object_type: Some("TABLE".to_owned()),
            login_user: "SCOTT".to_owned(),
            server_error: Some((942, "ORA-00942: table or view does not exist".to_owned())),
        });
        assert_eq!(attribute("ora_dict_obj_name", &[]), Value::Text("EMP".to_owned()));
        assert_eq!(attribute(SERVER_ERROR, &[Value::Integer(1)]), Value::Integer(942));
        assert_eq!(attribute(SERVER_ERROR, &[Value::Integer(2)]), Value::Integer(0));
        drop(firing);
        assert_eq!(attribute("ora_dict_obj_name", &[]), Value::Null);
    }

    fn run(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut found = String::new();
        session
            .execute(sql, &mut |result| found = crate::tests::render(&result))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        found
    }

    #[test]
    fn a_schema_trigger_logs_ddl_and_can_refuse_it() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::with_dialect(&store, crate::Dialect::Oracle);
        run(&mut session, "CREATE TABLE ddl_log (event VARCHAR2(30), object_name VARCHAR2(128), object_type VARCHAR2(30))");
        run(
            &mut session,
            "CREATE OR REPLACE TRIGGER log_ddl AFTER CREATE OR ALTER ON SCHEMA BEGIN INSERT INTO ddl_log VALUES (ora_sysevent, ora_dict_obj_name, ora_dict_obj_type); END;",
        );
        run(&mut session, "CREATE TABLE emp (id NUMBER PRIMARY KEY)");
        let logged = run(&mut session, "SELECT event, object_name, object_type FROM ddl_log");
        assert!(logged.contains("CREATE"), "{logged}");
        assert!(logged.contains("EMP"), "{logged}");
        assert!(logged.contains("TABLE"), "{logged}");
        run(
            &mut session,
            "CREATE TRIGGER no_drops BEFORE DROP ON SCHEMA BEGIN RAISE_APPLICATION_ERROR(-20001, 'drops are not allowed'); END;",
        );
        let refused = session.execute("DROP TABLE emp", &mut |_| {});
        assert!(refused.is_err());
        run(&mut session, "SELECT count(*) FROM emp");
        run(&mut session, "ALTER TRIGGER no_drops DISABLE");
        run(&mut session, "DROP TABLE emp");
        run(&mut session, "DROP TRIGGER no_drops");
        run(&mut session, "DROP TRIGGER log_ddl");
        let unsupported = session.execute("CREATE TRIGGER at_start AFTER STARTUP ON DATABASE BEGIN NULL; END;", &mut |_| {});
        assert!(unsupported.is_err());
    }

    #[test]
    fn logoff_triggers_run_as_a_session_ends() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::with_dialect(&store, crate::Dialect::Oracle);
        run(&mut session, "CREATE TABLE sign_outs (event VARCHAR2(30))");
        run(&mut session, "CREATE TRIGGER note_logoff BEFORE LOGOFF ON DATABASE BEGIN INSERT INTO sign_outs VALUES (ora_sysevent); END;");
        assert!(session.execute("CREATE TRIGGER late AFTER LOGOFF ON DATABASE BEGIN NULL; END;", &mut |_| {}).is_err());
        session.close().unwrap();
        let mut next = crate::Session::with_dialect(&store, crate::Dialect::Oracle);
        let logged = run(&mut next, "SELECT event FROM sign_outs");
        assert!(logged.contains("LOGOFF"), "{logged}");
    }
}
