use crate::commands::{Shared, document, ok, text};
use crate::server::Session;
use eggdb_auth::scram::ScramVerifier;
use eggdb_auth::scram_sha1::ScramSha1Verifier;
use eggdb_bson::{Bson, Document};
use eggdb_documents::{SortSpec, parse_filter};

const ADMIN: &str = "admin";
const USERS: &str = "system.users";
const SHA256_ITERATIONS: u32 = 15_000;
const SHA1_ITERATIONS: u32 = 10_000;
const SALT_BYTES: usize = 16;
const MOST_ROLES: usize = 1_000;
const BUILT_IN_ROLES: [&str; 16] = [
    "read",
    "readWrite",
    "dbAdmin",
    "dbOwner",
    "userAdmin",
    "clusterAdmin",
    "clusterManager",
    "clusterMonitor",
    "hostManager",
    "backup",
    "restore",
    "readAnyDatabase",
    "readWriteAnyDatabase",
    "userAdminAnyDatabase",
    "dbAdminAnyDatabase",
    "root",
];
const WRITE_COMMANDS: [&str; 12] = [
    "insert",
    "update",
    "delete",
    "findAndModify",
    "findandmodify",
    "create",
    "drop",
    "createIndexes",
    "dropIndexes",
    "renameCollection",
    "collMod",
    "dropDatabase",
];
const DEFINITION_COMMANDS: [&str; 7] = ["create", "drop", "createIndexes", "dropIndexes", "renameCollection", "collMod", "dropDatabase"];
const USER_COMMANDS: [&str; 6] = [
    "createUser",
    "updateUser",
    "dropUser",
    "dropAllUsersFromDatabase",
    "grantRolesToUser",
    "revokeRolesFromUser",
];
const ANY_DATABASE_COMMANDS: [&str; 18] = [
    "hello",
    "isMaster",
    "ismaster",
    "ping",
    "buildInfo",
    "buildinfo",
    "connectionStatus",
    "logout",
    "whatsmyuri",
    "endSessions",
    "killSessions",
    "refreshSessions",
    "getParameter",
    "hostInfo",
    "getCmdLineOpts",
    "serverStatus",
    "listDatabases",
    "getLog",
];

/// A user kept in `admin.system.users`: its roles and its SCRAM verifiers.
pub(crate) struct StoredUser {
    pub(crate) roles: Vec<(String, String)>,
    pub(crate) sha256: Option<ScramVerifier>,
    pub(crate) sha1: Option<ScramSha1Verifier>,
}

fn failure(code: i32, code_name: &str, message: impl Into<String>) -> Document {
    document(vec![
        ("ok", Bson::Double(0.0)),
        ("errmsg", text(&message.into())),
        ("code", Bson::Int32(code)),
        ("codeName", text(code_name)),
    ])
}

fn user_id(database: &str, user: &str) -> String {
    format!("{database}.{user}")
}

fn by_id(id: &str) -> Option<eggdb_documents::Filter> {
    parse_filter(&document(vec![("_id", text(id))])).ok()
}

fn stored_document(shared: &Shared<'_>, database: &str, user: &str) -> Option<Document> {
    let filter = by_id(&user_id(database, user))?;
    shared.store.find(ADMIN, USERS, &filter, &SortSpec::default()).ok()?.into_iter().next()
}

fn roles_of(value: Option<&Bson>, database: &str) -> Result<Vec<(String, String)>, Document> {
    let Some(Bson::Array(items)) = value else {
        return Ok(Vec::new());
    };
    if items.len() > MOST_ROLES {
        return Err(failure(2, "BadValue", format!("a user may hold at most {MOST_ROLES} roles")));
    }
    items
        .iter()
        .map(|item| {
            let (role, db) = match item {
                Bson::String(role) => (role.clone(), database.to_owned()),
                Bson::Document(given) => (
                    given.get_str("role").unwrap_or_default().to_owned(),
                    given.get_str("db").unwrap_or(database).to_owned(),
                ),
                _ => return Err(failure(14, "TypeMismatch", "a role must be a name or a {role, db} document")),
            };
            if !BUILT_IN_ROLES.contains(&role.as_str()) {
                return Err(failure(31, "RoleNotFound", format!("Could not find role: {role}@{db}")));
            }
            Ok((role, db))
        })
        .collect()
}

fn roles_bson(roles: &[(String, String)]) -> Bson {
    Bson::Array(
        roles
            .iter()
            .map(|(role, db)| Bson::Document(document(vec![("role", text(role)), ("db", text(db))])))
            .collect(),
    )
}

fn stored_roles(found: &Document) -> Vec<(String, String)> {
    let database = found.get_str("db").unwrap_or(ADMIN).to_owned();
    roles_of(found.get("roles"), &database).unwrap_or_default()
}

