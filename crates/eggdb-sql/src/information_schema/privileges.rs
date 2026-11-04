use super::{InformationRelation, InformationRow, character_data, identifier, yes_or_no};
use crate::pg_catalog::{CatalogSnapshot, RelationEntry, RelationSource};
use crate::roles::{COLUMN_PRIVILEGES, PUBLIC, Privilege};
use crate::types::Value;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shown {
    WithPublic,
    RolesOnly,
}

enum Viewer {
    Grantor,
    EnabledRoles(BTreeSet<String>),
}

impl Viewer {
    fn of(snapshot: &CatalogSnapshot) -> Self {
        match snapshot.settings.checked_role.as_deref() {
            None => Self::Grantor,
            Some(role) => Self::EnabledRoles(
                crate::roles::role_closure(&snapshot.access, role).unwrap_or_default(),
            ),
        }
    }

    fn sees(&self, grantee: &str, shown: Shown) -> bool {
        match self {
            Self::Grantor => true,
            Self::EnabledRoles(_) if grantee == PUBLIC => shown == Shown::WithPublic,
            Self::EnabledRoles(enabled) => enabled.contains(grantee),
        }
    }
}

fn shown_grantee(grantee: &str) -> &str {
    if grantee == PUBLIC { "PUBLIC" } else { grantee }
}

fn privilege_name(privilege: Privilege) -> String {
    privilege.keyword().to_ascii_uppercase()
}

fn relations_by_internal_name(snapshot: &CatalogSnapshot) -> HashMap<&str, &RelationEntry> {
    snapshot
        .relations
        .iter()
        .filter(|entry| {
            matches!(
                entry.source,
                RelationSource::Table(_) | RelationSource::View(_)
            )
        })
        .map(|entry| (entry.internal.as_str(), entry))
        .collect()
}

fn relation_column_names(entry: &RelationEntry) -> Vec<&str> {
    match &entry.source {
        RelationSource::Table(table) => table
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .filter(|name| !crate::temp_tables::is_hidden_column(name))
            .collect(),
        RelationSource::View(view) => view
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect(),
        _ => Vec::new(),
    }
}

/// `table_privileges` (with `PUBLIC`'s grants) and `role_table_grants` (without): each table
/// grant, one row per privilege, that names an enabled role of the current user as grantee,
/// as the SQL standard's views select them; a superuser session is the grantor of every grant
/// and sees them all.
pub(super) fn table_privileges_rows(snapshot: &CatalogSnapshot, shown: Shown) -> Vec<Vec<Value>> {
    let relation = match shown {
        Shown::WithPublic => InformationRelation::TablePrivileges,
        Shown::RolesOnly => InformationRelation::RoleTableGrants,
    };
    let viewer = Viewer::of(snapshot);
    let entries = relations_by_internal_name(snapshot);
    let mut rows = Vec::new();
    for grant in &snapshot.grants {
        if !viewer.sees(&grant.grantee, shown) {
            continue;
        }
        let Some(entry) = entries.get(grant.table.as_str()) else {
            continue;
        };
        for privilege in Privilege::ALL {
            if grant.privileges & privilege.bit() == 0 {
                continue;
            }
            rows.push(
                InformationRow::new(relation)
                    .set("grantor", identifier(&snapshot.user))
                    .set("grantee", identifier(shown_grantee(&grant.grantee)))
                    .set("table_catalog", identifier(&snapshot.database))
                    .set("table_schema", identifier(&entry.schema))
                    .set("table_name", identifier(&entry.name))
                    .set("privilege_type", character_data(&privilege_name(privilege)))
                    .set("is_grantable", yes_or_no(false))
                    .set("with_hierarchy", yes_or_no(privilege == Privilege::Select))
                    .done(),
            );
        }
    }
    rows
}

struct ColumnRow<'a> {
    entry: &'a RelationEntry,
    grantee: &'a str,
    column: &'a str,
    privilege: Privilege,
}

