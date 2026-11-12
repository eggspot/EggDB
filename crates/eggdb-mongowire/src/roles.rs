use crate::commands::{Shared, document, ok, text};
use crate::users::{ADMIN, BUILT_IN_ROLES, by_id, failure};
use eggdb_bson::{Bson, Document};
use eggdb_documents::{SortSpec, parse_filter};

const ROLES: &str = ROLES_COLLECTION;

/// Where user-defined roles are kept, in the `admin` database.
pub(crate) const ROLES_COLLECTION: &str = "system.roles";
const MOST_PRIVILEGES: usize = 1_000;
const MOST_INHERITED: usize = 1_000;

/// The role commands this module runs.
pub(crate) const ROLE_COMMANDS: [&str; 8] = [
    "createRole",
    "updateRole",
    "dropRole",
    "dropAllRolesFromDatabase",
    "grantRolesToRole",
    "revokeRolesFromRole",
    "grantPrivilegesToRole",
    "revokePrivilegesFromRole",
];

/// One privilege of a user-defined role: actions on a database and collection (empty for
/// any), or on the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Privilege {
    database: Option<String>,
    collection: Option<String>,
    cluster: bool,
    actions: Vec<String>,
}

impl Privilege {
    fn covers(&self, database: &str, collection: &str, action: &str) -> bool {
        if !self.actions.iter().any(|held| held == action || held == "anyAction") {
            return false;
        }
        if self.cluster {
            return true;
        }
        let database_fits = self.database.as_deref().is_none_or(|held| held.is_empty() || held == database);
        let collection_fits = self.collection.as_deref().is_none_or(|held| held.is_empty() || held == collection);
        database_fits && collection_fits
    }

    fn to_bson(&self) -> Bson {
        let resource = if self.cluster {
            document(vec![("cluster", Bson::Boolean(true))])
        } else {
            document(vec![
                ("db", text(self.database.as_deref().unwrap_or_default())),
                ("collection", text(self.collection.as_deref().unwrap_or_default())),
            ])
        };
        Bson::Document(document(vec![
            ("resource", Bson::Document(resource)),
            ("actions", Bson::Array(self.actions.iter().map(|action| text(action)).collect())),
        ]))
    }
}

fn role_id(database: &str, role: &str) -> String {
    format!("{database}.{role}")
}

fn stored_role(shared: &Shared<'_>, database: &str, role: &str) -> Option<Document> {
    let filter = by_id(&role_id(database, role))?;
    shared.store.find(ADMIN, ROLES, &filter, &SortSpec::default()).ok()?.into_iter().next()
}

/// Whether `role` of `database` is a built-in role or one created with `createRole`.
pub(crate) fn exists(shared: &Shared<'_>, role: &str, database: &str) -> bool {
    BUILT_IN_ROLES.contains(&role) || stored_role(shared, database, role).is_some()
}

