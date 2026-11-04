use super::monitoring::MonitoringRelation;
use super::oids::{self, OidRequest, Space};
use super::snapshot::CatalogSnapshot;
use super::{CatalogRelation, CatalogRow};
use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::SqlResult;
use crate::row_security::{Policy, PolicyCommand};
use crate::types::Value;

struct ListedPolicy {
    oid: u32,
    table_at: usize,
    policy: Policy,
}

fn listed_policies(snapshot: &CatalogSnapshot) -> SqlResult<Vec<ListedPolicy>> {
    let mut found = Vec::new();
    for (table_at, entry) in snapshot.relations.iter().enumerate() {
        if entry.table().is_none() {
            continue;
        }
        for policy in crate::row_security::policies(&snapshot.access, &entry.internal)? {
            found.push((table_at, entry.internal.clone(), policy));
        }
    }
    let requests: Vec<OidRequest> = found
        .iter()
        .map(|(_, table, policy)| OidRequest {
            kind: b'p',
            owner: table.clone(),
            name: policy.name.clone(),
        })
        .collect();
    let assigned = oids::assign(Space::Policy, &requests);
    Ok(found
        .into_iter()
        .zip(assigned)
        .map(|((table_at, _, policy), oid)| ListedPolicy {
            oid,
            table_at,
            policy,
        })
        .collect())
}

fn command_code(command: PolicyCommand) -> &'static str {
    match command {
        PolicyCommand::All => "*",
        PolicyCommand::Select => "r",
        PolicyCommand::Insert => "a",
        PolicyCommand::Update => "w",
        PolicyCommand::Delete => "d",
    }
}

fn command_name(command: PolicyCommand) -> &'static str {
    match command {
        PolicyCommand::All => "ALL",
        PolicyCommand::Select => "SELECT",
        PolicyCommand::Insert => "INSERT",
        PolicyCommand::Update => "UPDATE",
        PolicyCommand::Delete => "DELETE",
    }
}

fn role_oid(snapshot: &CatalogSnapshot, name: &str) -> u32 {
    if name == crate::roles::PUBLIC {
        return 0;
    }
    if name == snapshot.user {
        return oids::SESSION_USER;
    }
    oids::PREDEFINED_ROLES
        .iter()
        .find(|(_, predefined)| *predefined == name)
        .map_or_else(|| crate::roles::role_oid(name), |(oid, _)| *oid)
}

fn condition(snapshot: &CatalogSnapshot, table_at: usize, text: Option<&str>) -> Value {
    text.map_or(Value::Null, |text| {
        Value::Text(super::deparse::policy_condition_text(
            snapshot, table_at, text,
        ))
    })
}

/// `pg_policy`: one row per row security policy, its conditions as `pg_get_expr` shows them.
pub(super) fn policy_rows(snapshot: &CatalogSnapshot) -> SqlResult<Vec<Vec<Value>>> {
    Ok(listed_policies(snapshot)?
        .into_iter()
        .filter_map(|listed| {
            let entry = snapshot.relations.get(listed.table_at)?;
            let roles: Vec<Value> = listed
                .policy
                .roles
                .iter()
                .map(|role| Value::Oid(role_oid(snapshot, role)))
                .collect();
            Some(
                CatalogRow::new(CatalogRelation::Policy.columns())
                    .set("oid", Value::Oid(listed.oid))
                    .set("polname", Value::Text(listed.policy.name.clone()))
                    .set("polrelid", Value::Oid(entry.oid))
                    .set(
                        "polcmd",
                        Value::Text(command_code(listed.policy.command).to_owned()),
                    )
                    .set("polpermissive", Value::Boolean(listed.policy.permissive))
                    .set(
                        "polroles",
                        Value::Array(Box::new(ArrayValue::new(ArrayElement::Oid, roles))),
                    )
                    .set(
                        "polqual",
                        condition(snapshot, listed.table_at, listed.policy.using.as_deref()),
                    )
                    .set(
                        "polwithcheck",
                        condition(snapshot, listed.table_at, listed.policy.check.as_deref()),
                    )
                    .done(),
            )
        })
        .collect())
}

fn shown_roles(policy: &Policy) -> Vec<Value> {
    if policy.roles.iter().all(|role| role == crate::roles::PUBLIC) {
        return vec![Value::Text(crate::roles::PUBLIC.to_owned())];
    }
    let mut names: Vec<&String> = policy
        .roles
        .iter()
        .filter(|role| *role != crate::roles::PUBLIC)
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| Value::Text(name.clone()))
        .collect()
}

/// `pg_policies`: the view PostgreSQL defines over `pg_policy`, with names in place of OIDs.
pub(super) fn policies_view_rows(snapshot: &CatalogSnapshot) -> SqlResult<Vec<Vec<Value>>> {
    let columns = MonitoringRelation::Policies.columns();
    Ok(listed_policies(snapshot)?
        .into_iter()
        .filter_map(|listed| {
            let entry = snapshot.relations.get(listed.table_at)?;
            let permissive = if listed.policy.permissive {
                "PERMISSIVE"
            } else {
                "RESTRICTIVE"
            };
            Some(
                CatalogRow::new(columns)
                    .set("schemaname", Value::Text(entry.schema.clone()))
                    .set("tablename", Value::Text(entry.name.clone()))
                    .set("policyname", Value::Text(listed.policy.name.clone()))
                    .set("permissive", Value::Text(permissive.to_owned()))
                    .set(
                        "roles",
                        Value::Array(Box::new(ArrayValue::new(
                            ArrayElement::Name,
                            shown_roles(&listed.policy),
                        ))),
                    )
                    .set(
                        "cmd",
                        Value::Text(command_name(listed.policy.command).to_owned()),
                    )
                    .set(
                        "qual",
                        condition(snapshot, listed.table_at, listed.policy.using.as_deref()),
                    )
                    .set(
                        "with_check",
                        condition(snapshot, listed.table_at, listed.policy.check.as_deref()),
                    )
                    .done(),
            )
        })
        .collect())
}
