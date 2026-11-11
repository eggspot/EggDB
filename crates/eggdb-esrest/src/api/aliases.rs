use super::{
    Answer, Api, error, index_glob, index_key, indices_of, json, member_of, missing_index, read,
    valid_index, write,
};
use crate::json_out::Out;
use eggdb_http::Request;
use eggdb_keyspace::{KeysRead, KeysWrite};
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::BTreeMap;

pub(super) const ALIAS_RECORD: u8 = 3;
const MAX_ACTIONS: usize = 1000;

fn alias_prefix(alias: &str) -> Vec<u8> {
    let mut key = vec![ALIAS_RECORD];
    key.extend_from_slice(alias.as_bytes());
    key.push(0);
    key
}

fn alias_key(alias: &str, index: &str) -> Vec<u8> {
    let mut key = alias_prefix(alias);
    key.extend_from_slice(index.as_bytes());
    key
}

pub(super) fn add_alias(keys: &mut dyn KeysWrite, alias: &str, index: &str) -> Result<(), String> {
    keys.put(&alias_key(alias, index), b"{}")
}

pub(super) fn all_aliases(keys: &dyn KeysRead) -> Result<Vec<(String, String)>, String> {
    Ok(keys
        .scan(&[ALIAS_RECORD])?
        .into_iter()
        .filter_map(|(key, _)| {
            let rest = key.get(1..)?;
            let split = rest.iter().position(|&byte| byte == 0)?;
            Some((
                String::from_utf8_lossy(&rest[..split]).into_owned(),
                String::from_utf8_lossy(&rest[split + 1..]).into_owned(),
            ))
        })
        .collect())
}

pub(super) fn indices_of_alias(keys: &dyn KeysRead, alias: &str) -> Result<Vec<String>, String> {
    let prefix = alias_prefix(alias);
    Ok(keys
        .scan(&prefix)?
        .into_iter()
        .map(|(key, _)| String::from_utf8_lossy(&key[prefix.len()..]).into_owned())
        .collect())
}

pub(super) fn forget_index(keys: &mut dyn KeysWrite, index: &str) -> Result<(), String> {
    for (alias, aliased) in all_aliases(keys)? {
        if aliased == index {
            keys.delete(&alias_key(&alias, &aliased))?;
        }
    }
    Ok(())
}

enum Action {
    Add(Vec<String>, Vec<String>),
    Remove(Vec<String>, Vec<String>),
}

fn names(node: &Node, single: &str, many: &str) -> Vec<String> {
    let mut out = Vec::new();
    for name in [single, many] {
        match member_of(node, name).map(|found| found.value) {
            Some(Json::String(text)) => out.push(text),
            Some(Json::Array(items)) => out.extend(items.into_iter().filter_map(|item| match item.value {
                Json::String(text) => Some(text),
                _ => None,
            })),
            _ => {}
        }
    }
    out
}

