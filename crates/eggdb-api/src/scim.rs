//! SCIM 2.0 provisioning on the REST listener (RFC 7643 and RFC 7644, D1198): an identity
//! provider (Entra ID, Okta, OneLogin, Keycloak) creates, changes and removes EggDB's roles
//! through `/scim/v2/Users` and `/scim/v2/Groups`. A user is a role with `LOGIN` (`active`
//! false takes it away), a group is a role without, and a group's members are its role
//! members. Only the server's own user may call it.

use crate::request::literal;
use crate::{Endpoint, json_string};
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};
use eggdb_sql::{KeyValueStore, QueryResult, Value};

const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
const CONTENT_TYPE: &str = "application/scim+json";
const MOST_NAME_BYTES: usize = 63;
const SETUP: [&str; 2] = [
    "CREATE SCHEMA IF NOT EXISTS eggdb_scim",
    "CREATE TABLE IF NOT EXISTS eggdb_scim.resources (id text PRIMARY KEY, kind text NOT NULL, external_id text, display_name text)",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Group,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Group => "Group",
        }
    }

    fn collection(self) -> &'static str {
        match self {
            Self::User => "Users",
            Self::Group => "Groups",
        }
    }
}

struct Resource {
    id: String,
    external_id: Option<String>,
    display_name: Option<String>,
    active: bool,
    members: Vec<String>,
}

fn scim(status: u16, body: String) -> Response {
    Response {
        status,
        content_type: CONTENT_TYPE.to_owned(),
        body: body.into_bytes(),
        headers: vec![("Cache-Control".to_owned(), "no-store".to_owned())],
    }
}

fn scim_error(status: u16, scim_type: Option<&str>, detail: &str) -> Response {
    let kind = scim_type.map_or(String::new(), |kind| format!(",\"scimType\":{}", json_string(kind)));
    scim(
        status,
        format!(
            "{{\"schemas\":[\"{ERROR_SCHEMA}\"],\"status\":\"{status}\"{kind},\"detail\":{}}}",
            json_string(detail)
        ),
    )
}

fn role_name(name: &str) -> Option<String> {
    let valid = !name.is_empty()
        && name.len() <= MOST_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'@' | b'-'));
    valid.then(|| format!("\"{name}\""))
}

fn member<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Json> {
    members
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, node)| &node.value)
}

fn text_member(members: &[(String, Node)], name: &str) -> Option<String> {
    match member(members, name)? {
        Json::String(text) => Some(text.clone()),
        _ => None,
    }
}

fn boolean_member(members: &[(String, Node)], name: &str) -> Option<bool> {
    match member(members, name)? {
        Json::Boolean(value) => Some(*value),
        Json::String(text) => Some(text.eq_ignore_ascii_case("true")),
        _ => None,
    }
}

