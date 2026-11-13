//! Placement rules per table and residency per row (D1105): `CREATE EXTENSION eggdb_placement`
//! records, for each table, its storage tier (`hot`, `warm`, `cold` or `any`), whether edge
//! copies carry it, the regions allowed to hold it, and the column naming each row's home
//! region; `eggdb_placement.residency` lists, for each home region, the regions its rows may
//! be stored in. A trigger refuses a write a server's region may not hold (and, while a change
//! from another server is applied, drops the row and logs the refused transfer), and
//! [`filter_for_region`] removes from a copy what a region or an edge may not hold.

use crate::error::SqlResult;
use crate::session::Session;
use crate::store::KeyValueStore;

/// The statements `CREATE EXTENSION eggdb_placement` runs.
pub(crate) const PLACEMENT_SCRIPT: &str = r"
CREATE SCHEMA eggdb_placement;
CREATE TABLE eggdb_placement.settings (
    name text PRIMARY KEY,
    value text NOT NULL
);
INSERT INTO eggdb_placement.settings (name, value) VALUES ('region', '');
CREATE TABLE eggdb_placement.tables (
    table_schema text NOT NULL,
    table_name text NOT NULL,
    tier text NOT NULL,
    edge boolean NOT NULL,
    regions text,
    region_column text,
    PRIMARY KEY (table_schema, table_name)
);
CREATE TABLE eggdb_placement.residency (
    row_region text PRIMARY KEY,
    allowed_regions text NOT NULL
);
CREATE TABLE eggdb_placement.transfers (
    id bigserial PRIMARY KEY,
    at timestamptz NOT NULL DEFAULT now(),
    table_schema text NOT NULL,
    table_name text NOT NULL,
    row_region text,
    server_region text NOT NULL,
    outcome text NOT NULL
);
CREATE FUNCTION eggdb_placement.allowed(home_region text, server_region text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    allowed_list text;
BEGIN
    IF server_region IS NULL OR server_region = '' OR home_region IS NULL THEN
        RETURN true;
    END IF;
    SELECT allowed_regions INTO allowed_list FROM eggdb_placement.residency WHERE row_region = home_region;
    IF allowed_list IS NULL THEN
        RETURN home_region = server_region;
    END IF;
    RETURN server_region = ANY (string_to_array(allowed_list, ','));
END
$$;
CREATE FUNCTION eggdb_placement.table_allowed(target_schema text, target_table text, server_region text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    region_list text;
BEGIN
    IF server_region IS NULL OR server_region = '' THEN
        RETURN true;
    END IF;
    SELECT regions INTO region_list FROM eggdb_placement.tables WHERE table_schema = target_schema AND table_name = target_table;
    IF region_list IS NULL THEN
        RETURN true;
    END IF;
    RETURN server_region = ANY (string_to_array(region_list, ','));
END
$$;
CREATE FUNCTION eggdb_placement.check_row() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    server_region text;
    column_name text;
    row_region text;
BEGIN
    SELECT value INTO server_region FROM eggdb_placement.settings WHERE name = 'region';
    SELECT region_column INTO column_name FROM eggdb_placement.tables WHERE table_schema = TG_TABLE_SCHEMA AND table_name = TG_TABLE_NAME;
    IF column_name IS NOT NULL THEN
        row_region := to_jsonb(NEW) ->> column_name;
    END IF;
    IF eggdb_placement.table_allowed(TG_TABLE_SCHEMA, TG_TABLE_NAME, server_region) AND eggdb_placement.allowed(row_region, server_region) THEN
        RETURN NEW;
    END IF;
    IF coalesce(current_setting('eggdb_mp.applying', true), '') = 'on' THEN
        INSERT INTO eggdb_placement.transfers (table_schema, table_name, row_region, server_region, outcome) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, row_region, server_region, 'refused a row from another server');
        RETURN NULL;
    END IF;
    RAISE EXCEPTION 'placement: a row of %.% whose home region is % may not be stored in region %', TG_TABLE_SCHEMA, TG_TABLE_NAME, coalesce(row_region, '(any)'), server_region;
END
$$;
CREATE FUNCTION eggdb_placement.place(target_schema text, target_table text, tier_name text, on_edges boolean, allowed_regions text, home_column text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    IF tier_name NOT IN ('hot', 'warm', 'cold', 'any') THEN
        RAISE EXCEPTION 'the tier must be hot, warm, cold or any, not %', tier_name;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = target_schema AND table_name = target_table) THEN
        RAISE EXCEPTION 'there is no table %.%', target_schema, target_table;
    END IF;
    IF home_column IS NOT NULL AND NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = target_schema AND table_name = target_table AND column_name = home_column) THEN
        RAISE EXCEPTION 'table %.% has no column %', target_schema, target_table, home_column;
    END IF;
    DELETE FROM eggdb_placement.tables WHERE table_schema = target_schema AND table_name = target_table;
    INSERT INTO eggdb_placement.tables (table_schema, table_name, tier, edge, regions, region_column) VALUES (target_schema, target_table, tier_name, on_edges, allowed_regions, home_column);
    EXECUTE format('DROP TRIGGER IF EXISTS eggdb_placement_check ON %I.%I', target_schema, target_table);
    IF allowed_regions IS NOT NULL OR home_column IS NOT NULL THEN
        EXECUTE format('CREATE TRIGGER eggdb_placement_check BEFORE INSERT OR UPDATE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_placement.check_row()', target_schema, target_table);
    END IF;
    RETURN tier_name;