impl Api<'_> {
    pub(super) fn update_aliases(&self, request: &Request) -> Answer {
        let text = String::from_utf8_lossy(&request.body).into_owned();
        let body = parse_document(&text)?;
        let Some(Json::Array(listed)) = member_of(&body, "actions").map(|node| node.value) else {
            return Ok(error(
                400,
                "action_request_validation_exception",
                "Validation Failed: 1: no actions specified;",
            ));
        };
        if listed.len() > MAX_ACTIONS {
            return Ok(error(400, "illegal_argument_exception", "too many alias actions"));
        }
        let mut actions = Vec::new();
        for item in &listed {
            let Json::Object(members) = &item.value else {
                return Ok(error(400, "parse_exception", "an alias action is an object"));
            };
            let Some((kind, settings)) = members.first() else {
                return Ok(error(400, "parse_exception", "an alias action names add or remove"));
            };
            let indices = names(settings, "index", "indices");
            match kind.as_str() {
                "add" => actions.push(Action::Add(indices, names(settings, "alias", "aliases"))),
                "remove" => {
                    actions.push(Action::Remove(indices, names(settings, "alias", "aliases")));
                }
                other => {
                    return Ok(error(
                        400,
                        "illegal_argument_exception",
                        &format!("unsupported alias action [{other}]"),
                    ));
                }
            }
        }
        self.apply_alias_actions(actions)
    }

    fn apply_alias_actions(&self, actions: Vec<Action>) -> Answer {
        let outcome = write(self.keyspace, |keys| {
            let known = indices_of(keys)?;
            for action in &actions {
                let (indices, aliases, adding) = match action {
                    Action::Add(indices, aliases) => (indices, aliases, true),
                    Action::Remove(indices, aliases) => (indices, aliases, false),
                };
                if indices.is_empty() || aliases.is_empty() {
                    return Ok(Err(error(
                        400,
                        "action_request_validation_exception",
                        "Validation Failed: 1: [index] and [alias] are required;",
                    )));
                }
                let mut targets = Vec::new();
                for pattern in indices {
                    let matched: Vec<&String> = known
                        .iter()
                        .filter(|name| index_glob(pattern, name))
                        .collect();
                    if matched.is_empty() {
                        return Ok(Err(missing_index(pattern)));
                    }
                    targets.extend(matched.into_iter().cloned());
                }
                for alias in aliases {
                    if adding && (!valid_index(alias) || known.contains(alias)) {
                        return Ok(Err(error(
                            400,
                            "invalid_alias_name_exception",
                            &format!("Invalid alias name [{alias}]"),
                        )));
                    }
                    for index in &targets {
                        if adding {
                            keys.put(&alias_key(alias, index), b"{}")?;
                        } else if keys.get(&alias_key(alias, index))?.is_some() {
                            keys.delete(&alias_key(alias, index))?;
                        } else {
                            return Ok(Err(error(
                                404,
                                "aliases_not_found_exception",
                                &format!("aliases [{alias}] missing"),
                            )));
                        }
                    }
                }
            }
            Ok(Ok(()))
        })?;
        Ok(match outcome {
            Ok(()) => json(200, "{\"acknowledged\":true}".to_owned()),
            Err(refusal) => refusal,
        })
    }

    pub(super) fn put_alias(&self, index: &str, alias: &str) -> Answer {
        self.apply_alias_actions(vec![Action::Add(
            vec![index.to_owned()],
            vec![alias.to_owned()],
        )])
    }

    pub(super) fn delete_alias(&self, index: &str, alias: &str) -> Answer {
        self.apply_alias_actions(vec![Action::Remove(
            vec![index.to_owned()],
            vec![alias.to_owned()],
        )])
    }

    pub(super) fn get_aliases(&self, index: Option<&str>, alias: Option<&str>, head: bool) -> Answer {
        let (pairs, known) = read(self.keyspace, |keys| Ok((all_aliases(keys)?, indices_of(keys)?)))?;
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for name in &known {
            if index.is_none_or(|pattern| pattern == "_all" || index_glob(pattern, name)) {
                grouped.entry(name.clone()).or_default();
            }
        }
        let mut found = false;
        for (name, aliased) in pairs {
            if !grouped.contains_key(&aliased) {
                continue;
            }
            if alias.is_none_or(|pattern| index_glob(pattern, &name)) {
                found = true;
                if let Some(listed) = grouped.get_mut(&aliased) {
                    listed.push(name);
                }
            }
        }
        if let Some(pattern) = index
            && pattern != "_all"
            && !pattern.contains('*')
            && !known.iter().any(|name| name == pattern)
        {
            return Ok(missing_index(pattern));
        }
        if alias.is_some() {
            grouped.retain(|_, listed| !listed.is_empty());
        }
        let status = if alias.is_some() && !found { 404 } else { 200 };
        if head {
            return Ok(json(status, String::new()));
        }
        let mut out = Out::default();
        out.begin_object();
        for (name, listed) in &grouped {
            out.key(name).begin_object().key("aliases").begin_object();
            for alias in listed {
                out.key(alias).begin_object().end_object();
            }
            out.end_object().end_object();
        }
        if status == 404 {
            out.key("error")
                .string(&format!("alias [{}] missing", alias.unwrap_or_default()))
                .key("status")
                .integer(404);
        }
        out.end_object();
        Ok(json(status, out.finish()))
    }

    pub(super) fn cat_aliases(&self) -> Answer {
        let pairs = read(self.keyspace, |keys| all_aliases(keys))?;
        let mut out = Out::default();
        out.begin_array();
        for (alias, index) in pairs {
            out.begin_object()
                .key("alias")
                .string(&alias)
                .key("index")
                .string(&index)
                .key("filter")
                .string("-")
                .key("routing.index")
                .string("-")
                .key("routing.search")
                .string("-")
                .key("is_write_index")
                .string("-")
                .end_object();
        }
        out.end_array();
        Ok(json(200, out.finish()))
    }
}