fn privileges_of(value: Option<&Bson>) -> Result<Vec<Privilege>, Document> {
    let Some(Bson::Array(items)) = value else {
        return Ok(Vec::new());
    };
    if items.len() > MOST_PRIVILEGES {
        return Err(failure(2, "BadValue", format!("a role may hold at most {MOST_PRIVILEGES} privileges")));
    }
    items
        .iter()
        .map(|item| {
            let Bson::Document(item) = item else {
                return Err(failure(14, "TypeMismatch", "a privilege is a {resource, actions} document"));
            };
            let Some(resource) = item.get_document("resource") else {
                return Err(failure(2, "BadValue", "a privilege needs a resource"));
            };
            let actions: Vec<String> = item
                .get_array("actions")
                .map(|actions| actions.iter().filter_map(Bson::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            if actions.is_empty() {
                return Err(failure(2, "BadValue", "a privilege needs at least one action"));
            }
            Ok(Privilege {
                database: resource.get_str("db").map(str::to_owned),
                collection: resource.get_str("collection").map(str::to_owned),
                cluster: resource.get_bool("cluster") == Some(true) || resource.get_bool("anyResource") == Some(true),
                actions,
            })
        })
        .collect()
}

fn inherited_of(value: Option<&Bson>, database: &str) -> Result<Vec<(String, String)>, Document> {
    let Some(Bson::Array(items)) = value else {
        return Ok(Vec::new());
    };
    if items.len() > MOST_INHERITED {
        return Err(failure(2, "BadValue", format!("a role may inherit at most {MOST_INHERITED} roles")));
    }
    items
        .iter()
        .map(|item| match item {
            Bson::String(role) => Ok((role.clone(), database.to_owned())),
            Bson::Document(given) => Ok((
                given.get_str("role").unwrap_or_default().to_owned(),
                given.get_str("db").unwrap_or(database).to_owned(),
            )),
            _ => Err(failure(14, "TypeMismatch", "a role must be a name or a {role, db} document")),
        })
        .collect()
}

fn checked_inherited(shared: &Shared<'_>, roles: &[(String, String)]) -> Result<(), Document> {
    match roles.iter().find(|(role, db)| !exists(shared, role, db)) {
        Some((role, db)) => Err(failure(31, "RoleNotFound", format!("Could not find role: {role}@{db}"))),
        None => Ok(()),
    }
}

fn roles_bson(roles: &[(String, String)]) -> Bson {
    Bson::Array(
        roles
            .iter()
            .map(|(role, db)| Bson::Document(document(vec![("role", text(role)), ("db", text(db))])))
            .collect(),
    )
}

fn role_record(database: &str, role: &str, privileges: &[Privilege], roles: &[(String, String)]) -> Document {
    document(vec![
        ("_id", text(&role_id(database, role))),
        ("role", text(role)),
        ("db", text(database)),
        ("privileges", Bson::Array(privileges.iter().map(Privilege::to_bson).collect())),
        ("roles", roles_bson(roles)),
    ])
}

fn save(shared: &Shared<'_>, database: &str, role: &str, record: Document, replace: bool) -> Result<(), Document> {
    if replace && let Some(filter) = by_id(&role_id(database, role)) {
        shared.store.delete(ADMIN, ROLES, &filter, true).map_err(|error| crate::commands::failure(&error))?;
    }
    shared.store.insert(ADMIN, ROLES, vec![record], true).map_err(|error| crate::commands::failure(&error))?;
    Ok(())
}

fn named(body: &Document, command: &str) -> Option<String> {
    body.get_str(command).map(str::to_owned).filter(|name| !name.is_empty())
}

fn stored_parts(found: &Document, database: &str) -> (Vec<Privilege>, Vec<(String, String)>) {
    (
        privileges_of(found.get("privileges")).unwrap_or_default(),
        inherited_of(found.get("roles"), database).unwrap_or_default(),
    )
}

/// Runs a role command (`createRole`, `updateRole`, `dropRole`, `dropAllRolesFromDatabase`,
/// `grantRolesToRole`, `revokeRolesFromRole`, `grantPrivilegesToRole`,
/// `revokePrivilegesFromRole`); `None` for another.
pub(crate) fn run(shared: &Shared<'_>, command: &str, database: &str, body: &Document) -> Option<Document> {
    if !ROLE_COMMANDS.contains(&command) {
        return None;
    }
    if command == "dropAllRolesFromDatabase" {
        let filter = parse_filter(&document(vec![("db", text(database))]));
        return Some(match filter.map(|filter| shared.store.delete(ADMIN, ROLES, &filter, false)) {
            Ok(Ok(removed)) => ok(document(vec![("n", Bson::Int64(removed as i64))])),
            Ok(Err(error)) | Err(error) => crate::commands::failure(&error),
        });
    }
    let Some(role) = named(body, command) else {
        return Some(failure(2, "BadValue", format!("{command} needs a role name")));
    };
    if BUILT_IN_ROLES.contains(&role.as_str()) {
        return Some(failure(2, "BadValue", format!("Cannot change the built-in role {role}")));
    }
    let found = stored_role(shared, database, &role);
    Some(match (command, found) {
        ("createRole", Some(_)) => failure(51_002, "Location51002", format!("Role \"{role}@{database}\" already exists")),
        ("createRole", None) => {
            let (privileges, roles) = match (privileges_of(body.get("privileges")), inherited_of(body.get("roles"), database)) {
                (Ok(privileges), Ok(roles)) => (privileges, roles),
                (Err(problem), _) | (_, Err(problem)) => return Some(problem),
            };
            if let Err(problem) = checked_inherited(shared, &roles) {
                return Some(problem);
            }
            done(save(shared, database, &role, role_record(database, &role, &privileges, &roles), false))
        }
        (_, None) => failure(31, "RoleNotFound", format!("Role '{role}@{database}' not found")),
        ("dropRole", Some(_)) => match by_id(&role_id(database, &role)).map(|filter| shared.store.delete(ADMIN, ROLES, &filter, true)) {
            Some(Ok(_)) => ok(Document::new()),
            Some(Err(error)) => crate::commands::failure(&error),
            None => failure(2, "BadValue", "the role name cannot be used"),
        },
        (_, Some(found)) => {
            let (mut privileges, mut roles) = stored_parts(&found, database);
            let given_privileges = match privileges_of(body.get("privileges")) {
                Ok(given) => given,
                Err(problem) => return Some(problem),
            };
            let given_roles = match inherited_of(body.get("roles"), database) {
                Ok(given) => given,
                Err(problem) => return Some(problem),
            };
            match command {
                "updateRole" => {
                    if body.get("privileges").is_some() {
                        privileges = given_privileges;
                    }
                    if body.get("roles").is_some() {
                        roles = given_roles;
                    }
                }
                "grantRolesToRole" => roles.extend(given_roles.into_iter().filter(|given| !roles.contains(given)).collect::<Vec<_>>()),
                "revokeRolesFromRole" => roles.retain(|held| !given_roles.contains(held)),
                "grantPrivilegesToRole" => privileges.extend(given_privileges),
                _ => privileges.retain(|held| !given_privileges.contains(held)),
            }
            if roles.iter().any(|(name, db)| *name == role && db == database) {
                return Some(failure(17_166, "InvalidRoleModification", format!("Role \"{role}@{database}\" cannot inherit from itself")));
            }
            if let Err(problem) = checked_inherited(shared, &roles) {
                return Some(problem);
            }
            done(save(shared, database, &role, role_record(database, &role, &privileges, &roles), true))
        }
    })
}

fn done(saved: Result<(), Document>) -> Document {
    match saved {
        Ok(()) => ok(Document::new()),
        Err(problem) => problem,
    }
}

/// `rolesInfo`: the built-in roles and the database's user-defined ones (`rolesInfo: 1`), or
/// the roles named, with their inherited roles and, with `showPrivileges`, their privileges.
pub(crate) fn roles_info(shared: &Shared<'_>, database: &str, body: &Document) -> Document {
    let show_privileges = matches!(body.get("showPrivileges"), Some(Bson::Boolean(true)));
    let show_built_in = !matches!(body.get("showBuiltinRoles"), Some(Bson::Boolean(false)));
    let wanted: Option<Vec<(String, String)>> = match body.get("rolesInfo") {
        Some(Bson::String(role)) => Some(vec![(role.clone(), database.to_owned())]),
        Some(Bson::Document(given)) => Some(vec![(
            given.get_str("role").unwrap_or_default().to_owned(),
            given.get_str("db").unwrap_or(database).to_owned(),
        )]),
        Some(Bson::Array(items)) => inherited_of(Some(&Bson::Array(items.clone())), database).ok(),
        _ => None,
    };
    let mut shown = Vec::new();
    let described = |role: &str, db: &str, built_in: bool, found: Option<&Document>| {
        let (privileges, inherited) = found.map(|found| stored_parts(found, db)).unwrap_or_default();
        let mut entry = document(vec![
            ("role", text(role)),
            ("db", text(db)),
            ("isBuiltin", Bson::Boolean(built_in)),
            ("roles", roles_bson(&inherited)),
            ("inheritedRoles", roles_bson(&inherited)),
        ]);
        if show_privileges {
            entry.insert("privileges", Bson::Array(privileges.iter().map(Privilege::to_bson).collect()));
        }
        Bson::Document(entry)
    };
    match wanted {
        Some(wanted) => {
            for (role, db) in wanted {
                if BUILT_IN_ROLES.contains(&role.as_str()) {
                    shown.push(described(&role, &db, true, None));
                } else if let Some(found) = stored_role(shared, &db, &role) {
                    shown.push(described(&role, &db, false, Some(&found)));
                }
            }
        }
        None => {
            if show_built_in {
                shown.extend(BUILT_IN_ROLES.iter().map(|role| described(role, database, true, None)));
            }
            let filter = parse_filter(&document(vec![("db", text(database))]));
            if let Ok(found) = filter.and_then(|filter| shared.store.find(ADMIN, ROLES, &filter, &SortSpec::default())) {
                for record in &found {
                    let role = record.get_str("role").unwrap_or_default();
                    shown.push(described(role, database, false, Some(record)));
                }
            }
        }
    }
    ok(document(vec![("roles", Bson::Array(shown))]))
}

/// Everything `roles` grant: the roles themselves with every role they inherit, transitively,
/// and the privileges of the user-defined ones among them.
pub(crate) fn expanded(shared: &Shared<'_>, roles: &[(String, String)]) -> (Vec<(String, String)>, Vec<Privilege>) {
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut privileges = Vec::new();
    let mut waiting: Vec<(String, String)> = roles.to_vec();
    while let Some((role, db)) = waiting.pop() {
        if seen.contains(&(role.clone(), db.clone())) || seen.len() >= MOST_INHERITED {
            continue;
        }
        seen.push((role.clone(), db.clone()));
        if BUILT_IN_ROLES.contains(&role.as_str()) {
            continue;
        }
        if let Some(found) = stored_role(shared, &db, &role) {
            let (held, inherited) = stored_parts(&found, &db);
            privileges.extend(held);
            waiting.extend(inherited);
        }
    }
    (seen, privileges)
}

/// The privilege actions MongoDB checks for `command`.
pub(crate) fn actions_of(command: &str, writes_out: bool) -> &'static [&'static str] {
    match command {
        "find" | "count" | "distinct" | "getMore" | "killCursors" => &["find"],
        "aggregate" if writes_out => &["find", "insert", "remove"],
        "aggregate" => &["find"],
        "insert" => &["insert"],
        "update" => &["update"],
        "delete" => &["remove"],
        "findAndModify" | "findandmodify" => &["find", "update"],
        "create" => &["createCollection"],
        "drop" => &["dropCollection"],
        "createIndexes" => &["createIndex"],
        "dropIndexes" => &["dropIndex"],
        "listIndexes" => &["listIndexes"],
        "listCollections" => &["listCollections"],
        "renameCollection" => &["renameCollectionSameDB"],
        "collMod" => &["collMod"],
        "dropDatabase" => &["dropDatabase"],
        "createUser" => &["createUser"],
        "dropUser" | "dropAllUsersFromDatabase" => &["dropUser"],
        "updateUser" => &["changePassword", "changeCustomData"],
        "grantRolesToUser" => &["grantRole"],
        "revokeRolesFromUser" => &["revokeRole"],
        "usersInfo" | "rolesInfo" => &["viewUser", "viewRole"],
        "createRole" => &["createRole"],
        "dropRole" | "dropAllRolesFromDatabase" => &["dropRole"],
        "updateRole" | "grantRolesToRole" | "revokeRolesFromRole" | "grantPrivilegesToRole" | "revokePrivilegesFromRole" => {
            &["grantRole", "revokeRole"]
        }
        _ => &["find"],
    }
}

/// Whether `privileges` allow every action `command` needs on `database`.`collection`.
pub(crate) fn privileges_allow(privileges: &[Privilege], command: &str, writes_out: bool, database: &str, collection: &str) -> bool {
    !privileges.is_empty()
        && actions_of(command, writes_out)
            .iter()
            .all(|action| privileges.iter().any(|privilege| privilege.covers(database, collection, action)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileges_cover_their_resources_and_actions() {
        let reader = Privilege {
            database: Some("shop".to_owned()),
            collection: Some("orders".to_owned()),
            cluster: false,
            actions: vec!["find".to_owned()],
        };
        assert!(reader.covers("shop", "orders", "find"));
        assert!(!reader.covers("shop", "users", "find"));
        assert!(!reader.covers("shop", "orders", "insert"));
        let any_collection = Privilege {
            collection: Some(String::new()),
            actions: vec!["find".to_owned(), "insert".to_owned()],
            ..reader.clone()
        };
        assert!(any_collection.covers("shop", "users", "insert"));
        assert!(privileges_allow(&[reader.clone()], "find", false, "shop", "orders"));
        assert!(!privileges_allow(&[reader], "findAndModify", false, "shop", "orders"));
        assert!(privileges_allow(&[any_collection], "aggregate", false, "shop", "x"));
    }
}