END
$$;
";

/// What [`filter_for_region`] removed from a copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterReport {
    /// The tables dropped: kept off edges, or not allowed in the region.
    pub dropped_tables: Vec<String>,
    /// The rows deleted because their home region's rows may not be stored in the region.
    pub deleted_rows: u64,
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn rows_of(session: &mut Session<'_>, sql: &str) -> SqlResult<Vec<Vec<Option<String>>>> {
    let mut rows = Vec::new();
    session.execute(sql, &mut |result| {
        rows = result.rows.iter().map(|row| row.iter().map(|value| value.to_text()).collect()).collect();
    })?;
    Ok(rows)
}

/// Removes from a writable copy what `region` may not hold and, when `for_edge`, what edge
/// copies do not carry: the tables marked `edge = false`, the tables whose allowed regions
/// leave `region` out, and the rows whose home region's residency leaves it out. A copy
/// without `eggdb_placement` is left as it is.
pub fn filter_for_region<S: KeyValueStore>(store: &S, region: Option<&str>, for_edge: bool) -> SqlResult<FilterReport> {
    let mut session = Session::with_dialect(store, crate::dialect::Dialect::PostgreSql);
    let installed = rows_of(&mut session, "SELECT 1 FROM information_schema.schemata WHERE schema_name = 'eggdb_placement'")?;
    let mut report = FilterReport::default();
    if installed.is_empty() {
        return Ok(report);
    }
    let rules = rows_of(&mut session, "SELECT table_schema, table_name, edge::text, regions, region_column FROM eggdb_placement.tables ORDER BY table_schema, table_name")?;
    let mut script = String::from("BEGIN;\n");
    for rule in rules {
        let text = |at: usize| rule.get(at).cloned().flatten();
        let (Some(schema), Some(table)) = (text(0), text(1)) else {
            continue;
        };
        let on_edges = text(2).is_none_or(|edge| edge == "true" || edge == "t");
        let allowed_here = match (region, text(3)) {
            (Some(region), Some(list)) => list.split(',').any(|allowed| allowed.trim() == region),
            _ => true,
        };
        let name = format!("{}.{}", quoted(&schema), quoted(&table));
        if (for_edge && !on_edges) || !allowed_here {
            script.push_str(&format!("DROP TABLE IF EXISTS {name};\nDELETE FROM eggdb_placement.tables WHERE table_schema = {} AND table_name = {};\n", literal(&schema), literal(&table)));
            report.dropped_tables.push(format!("{schema}.{table}"));
            continue;
        }
        if let (Some(region), Some(column)) = (region, text(4)) {
            let counted = rows_of(
                &mut session,
                &format!("SELECT count(*) FROM {name} WHERE NOT eggdb_placement.allowed({}::text, {})", quoted(&column), literal(region)),
            )?;
            report.deleted_rows += counted.first().and_then(|row| row.first().cloned().flatten()).and_then(|count| count.parse::<u64>().ok()).unwrap_or(0);
            script.push_str(&format!("DELETE FROM {name} WHERE NOT eggdb_placement.allowed({}::text, {});\n", quoted(&column), literal(region)));
        }
    }
    if let Some(region) = region {
        script.push_str(&format!("UPDATE eggdb_placement.settings SET value = {} WHERE name = 'region';\n", literal(region)));
    }
    script.push_str("COMMIT;\n");
    session.execute(&script, &mut |_| {})?;
    let _ = session.close();
    Ok(report)
}

/// The tables whose rules keep them in a faster tier, `hot` or `warm`, as `(schema, table)`:
/// what a server reads through now and then so their pages stay in memory or on local disk.
pub fn warm_tables<S: KeyValueStore>(store: &S) -> SqlResult<Vec<(String, String)>> {
    let mut session = Session::with_dialect(store, crate::dialect::Dialect::PostgreSql);
    let installed = rows_of(&mut session, "SELECT 1 FROM information_schema.schemata WHERE schema_name = 'eggdb_placement'")?;
    if installed.is_empty() {
        return Ok(Vec::new());
    }
    let rows = rows_of(&mut session, "SELECT table_schema, table_name FROM eggdb_placement.tables WHERE tier IN ('hot', 'warm') ORDER BY tier, table_schema, table_name")?;
    let _ = session.close();
    Ok(rows
        .into_iter()
        .filter_map(|row| Some((row.first()?.clone()?, row.get(1)?.clone()?)))
        .collect())
}

/// Reads every page of each `hot` and `warm` table once, so the pages stay in the page cache and
/// in the local tier of a page store (D1103) as the least recently used ones leave; returns how
/// many tables were read.
pub fn prewarm<S: KeyValueStore>(store: &S) -> SqlResult<usize> {
    let tables = warm_tables(store)?;
    let mut session = Session::with_dialect(store, crate::dialect::Dialect::PostgreSql);
    for (schema, table) in &tables {
        rows_of(&mut session, &format!("SELECT count(*) FROM {}.{}", quoted(schema), quoted(table)))?;
    }
    let _ = session.close();
    Ok(tables.len())
}