fn member_ids(value: &Json) -> Vec<String> {
    let Json::Array(items) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match &item.value {
            Json::Object(fields) => text_member(fields, "value"),
            _ => None,
        })
        .collect()
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn eq_filter(filter: &str) -> Option<(String, String)> {
    let (attribute, rest) = filter.trim().split_once(' ')?;
    let value = rest.trim().strip_prefix("eq")?.trim();
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    Some((attribute.to_ascii_lowercase(), value.replace("\\\"", "\"")))
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    /// Answers `/scim/v2/...`, or `None` for any other path.
    pub(crate) fn scim_page(&self, path: &str, request: &Request) -> Option<Response> {
        let rest = path.strip_prefix("scim/v2")?;
        let rest = rest.trim_start_matches('/');
        let (collection, id) = match rest.split_once('/') {
            Some((collection, id)) => (collection, Some(crate::request::percent_decode(id))),
            None => (rest, None),
        };
        let method = request.method.as_str();
        Some(match (collection, id.as_deref(), method) {
            ("ServiceProviderConfig", None, "GET") => scim(
                200,
                "{\"schemas\":[\"urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig\"],\"patch\":{\"supported\":true},\"bulk\":{\"supported\":false,\"maxOperations\":0,\"maxPayloadSize\":0},\"filter\":{\"supported\":true,\"maxResults\":10000},\"changePassword\":{\"supported\":true},\"sort\":{\"supported\":false},\"etag\":{\"supported\":false},\"authenticationSchemes\":[{\"type\":\"httpbasic\",\"name\":\"HTTP Basic\",\"description\":\"The server's own user\"}]}".to_owned(),
            ),
            ("Users" | "Groups", _, _) => {
                let kind = if collection == "Users" { Kind::User } else { Kind::Group };
                self.scim_resources(kind, id.as_deref(), method, request)
            }
            _ => scim_error(404, None, "SCIM serves /scim/v2/Users, /scim/v2/Groups and /scim/v2/ServiceProviderConfig"),
        })
    }

    fn scim_sql(&self, statements: &[String]) -> Result<Vec<QueryResult>, Response> {
        let mut session = self.session().map_err(|error| scim_error(500, None, &error.to_string()))?;
        let mut results = Vec::new();
        for sql in SETUP.iter().map(|sql| (*sql).to_owned()).chain(statements.iter().cloned()) {
            session.execute(&sql, &mut |result| results.push(result)).map_err(|error| {
                let status = match error.state().code() {
                    "42710" | "23505" => 409,
                    "42704" => 404,
                    "2BP01" => 409,
                    _ => 400,
                };
                scim_error(status, (status == 409).then_some("uniqueness"), &error.to_string())
            })?;
        }
        Ok(results)
    }

    fn scim_read(&self, kind: Kind, wanted: Option<&str>) -> Result<Vec<Resource>, Response> {
        let mut query = format!(
            "SELECT r.id, r.external_id, r.display_name, p.rolcanlogin FROM eggdb_scim.resources r JOIN pg_catalog.pg_roles p ON p.rolname = r.id WHERE r.kind = {}",
            literal(kind.word())
        );
        if let Some(id) = wanted {
            query.push_str(&format!(" AND r.id = {}", literal(id)));
        }
        query.push_str(" ORDER BY r.id");
        let results = self.scim_sql(&[
            query,
            "SELECT grantee, role_name FROM information_schema.applicable_roles".to_owned(),
        ])?;
        let memberships: Vec<(String, String)> = results
            .last()
            .map(|result| {
                result
                    .rows
                    .iter()
                    .filter_map(|row| Some((text_of(row.first()?)?, text_of(row.get(1)?)?)))
                    .collect()
            })
            .unwrap_or_default();
        let rows = results.iter().rev().nth(1).map(|result| result.rows.clone()).unwrap_or_default();
        Ok(rows
            .iter()
            .filter_map(|row| {
                let id = text_of(row.first()?)?;
                let members = memberships
                    .iter()
                    .filter(|(_, role)| *role == id)
                    .map(|(member, _)| member.clone())
                    .collect();
                Some(Resource {
                    external_id: row.get(1).and_then(text_of),
                    display_name: row.get(2).and_then(text_of),
                    active: matches!(row.get(3), Some(Value::Boolean(true))),
                    members,
                    id,
                })
            })
            .collect())
    }

    fn scim_resources(&self, kind: Kind, id: Option<&str>, method: &str, request: &Request) -> Response {
        let outcome = match (id, method) {
            (None, "GET") => self.scim_list(kind, request),
            (Some(id), "GET") => self.scim_one(kind, id),
            (None, "POST") => self.scim_create(kind, &request.body),
            (Some(id), "PUT") => self.scim_replace(kind, id, &request.body),
            (Some(id), "PATCH") => self.scim_patch(kind, id, &request.body),
            (Some(id), "DELETE") => self.scim_delete(kind, id),
            _ => Err(scim_error(405, None, "use GET, POST, PUT, PATCH or DELETE")),
        };
        outcome.unwrap_or_else(|response| response)
    }

    fn scim_rendered(&self, kind: Kind, resource: &Resource) -> String {
        let location = format!("/scim/v2/{}/{}", kind.collection(), resource.id);
        let external = resource
            .external_id
            .as_ref()
            .map_or(String::new(), |id| format!(",\"externalId\":{}", json_string(id)));
        let meta = format!(
            "\"meta\":{{\"resourceType\":\"{}\",\"location\":{}}}",
            kind.word(),
            json_string(&location)
        );
        match kind {
            Kind::User => format!(
                "{{\"schemas\":[\"{USER_SCHEMA}\"],\"id\":{},\"userName\":{}{external},\"active\":{},{meta}}}",
                json_string(&resource.id),
                json_string(&resource.id),
                resource.active
            ),
            Kind::Group => {
                let members: Vec<String> = resource
                    .members
                    .iter()
                    .map(|member| format!("{{\"value\":{}}}", json_string(member)))
                    .collect();
                format!(
                    "{{\"schemas\":[\"{GROUP_SCHEMA}\"],\"id\":{},\"displayName\":{}{external},\"members\":[{}],{meta}}}",
                    json_string(&resource.id),
                    json_string(resource.display_name.as_deref().unwrap_or(&resource.id)),
                    members.join(",")
                )
            }
        }
    }

    fn scim_list(&self, kind: Kind, request: &Request) -> Result<Response, Response> {
        let filter = request
            .query
            .as_deref()
            .unwrap_or_default()
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == "filter")
            .map(|(_, value)| crate::request::percent_decode(value));
        let wanted = match filter.as_deref().map(eq_filter) {
            None => None,
            Some(Some(found)) => Some(found),
            Some(None) => return Err(scim_error(400, Some("invalidFilter"), "only attribute eq \"value\" filters are supported")),
        };
        let resources: Vec<Resource> = self
            .scim_read(kind, None)?
            .into_iter()
            .filter(|resource| match &wanted {
                None => true,
                Some((attribute, value)) => match attribute.as_str() {
                    "username" | "displayname" | "id" => {
                        resource.id == *value || resource.display_name.as_deref() == Some(value)
                    }
                    "externalid" => resource.external_id.as_deref() == Some(value),
                    _ => false,
                },
            })
            .collect();
        let rendered: Vec<String> = resources.iter().map(|resource| self.scim_rendered(kind, resource)).collect();
        Ok(scim(
            200,
            format!(
                "{{\"schemas\":[\"{LIST_SCHEMA}\"],\"totalResults\":{},\"startIndex\":1,\"itemsPerPage\":{},\"Resources\":[{}]}}",
                rendered.len(),
                rendered.len(),
                rendered.join(",")
            ),
        ))
    }

    fn scim_one(&self, kind: Kind, id: &str) -> Result<Response, Response> {
        let resource = self
            .scim_read(kind, Some(id))?
            .into_iter()
            .next()
            .ok_or_else(|| scim_error(404, None, &format!("no {} {id}", kind.word())))?;
        Ok(scim(200, self.scim_rendered(kind, &resource)))
    }

    fn scim_create(&self, kind: Kind, body: &[u8]) -> Result<Response, Response> {
        let fields = scim_body(body)?;
        let name_field = match kind {
            Kind::User => "userName",
            Kind::Group => "displayName",
        };
        let name = text_member(&fields, name_field)
            .ok_or_else(|| scim_error(400, Some("invalidValue"), &format!("a {} needs {name_field}", kind.word())))?;
        let role = role_name(&name).ok_or_else(|| {
            scim_error(400, Some("invalidValue"), "a name is 1 to 63 letters, digits, '_', '.', '@' or '-'")
        })?;
        let mut statements = Vec::new();
        match kind {
            Kind::User => {
                let login = if boolean_member(&fields, "active").unwrap_or(true) { "LOGIN" } else { "NOLOGIN" };
                let password = text_member(&fields, "password")
                    .map_or(String::new(), |password| format!(" PASSWORD {}", literal(&password)));
                statements.push(format!("CREATE ROLE {role} {login}{password}"));
            }
            Kind::Group => {
                statements.push(format!("CREATE ROLE {role} NOLOGIN"));
                for member in member(&fields, "members").map(member_ids).unwrap_or_default() {
                    let member = role_name(&member)
                        .ok_or_else(|| scim_error(400, Some("invalidValue"), "a member is not a user's id"))?;
                    statements.push(format!("GRANT {role} TO {member}"));
                }
            }
        }
        statements.push(format!(
            "INSERT INTO eggdb_scim.resources VALUES ({}, {}, {}, {})",
            literal(&name),
            literal(kind.word()),
            text_member(&fields, "externalId").map_or("NULL".to_owned(), |id| literal(&id)),
            literal(&name)
        ));
        self.scim_transaction(statements)?;
        let resource = self
            .scim_read(kind, Some(&name))?
            .into_iter()
            .next()
            .ok_or_else(|| scim_error(500, None, "the new resource cannot be read back"))?;
        let mut response = scim(201, self.scim_rendered(kind, &resource));
        response
            .headers
            .push(("Location".to_owned(), format!("/scim/v2/{}/{name}", kind.collection())));
        Ok(response)
    }

    fn scim_transaction(&self, mut statements: Vec<String>) -> Result<(), Response> {
        statements.insert(0, "BEGIN".to_owned());
        statements.push("COMMIT".to_owned());
        self.scim_sql(&statements).map(|_| ())
    }

    fn scim_existing(&self, kind: Kind, id: &str) -> Result<(Resource, String), Response> {
        let resource = self
            .scim_read(kind, Some(id))?
            .into_iter()
            .next()
            .ok_or_else(|| scim_error(404, None, &format!("no {} {id}", kind.word())))?;
        let role = role_name(id).ok_or_else(|| scim_error(404, None, &format!("no {} {id}", kind.word())))?;
        Ok((resource, role))
    }

    fn scim_replace(&self, kind: Kind, id: &str, body: &[u8]) -> Result<Response, Response> {
        let (resource, role) = self.scim_existing(kind, id)?;
        let fields = scim_body(body)?;
        let mut statements = Vec::new();
        match kind {
            Kind::User => {
                let login = if boolean_member(&fields, "active").unwrap_or(true) { "LOGIN" } else { "NOLOGIN" };
                statements.push(format!("ALTER ROLE {role} {login}"));
                if let Some(password) = text_member(&fields, "password") {
                    statements.push(format!("ALTER ROLE {role} PASSWORD {}", literal(&password)));
                }
            }
            Kind::Group => {
                let wanted = member(&fields, "members").map(member_ids).unwrap_or_default();
                statements.extend(self.membership_changes(&role, &resource.members, &wanted)?);
            }
        }
        if let Some(external) = text_member(&fields, "externalId") {
            statements.push(format!(
                "UPDATE eggdb_scim.resources SET external_id = {} WHERE id = {}",
                literal(&external),
                literal(id)
            ));
        }
        self.scim_transaction(statements)?;
        self.scim_one(kind, id)
    }

    fn membership_changes(&self, group: &str, current: &[String], wanted: &[String]) -> Result<Vec<String>, Response> {
        let mut statements = Vec::new();
        for member in wanted.iter().filter(|member| !current.contains(member)) {
            let member = role_name(member).ok_or_else(|| scim_error(400, Some("invalidValue"), "a member is not a user's id"))?;
            statements.push(format!("GRANT {group} TO {member}"));
        }
        for member in current.iter().filter(|member| !wanted.contains(member)) {
            if let Some(member) = role_name(member) {
                statements.push(format!("REVOKE {group} FROM {member}"));
            }
        }
        Ok(statements)
    }

    fn scim_patch(&self, kind: Kind, id: &str, body: &[u8]) -> Result<Response, Response> {
        let (resource, role) = self.scim_existing(kind, id)?;
        let fields = scim_body(body)?;
        let Some(Json::Array(operations)) = member(&fields, "Operations") else {
            return Err(scim_error(400, Some("invalidSyntax"), &format!("a {PATCH_SCHEMA} needs Operations")));
        };
        let mut members = resource.members.clone();
        let mut statements = Vec::new();
        for operation in operations {
            let Json::Object(parts) = &operation.value else {
                return Err(scim_error(400, Some("invalidSyntax"), "each operation is an object"));
            };
            let op = text_member(parts, "op").unwrap_or_default().to_ascii_lowercase();
            let path = text_member(parts, "path").unwrap_or_default();
            let value = member(parts, "value");
            match (kind, op.as_str(), path.to_ascii_lowercase().as_str(), value) {
                (Kind::User, "replace" | "add", "active", Some(value)) => {
                    let active = matches!(value, Json::Boolean(true))
                        || matches!(value, Json::String(text) if text.eq_ignore_ascii_case("true"));
                    statements.push(format!("ALTER ROLE {role} {}", if active { "LOGIN" } else { "NOLOGIN" }));
                }
                (Kind::User, "replace" | "add", "", Some(Json::Object(values))) => {
                    if let Some(active) = boolean_member(values, "active") {
                        statements.push(format!("ALTER ROLE {role} {}", if active { "LOGIN" } else { "NOLOGIN" }));
                    }
                    if let Some(password) = text_member(values, "password") {
                        statements.push(format!("ALTER ROLE {role} PASSWORD {}", literal(&password)));
                    }
                }
                (Kind::User, "replace", "password", Some(Json::String(password))) => {
                    statements.push(format!("ALTER ROLE {role} PASSWORD {}", literal(password)));
                }
                (Kind::Group, "add", "members", Some(value)) => {
                    for added in member_ids(value) {
                        if !members.contains(&added) {
                            members.push(added);
                        }
                    }
                }
                (Kind::Group, "replace", "members", Some(value)) => members = member_ids(value),
                (Kind::Group, "remove", "members", Some(value)) => {
                    let removed = member_ids(value);
                    members.retain(|member| !removed.contains(member));
                }
                (Kind::Group, "remove", "members", None) => members.clear(),
                (Kind::Group, "remove", lowered, None) if lowered.starts_with("members[") => {
                    match eq_filter(path["members[".len()..].trim_end_matches(']')) {
                        Some((attribute, value)) if attribute == "value" => {
                            members.retain(|member| *member != value);
                        }
                        _ => return Err(scim_error(400, Some("invalidPath"), "remove members[value eq \"id\"]")),
                    }
                }
                (_, _, "externalid", Some(Json::String(external))) => statements.push(format!(
                    "UPDATE eggdb_scim.resources SET external_id = {} WHERE id = {}",
                    literal(external),
                    literal(id)
                )),
                (_, _, "displayname" | "username", _) => {
                    return Err(scim_error(400, Some("mutability"), "a role's name does not change; remove it and add a new one"));
                }
                _ => return Err(scim_error(400, Some("invalidPath"), &format!("operation {op} on '{path}' is not supported"))),
            }
        }
        if kind == Kind::Group {
            statements.extend(self.membership_changes(&role, &resource.members, &members)?);
        }
        self.scim_transaction(statements)?;
        self.scim_one(kind, id)
    }

    fn scim_delete(&self, kind: Kind, id: &str) -> Result<Response, Response> {
        let (_, role) = self.scim_existing(kind, id)?;
        self.scim_transaction(vec![
            format!("DELETE FROM eggdb_scim.resources WHERE id = {}", literal(id)),
            format!("DROP ROLE {role}"),
        ])?;
        Ok(Response {
            status: 204,
            content_type: CONTENT_TYPE.to_owned(),
            body: Vec::new(),
            headers: Vec::new(),
        })
    }
}

fn scim_body(body: &[u8]) -> Result<Vec<(String, Node)>, Response> {
    let text = std::str::from_utf8(body).map_err(|_| scim_error(400, Some("invalidSyntax"), "the body is not UTF-8"))?;
    let document = parse_document(text).map_err(|_| scim_error(400, Some("invalidSyntax"), "the body is not JSON"))?;
    match document.value {
        Json::Object(fields) => Ok(fields),
        _ => Err(scim_error(400, Some("invalidSyntax"), "the body is not a JSON object")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_filters_are_checked() {
        assert_eq!(role_name("ada@contoso.com").as_deref(), Some("\"ada@contoso.com\""));
        assert_eq!(role_name("a\"b"), None);
        assert_eq!(role_name(""), None);
        assert_eq!(
            eq_filter("userName eq \"ada@contoso.com\""),
            Some(("username".to_owned(), "ada@contoso.com".to_owned()))
        );
        assert_eq!(eq_filter("userName sw \"a\""), None);
    }
}