/// The user `user` of `database`, as `admin.system.users` keeps it.
pub(crate) fn stored_user(shared: &Shared<'_>, database: &str, user: &str) -> Option<StoredUser> {
    let found = stored_document(shared, database, user)?;
    let credentials = found.get_document("credentials");
    Some(StoredUser {
        roles: stored_roles(&found),
        sha256: credentials.and_then(|credentials| credentials.get_str("SCRAM-SHA-256")).and_then(ScramVerifier::from_stored),
        sha1: credentials.and_then(|credentials| credentials.get_str("SCRAM-SHA-1")).and_then(ScramSha1Verifier::from_stored),
    })
}

fn credentials(shared: &Shared<'_>, user: &str, password: &str, mechanisms: &[String]) -> Result<Document, Document> {
    let mut salt = [0u8; SALT_BYTES];
    let mut credentials = Document::new();
    let wants = |name: &str| mechanisms.is_empty() || mechanisms.iter().any(|mechanism| mechanism == name);
    if wants("SCRAM-SHA-256") {
        (shared.config.random)(&mut salt).map_err(|_| failure(1, "InternalError", "could not generate a salt"))?;
        let verifier = ScramVerifier::derive(password.as_bytes(), &salt, SHA256_ITERATIONS);
        credentials.insert("SCRAM-SHA-256", text(&verifier.to_stored()));
    }
    if wants("SCRAM-SHA-1") {
        (shared.config.random)(&mut salt).map_err(|_| failure(1, "InternalError", "could not generate a salt"))?;
        let verifier = ScramSha1Verifier::for_mongodb(user, password, &salt, SHA1_ITERATIONS);
        credentials.insert("SCRAM-SHA-1", text(&verifier.to_stored()));
    }
    if credentials.len() == 0 {
        return Err(failure(2, "BadValue", "mechanisms must name SCRAM-SHA-1 or SCRAM-SHA-256"));
    }
    Ok(credentials)
}

