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

impl Viewer {
    fn enables(&self, role: &str) -> bool {
        match self {
            Self::Grantor => true,
            Self::EnabledRoles(enabled) => enabled.contains(role),
        }
    }

    fn is_grantor(&self) -> bool {
        matches!(self, Self::Grantor)
    }
}

const DATABASE_OWNER_ROLE: &str = "pg_database_owner";

fn every_role_name(snapshot: &CatalogSnapshot) -> Vec<&str> {
    let stored = |name: &str| snapshot.stored_roles.iter().any(|role| role.name == name);
    let mut names = Vec::new();
    if !stored(&snapshot.user) {
        names.push(snapshot.user.as_str());
    }
    for (_, name) in crate::pg_catalog::PREDEFINED_ROLES {
        if name != snapshot.user && !stored(name) {
            names.push(name);
        }
    }
    names.extend(snapshot.stored_roles.iter().map(|role| role.name.as_str()));
    names
}

/// `enabled_roles`: the current user and every role it belongs to, directly or through other
/// roles; a superuser session uses every role, as PostgreSQL's `pg_has_role` answers it.
pub(super) fn enabled_roles_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let viewer = Viewer::of(snapshot);
    every_role_name(snapshot)
        .into_iter()
        .filter(|name| viewer.enables(name))
        .map(|name| {
            InformationRow::new(InformationRelation::EnabledRoles)
                .set("role_name", identifier(name))
                .done()
        })
        .collect()
}

fn applicable_role_row(grantee: &str, role: &str) -> Vec<Value> {
    InformationRow::new(InformationRelation::ApplicableRoles)
        .set("grantee", identifier(grantee))
        .set("role_name", identifier(role))
        .set("is_grantable", yes_or_no(false))
        .done()
}

/// `applicable_roles`: each membership whose member is an enabled role of the current user, and
/// for a superuser session also the database owner's implicit membership in
/// `pg_database_owner`. No membership carries `ADMIN OPTION`, since only superusers grant roles.
pub(super) fn applicable_roles_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let viewer = Viewer::of(snapshot);
    let mut rows = Vec::new();
    for member in &snapshot.stored_roles {
        if !viewer.enables(&member.name) {
            continue;
        }
        for role in &member.member_of {
            rows.push(applicable_role_row(&member.name, role));
        }
    }
    if viewer.is_grantor() {
        rows.push(applicable_role_row(&snapshot.user, DATABASE_OWNER_ROLE));
    }
    rows
}

struct DefaultGrant<'a> {
    grantee: &'a str,
    is_grantable: bool,
}

fn default_grants<'a>(snapshot: &'a CatalogSnapshot, viewer: &Viewer) -> Vec<DefaultGrant<'a>> {
    let mut grants = vec![DefaultGrant {
        grantee: "PUBLIC",
        is_grantable: false,
    }];
    if viewer.is_grantor() {
        grants.push(DefaultGrant {
            grantee: &snapshot.user,
            is_grantable: true,
        });
    }
    grants
}

/// `routine_privileges`: `EXECUTE` on every routine, which `PUBLIC` holds by PostgreSQL's
/// default and EggDB does not check, and, for a superuser session, the owner's own `EXECUTE`
/// with its grant option, in the order `aclexplode(acldefault('f', proowner))` lists them.
pub(super) fn routine_privileges_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let viewer = Viewer::of(snapshot);
    let grants = default_grants(snapshot, &viewer);
    let mut rows = Vec::new();
    for routine in &snapshot.routines {
        let specific = super::specific_name(routine);
        for grant in &grants {
            rows.push(
                InformationRow::new(InformationRelation::RoutinePrivileges)
                    .set("grantor", identifier(&snapshot.user))
                    .set("grantee", identifier(grant.grantee))
                    .set("specific_catalog", identifier(&snapshot.database))
                    .set("specific_schema", identifier(&routine.schema))
                    .set("specific_name", identifier(&specific))
                    .set("routine_catalog", identifier(&snapshot.database))
                    .set("routine_schema", identifier(&routine.schema))
                    .set("routine_name", identifier(&routine.name))
                    .set("privilege_type", character_data("EXECUTE"))
                    .set("is_grantable", yes_or_no(grant.is_grantable))
                    .done(),
            );
        }
    }
    rows
}

/// `udt_privileges`: `TYPE USAGE` on every composite type, held by `PUBLIC` by PostgreSQL's
/// default and never checked by EggDB, and, for a superuser session, by the owner with its grant
/// option.
pub(super) fn udt_privileges_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let viewer = Viewer::of(snapshot);
    let grants = default_grants(snapshot, &viewer);
    let mut rows = Vec::new();
    for found in snapshot.user_types.types() {
        if !matches!(found.body, crate::user_types::TypeBody::Composite(_)) {
            continue;
        }
        for grant in &grants {
            rows.push(
                InformationRow::new(InformationRelation::UdtPrivileges)
                    .set("grantor", identifier(&snapshot.user))
                    .set("grantee", identifier(grant.grantee))
                    .set("udt_catalog", identifier(&snapshot.database))
                    .set("udt_schema", identifier(crate::schemas::schema_of(&found.name)))
                    .set("udt_name", identifier(crate::schemas::base_name(&found.name)))
                    .set("privilege_type", character_data("TYPE USAGE"))
                    .set("is_grantable", yes_or_no(grant.is_grantable))
                    .done(),
            );
        }
    }
    rows
}