fn column_row(
    snapshot: &CatalogSnapshot,
    relation: InformationRelation,
    row: ColumnRow<'_>,
) -> Vec<Value> {
    InformationRow::new(relation)
        .set("grantor", identifier(&snapshot.user))
        .set("grantee", identifier(shown_grantee(row.grantee)))
        .set("table_catalog", identifier(&snapshot.database))
        .set("table_schema", identifier(&row.entry.schema))
        .set("table_name", identifier(&row.entry.name))
        .set("column_name", identifier(row.column))
        .set(
            "privilege_type",
            character_data(&privilege_name(row.privilege)),
        )
        .set("is_grantable", yes_or_no(false))
        .done()
}

/// `column_privileges` (with `PUBLIC`'s grants) and `role_column_grants` (without): each
/// column grant, and each table grant of `SELECT`, `INSERT`, `UPDATE` or `REFERENCES` spread
/// over the table's columns, whose grantee is an enabled role of the current user.
pub(super) fn column_privileges_rows(snapshot: &CatalogSnapshot, shown: Shown) -> Vec<Vec<Value>> {
    let relation = match shown {
        Shown::WithPublic => InformationRelation::ColumnPrivileges,
        Shown::RolesOnly => InformationRelation::RoleColumnGrants,
    };
    let viewer = Viewer::of(snapshot);
    let entries = relations_by_internal_name(snapshot);
    let mut rows = Vec::new();
    for grant in &snapshot.grants {
        if !viewer.sees(&grant.grantee, shown) {
            continue;
        }
        let Some(entry) = entries.get(grant.table.as_str()) else {
            continue;
        };
        for column in relation_column_names(entry) {
            for privilege in COLUMN_PRIVILEGES {
                if grant.privileges & privilege.bit() != 0 {
                    rows.push(column_row(
                        snapshot,
                        relation,
                        ColumnRow {
                            entry,
                            grantee: &grant.grantee,
                            column,
                            privilege,
                        },
                    ));
                }
            }
        }
    }
    for grant in &snapshot.column_grants {
        if !viewer.sees(&grant.grantee, shown) {
            continue;
        }
        let Some(entry) = entries.get(grant.table.as_str()) else {
            continue;
        };
        for privilege in COLUMN_PRIVILEGES {
            if grant.holds(privilege) {
                rows.push(column_row(
                    snapshot,
                    relation,
                    ColumnRow {
                        entry,
                        grantee: &grant.grantee,
                        column: &grant.column,
                        privilege,
                    },
                ));
            }
        }
    }
    rows
}

fn usage_row(
    snapshot: &CatalogSnapshot,
    schema: &str,
    name: &str,
    object_type: &str,
) -> Vec<Value> {
    InformationRow::new(InformationRelation::UsagePrivileges)
        .set("grantor", identifier(&snapshot.user))
        .set("grantee", identifier("PUBLIC"))
        .set("object_catalog", identifier(&snapshot.database))
        .set("object_schema", identifier(schema))
        .set("object_name", identifier(name))
        .set("object_type", character_data(object_type))
        .set("privilege_type", character_data("USAGE"))
        .set("is_grantable", yes_or_no(false))
        .done()
}

/// `usage_privileges`: `USAGE` held by `PUBLIC` on every domain and every sequence, since no
/// role is refused either until their privileges are stored (PostgreSQL grants `USAGE` on a
/// domain to `PUBLIC` by default too).
pub(super) fn usage_privileges_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for found in snapshot.user_types.types() {
        if matches!(found.body, crate::user_types::TypeBody::Domain(_)) {
            rows.push(usage_row(
                snapshot,
                crate::schemas::schema_of(&found.name),
                crate::schemas::base_name(&found.name),
                "DOMAIN",
            ));
        }
    }
    for entry in &snapshot.relations {
        if matches!(entry.source, RelationSource::Sequence(_)) {
            rows.push(usage_row(snapshot, &entry.schema, &entry.name, "SEQUENCE"));
        }
    }
    rows
}