fn mechanisms(body: &Document) -> Vec<String> {
    body.get_array("mechanisms")
        .map(|items| items.iter().filter_map(Bson::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn save(shared: &Shared<'_>, database: &str, user: &str, record: Document, replace: bool) -> Result<(), Document> {
    let id = user_id(database, user);
    if replace && let Some(filter) = by_id(&id) {
        shared.store.delete(ADMIN, USERS, &filter, true).map_err(|error| crate::commands::failure(&error))?;
    }
    shared.store.insert(ADMIN, USERS, vec![record], true).map_err(|error| crate::commands::failure(&error))?;
    Ok(())
}

fn user_record(database: &str, user: &str, roles: &[(String, String)], credentials: Document, custom: Option<&Bson>) -> Document {
    let mut record = document(vec![
        ("_id", text(&user_id(database, user))),
        ("user", text(user)),
        ("db", text(database)),
        ("credentials", Bson::Document(credentials)),
        ("roles", roles_bson(roles)),
    ]);
    if let Some(custom) = custom {
        record.insert("customData", custom.clone());
    }
    record
}

fn named(body: &Document, command: &str) -> Option<String> {
    body.get_str(command).map(str::to_owned).filter(|name| !name.is_empty())
}

/// Runs a user command (`createUser`, `updateUser`, `dropUser`, `dropAllUsersFromDatabase`,
/// `usersInfo`, `grantRolesToUser`, `revokeRolesFromUser`, `rolesInfo`); `None` for another.
pub(crate) fn run(shared: &Shared<'_>, command: &str, database: &str, body: &Document) -> Option<Document> {
    Some(match command {
        "createUser" => create_user(shared, database, body),
        "updateUser" => update_user(shared, database, body),
        "dropUser" => {
            let Some(user) = named(body, command) else {
                return Some(failure(2, "BadValue", "dropUser needs a user name"));
            };
            match by_id(&user_id(database, &user)).map(|filter| shared.store.delete(ADMIN, USERS, &filter, true)) {
                Some(Ok(0)) | None => failure(11, "UserNotFound", format!("User '{user}@{database}' not found")),
                Some(Ok(_)) => ok(Document::new()),
                Some(Err(error)) => crate::commands::failure(&error),
            }
        }
        "dropAllUsersFromDatabase" => {
            let filter = parse_filter(&document(vec![("db", text(database))]));
            match filter.map(|filter| shared.store.delete(ADMIN, USERS, &filter, false)) {
                Ok(Ok(removed)) => ok(document(vec![("n", Bson::Int64(removed as i64))])),
                Ok(Err(error)) | Err(error) => crate::commands::failure(&error),
            }
        }
        "grantRolesToUser" | "revokeRolesFromUser" => change_roles(shared, command, database, body),
        "usersInfo" => users_info(shared, database, body),
        "rolesInfo" => ok(document(vec![(
            "roles",
            Bson::Array(
                BUILT_IN_ROLES
                    .iter()
                    .map(|role| {
                        Bson::Document(document(vec![
                            ("role", text(role)),
                            ("db", text(database)),
                            ("isBuiltin", Bson::Boolean(true)),
                            ("roles", Bson::Array(Vec::new())),
                            ("inheritedRoles", Bson::Array(Vec::new())),
                        ]))
                    })
                    .collect(),
            ),
        )])),
        _ => return None,
    })
}

fn create_user(shared: &Shared<'_>, database: &str, body: &Document) -> Document {
    let Some(user) = named(body, "createUser") else {
        return failure(2, "BadValue", "createUser needs a user name");
    };
    if database == "$external" || user == shared.config.user {
        return failure(51_003, "Location51003", format!("User \"{user}@{database}\" already exists"));
    }
    if stored_document(shared, database, &user).is_some() {
        return failure(51_003, "Location51003", format!("User \"{user}@{database}\" already exists"));
    }
    let Some(password) = body.get_str("pwd") else {
        return failure(2, "BadValue", "createUser needs pwd: EggDB has no external users");
    };
    let roles = match roles_of(body.get("roles"), database) {
        Ok(roles) => roles,
        Err(problem) => return problem,
    };
    let credentials = match credentials(shared, &user, password, &mechanisms(body)) {
        Ok(credentials) => credentials,
        Err(problem) => return problem,
    };
    match save(shared, database, &user, user_record(database, &user, &roles, credentials, body.get("customData")), false) {
        Ok(()) => ok(Document::new()),
        Err(problem) => problem,
    }
}

fn update_user(shared: &Shared<'_>, database: &str, body: &Document) -> Document {
    let Some(user) = named(body, "updateUser") else {
        return failure(2, "BadValue", "updateUser needs a user name");
    };
    let Some(found) = stored_document(shared, database, &user) else {
        return failure(11, "UserNotFound", format!("User '{user}@{database}' not found"));
    };
    let roles = match body.get("roles") {
        Some(given) => match roles_of(Some(given), database) {
            Ok(roles) => roles,
            Err(problem) => return problem,
        },
        None => stored_roles(&found),
    };
    let credentials = match body.get_str("pwd") {
        Some(password) => match credentials(shared, &user, password, &mechanisms(body)) {
            Ok(credentials) => credentials,
            Err(problem) => return problem,
        },
        None => found.get_document("credentials").cloned().unwrap_or_default(),
    };
    let custom = body.get("customData").or_else(|| found.get("customData"));
    match save(shared, database, &user, user_record(database, &user, &roles, credentials, custom), true) {
        Ok(()) => ok(Document::new()),
        Err(problem) => problem,
    }
}

fn change_roles(shared: &Shared<'_>, command: &str, database: &str, body: &Document) -> Document {
    let Some(user) = named(body, command) else {
        return failure(2, "BadValue", format!("{command} needs a user name"));
    };
    let Some(found) = stored_document(shared, database, &user) else {
        return failure(11, "UserNotFound", format!("User '{user}@{database}' not found"));
    };
    let changed = match roles_of(body.get("roles"), database) {
        Ok(roles) => roles,
        Err(problem) => return problem,
    };
    let mut roles = stored_roles(&found);
    if command == "grantRolesToUser" {
        for role in changed {
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
    } else {
        roles.retain(|role| !changed.contains(role));
    }
    let credentials = found.get_document("credentials").cloned().unwrap_or_default();
    match save(shared, database, &user, user_record(database, &user, &roles, credentials, found.get("customData")), true) {
        Ok(()) => ok(Document::new()),
        Err(problem) => problem,
    }
}

fn users_info(shared: &Shared<'_>, database: &str, body: &Document) -> Document {
    let wanted: Option<Vec<(String, String)>> = match body.get("usersInfo") {
        Some(Bson::String(user)) => Some(vec![(database.to_owned(), user.clone())]),
        Some(Bson::Document(given)) => Some(vec![(
            given.get_str("db").unwrap_or(database).to_owned(),
            given.get_str("user").unwrap_or_default().to_owned(),
        )]),
        Some(Bson::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| match item {
                    Bson::String(user) => Some((database.to_owned(), user.clone())),
                    Bson::Document(given) => Some((given.get_str("db").unwrap_or(database).to_owned(), given.get_str("user")?.to_owned())),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    };
    let filter = match &wanted {
        Some(_) => parse_filter(&Document::new()),
        None if matches!(body.get("forAllDBs"), Some(Bson::Boolean(true))) => parse_filter(&Document::new()),
        None => parse_filter(&document(vec![("db", text(database))])),
    };
    let found = match filter.and_then(|filter| shared.store.find(ADMIN, USERS, &filter, &SortSpec::default())) {
        Ok(found) => found,
        Err(error) => return crate::commands::failure(&error),
    };
    let show_credentials = matches!(body.get("showCredentials"), Some(Bson::Boolean(true)));
    let users: Vec<Bson> = found
        .into_iter()
        .filter(|record| {
            wanted.as_ref().is_none_or(|wanted| {
                wanted.iter().any(|(db, user)| record.get_str("db") == Some(db.as_str()) && record.get_str("user") == Some(user.as_str()))
            })
        })
        .map(|record| {
            let mut shown = document(vec![
                ("_id", record.get("_id").cloned().unwrap_or(Bson::Null)),
                ("user", record.get("user").cloned().unwrap_or(Bson::Null)),
                ("db", record.get("db").cloned().unwrap_or(Bson::Null)),
                ("roles", record.get("roles").cloned().unwrap_or(Bson::Array(Vec::new()))),
            ]);
            let mechanisms: Vec<Bson> = record
                .get_document("credentials")
                .map(|credentials| credentials.iter().map(|(name, _)| text(name)).collect())
                .unwrap_or_default();
            shown.insert("mechanisms", Bson::Array(mechanisms));
            if let Some(custom) = record.get("customData") {
                shown.insert("customData", custom.clone());
            }
            if show_credentials && let Some(credentials) = record.get("credentials") {
                shown.insert("credentials", credentials.clone());
            }
            Bson::Document(shown)
        })
        .collect();
    ok(document(vec![("users", Bson::Array(users))]))
}

fn holds(roles: &[(String, String)], database: &str, names: &[&str], any_database: &[&str]) -> bool {
    roles.iter().any(|(role, db)| {
        role == "root"
            || (db == database && names.contains(&role.as_str()))
            || (db == ADMIN && any_database.contains(&role.as_str()))
    })
}

fn writes(command: &str, body: &Document) -> bool {
    if WRITE_COMMANDS.contains(&command) {
        return true;
    }
    command == "aggregate"
        && body.get_array("pipeline").and_then(|pipeline| pipeline.last()).is_some_and(|stage| match stage {
            Bson::Document(stage) => stage.get("$out").is_some() || stage.get("$merge").is_some(),
            _ => false,
        })
}

/// The roles of the session's signed-in user: `root` for the server's own user, the stored
/// roles for a user created with `createUser`.
pub(crate) fn session_roles(shared: &Shared<'_>, session: &mut Session) -> Vec<(String, String)> {
    if let Some(roles) = &session.roles {
        return roles.clone();
    }
    let Some(user) = session.authenticated_user.clone() else {
        return Vec::new();
    };
    let roles = if user == shared.config.user {
        vec![("root".to_owned(), ADMIN.to_owned())]
    } else {
        stored_user(shared, &session.authentication_database, &user).map(|stored| stored.roles).unwrap_or_default()
    };
    session.roles = Some(roles.clone());
    roles
}

/// Whether the session's user may run `command` on `database`, by MongoDB's built-in roles:
/// reads need `read` or more, writes `readWrite` or `dbOwner`, collection and index definitions
/// `dbAdmin` too, and user commands `userAdmin` or `dbOwner`; `*AnyDatabase` roles held on
/// `admin` and `root` cover every database.
pub(crate) fn allowed(shared: &Shared<'_>, session: &mut Session, command: &str, database: &str, body: &Document) -> bool {
    if ANY_DATABASE_COMMANDS.contains(&command) || session.authenticated_user.is_none() {
        return true;
    }
    let roles = session_roles(shared, session);
    let touches_users = database == ADMIN && body.first().is_some_and(|(_, value)| value.as_str() == Some(USERS));
    if USER_COMMANDS.contains(&command) || touches_users {
        return holds(&roles, database, &["userAdmin", "dbOwner"], &["userAdminAnyDatabase"]);
    }
    if command == "usersInfo" || command == "rolesInfo" {
        return holds(&roles, database, &["userAdmin", "dbOwner", "read", "readWrite", "dbAdmin"], &["userAdminAnyDatabase", "readAnyDatabase", "readWriteAnyDatabase"]);
    }
    if DEFINITION_COMMANDS.contains(&command) && holds(&roles, database, &["dbAdmin"], &["dbAdminAnyDatabase"]) {
        return true;
    }
    if writes(command, body) {
        return holds(&roles, database, &["readWrite", "dbOwner"], &["readWriteAnyDatabase"]);
    }
    holds(&roles, database, &["read", "readWrite", "dbOwner", "dbAdmin"], &["readAnyDatabase", "readWriteAnyDatabase"])
}
