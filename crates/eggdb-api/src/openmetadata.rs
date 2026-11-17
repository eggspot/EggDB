//! A read-only API in OpenMetadata's shape (D1201) under `/openmetadata/api/v1`, so a data
//! catalog that speaks it (OpenMetadata, Collate, and tools built on its client) lists EggDB's
//! databases, schemas, tables with their columns and glossary tags, and table lineage from
//! `eggdb_catalog` without an ingestion job. Fully qualified names are
//! `eggdb.<database>.<schema>.<table>`, and ids are stable UUIDs made from them.

use crate::request::{literal, percent_decode};
use crate::{Endpoint, failure, json, json_string};
use eggdb_http::{Request, Response};
use eggdb_sql::{KeyValueStore, QueryResult, Value};

const SERVICE: &str = "eggdb";
const VERSION: &str = "1.3.0";

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::Text(text)) => text.clone(),
        _ => String::new(),
    }
}

/// A stable UUID (version 8 layout) for an entity's fully qualified name.
pub(crate) fn entity_id(fqn: &str) -> String {
    let digest = eggdb_crypto::sha256(fqn.as_bytes());
    let hex: String = digest[..16].iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-8{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The OpenMetadata column data type for a SQL type name.
pub(crate) fn data_type(sql_type: &str) -> &'static str {
    let lowered = sql_type.to_ascii_lowercase();
    match lowered.as_str() {
        "smallint" | "int2" => "SMALLINT",
        "integer" | "int" | "int4" => "INT",
        "bigint" | "int8" => "BIGINT",
        "real" | "float4" => "FLOAT",
        "double precision" | "float8" => "DOUBLE",
        "numeric" | "decimal" => "NUMERIC",
        "boolean" | "bool" => "BOOLEAN",
        "date" => "DATE",
        "time without time zone" | "time with time zone" | "time" => "TIME",
        "timestamp without time zone" | "timestamp" => "TIMESTAMP",
        "timestamp with time zone" | "timestamptz" => "TIMESTAMPZ",
        "interval" => "INTERVAL",
        "text" => "TEXT",
        "character varying" | "varchar" => "VARCHAR",
        "character" | "char" | "bpchar" => "CHAR",
        "bytea" => "BYTEA",
        "uuid" => "UUID",
        "json" => "JSON",
        "jsonb" => "JSONB",
        "xml" => "XML",
        "inet" | "cidr" => "INET",
        "geometry" | "geography" => "GEOMETRY",
        "array" => "ARRAY",
        _ => "UNKNOWN",
    }
}

fn reference(kind: &str, fqn: &str, name: &str) -> String {
    format!(
        "{{\"id\":\"{}\",\"type\":\"{kind}\",\"name\":{},\"fullyQualifiedName\":{}}}",
        entity_id(fqn),
        json_string(name),
        json_string(fqn)
    )
}

fn page(items: Vec<String>) -> Response {
    let total = items.len();
    json(
        200,
        format!(
            "{{\"data\":[{}],\"paging\":{{\"total\":{total}}}}}",
            items.join(",")
        ),
    )
}

fn query_value(request: &Request, name: &str) -> Option<String> {
    request
        .query
        .as_deref()?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| percent_decode(value))
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    fn catalog_rows(&self, sql: &str) -> Result<Vec<Vec<Value>>, Response> {
        let failed = |error: eggdb_sql::SqlError| failure(500, error.state().code(), &error.to_string());
        let mut session = self.session().map_err(failed)?;
        let mut results: Vec<QueryResult> = Vec::new();
        session.execute(sql, &mut |result| results.push(result)).map_err(failed)?;
        Ok(results.pop().map(|result| result.rows).unwrap_or_default())
    }

    fn database_fqn(&self) -> String {
        format!("{SERVICE}.{}", self.database)
    }

    /// Answers `openmetadata/api/v1/...` (the path after `openmetadata/`).
    pub(crate) fn openmetadata(&self, path: &str, request: &Request) -> Response {
        if request.method != "GET" {
            return failure(405, "PGRST117", "the OpenMetadata API here is read-only");
        }
        let Some(rest) = path.strip_prefix("api/v1/") else {
            return failure(404, "PGRST125", "OpenMetadata's API is at /openmetadata/api/v1");
        };
        let answered = match rest {
            "system/version" => Ok(json(200, format!("{{\"version\":\"{VERSION}\"}}"))),
            "services/databaseServices" => Ok(page(vec![format!(
                "{{\"id\":\"{}\",\"name\":\"{SERVICE}\",\"fullyQualifiedName\":\"{SERVICE}\",\"serviceType\":\"CustomDatabase\"}}",
                entity_id(SERVICE)
            )])),
            "databases" => Ok(page(vec![self.database_entity()])),
            "databaseSchemas" => self.schema_entities(),
            "tables" => self.table_entities(request),
            _ => match (rest.strip_prefix("tables/name/"), rest.strip_prefix("lineage/table/name/")) {
                (Some(fqn), _) => self.table_by_name(&percent_decode(fqn)),
                (_, Some(fqn)) => self.lineage(&percent_decode(fqn)),
                _ => Err(failure(404, "PGRST125", &format!("no OpenMetadata resource {rest}"))),
            },
        };
        answered.unwrap_or_else(|response| response)
    }

    fn database_entity(&self) -> String {
        let fqn = self.database_fqn();
        format!(
            "{{\"id\":\"{}\",\"name\":{},\"fullyQualifiedName\":{},\"service\":{}}}",
            entity_id(&fqn),
            json_string(&self.database),
            json_string(&fqn),
            reference("databaseService", SERVICE, SERVICE)
        )
    }

    fn schema_entities(&self) -> Result<Response, Response> {
        let rows = self.catalog_rows(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name NOT IN ('pg_catalog', 'information_schema', 'pg_toast') ORDER BY 1",
        )?;
        let database = self.database_fqn();
        Ok(page(
            rows.iter()
                .map(|row| {
                    let name = text_of(row.first());
                    let fqn = format!("{database}.{name}");
                    format!(
                        "{{\"id\":\"{}\",\"name\":{},\"fullyQualifiedName\":{},\"database\":{}}}",
                        entity_id(&fqn),
                        json_string(&name),
                        json_string(&fqn),
                        reference("database", &database, &self.database)
                    )
                })
                .collect(),
        ))
    }

    fn table_entity(&self, schema: &str, table: &str, kind: &str) -> Result<String, Response> {
        let columns = self.catalog_rows(&format!(
            "SELECT column_name, data_type, is_nullable, ordinal_position FROM information_schema.columns WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
            literal(schema),
            literal(table)
        ))?;
        let tags = self
            .catalog_rows(&format!(
                "SELECT column_name, term FROM eggdb_catalog.tags WHERE schema_name = {} AND table_name = {}",
                literal(schema),
                literal(table)
            ))
            .unwrap_or_default();
        let tag_list = |column: &str| -> String {
            let found: Vec<String> = tags
                .iter()
                .filter(|row| text_of(row.first()) == column)
                .map(|row| {
                    let term = text_of(row.get(1));
                    format!(
                        "{{\"tagFQN\":{},\"source\":\"Glossary\",\"labelType\":\"Manual\",\"state\":\"Confirmed\"}}",
                        json_string(&format!("eggdb_catalog.{term}"))
                    )
                })
                .collect();
            format!("[{}]", found.join(","))
        };
        let schema_fqn = format!("{}.{schema}", self.database_fqn());
        let fqn = format!("{schema_fqn}.{table}");
        let rendered: Vec<String> = columns
            .iter()
            .map(|row| {
                let name = text_of(row.first());
                let sql_type = text_of(row.get(1));
                let constraint = if text_of(row.get(2)) == "NO" { "NOT_NULL" } else { "NULL" };
                format!(
                    "{{\"name\":{},\"dataType\":\"{}\",\"dataTypeDisplay\":{},\"constraint\":\"{constraint}\",\"ordinalPosition\":{},\"fullyQualifiedName\":{},\"tags\":{}}}",
                    json_string(&name),
                    data_type(&sql_type),
                    json_string(&sql_type),
                    match row.get(3) {
                        Some(Value::Integer(position)) => position.to_string(),
                        Some(Value::BigInt(position)) => position.to_string(),
                        Some(Value::SmallInt(position)) => position.to_string(),
                        _ => "0".to_owned(),
                    },
                    json_string(&format!("{fqn}.{name}")),
                    tag_list(&name)
                )
            })
            .collect();
        let table_type = if kind == "VIEW" { "View" } else { "Regular" };
        Ok(format!(
            "{{\"id\":\"{}\",\"name\":{},\"fullyQualifiedName\":{},\"tableType\":\"{table_type}\",\"columns\":[{}],\"tags\":{},\"databaseSchema\":{},\"database\":{},\"service\":{}}}",
            entity_id(&fqn),
            json_string(table),
            json_string(&fqn),
            rendered.join(","),
            tag_list(""),
            reference("databaseSchema", &schema_fqn, schema),
            reference("database", &self.database_fqn(), &self.database),
            reference("databaseService", SERVICE, SERVICE)
        ))
    }

    fn table_entities(&self, request: &Request) -> Result<Response, Response> {
        let wanted_schema = query_value(request, "databaseSchema")
            .and_then(|fqn| fqn.rsplit_once('.').map(|(_, schema)| schema.to_owned()));
        let mut sql = "SELECT table_schema, table_name, table_type FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema', 'pg_toast')".to_owned();
        if let Some(schema) = &wanted_schema {
            sql.push_str(&format!(" AND table_schema = {}", literal(schema)));
        }
        sql.push_str(" ORDER BY 1, 2");
        let rows = self.catalog_rows(&sql)?;
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            items.push(self.table_entity(&text_of(row.first()), &text_of(row.get(1)), &text_of(row.get(2)))?);
        }
        Ok(page(items))
    }

    fn split_table_fqn(&self, fqn: &str) -> Result<(String, String), Response> {
        let prefix = format!("{}.", self.database_fqn());
        let missing = || failure(404, "PGRST205", &format!("no table {fqn}"));
        let rest = fqn.strip_prefix(&prefix).ok_or_else(missing)?;
        let (schema, table) = rest.split_once('.').ok_or_else(missing)?;
        Ok((schema.to_owned(), table.to_owned()))
    }

    fn table_by_name(&self, fqn: &str) -> Result<Response, Response> {
        let (schema, table) = self.split_table_fqn(fqn)?;
        let kind = self
            .catalog_rows(&format!(
                "SELECT table_type FROM information_schema.tables WHERE table_schema = {} AND table_name = {}",
                literal(&schema),
                literal(&table)
            ))?
            .first()
            .map(|row| text_of(row.first()))
            .ok_or_else(|| failure(404, "PGRST205", &format!("no table {fqn}")))?;
        Ok(json(200, self.table_entity(&schema, &table, &kind)?))
    }

    fn lineage(&self, fqn: &str) -> Result<Response, Response> {
        let (schema, table) = self.split_table_fqn(fqn)?;
        let asset = format!("{schema}.{table}");
        let edges = self
            .catalog_rows(&format!(
                "SELECT source_asset, target_asset FROM eggdb_catalog.lineage WHERE source_asset = {0} OR target_asset = {0}",
                literal(&asset)
            ))
            .unwrap_or_default();
        let database = self.database_fqn();
        let full = |asset: &str| format!("{database}.{asset}");
        let mut nodes: Vec<String> = Vec::new();
        let mut upstream = Vec::new();
        let mut downstream = Vec::new();
        for row in &edges {
            let (from, to) = (text_of(row.first()), text_of(row.get(1)));
            let edge = format!(
                "{{\"fromEntity\":\"{}\",\"toEntity\":\"{}\"}}",
                entity_id(&full(&from)),
                entity_id(&full(&to))
            );
            let other = if to == asset { &from } else { &to };
            if !nodes.contains(other) {
                nodes.push(other.clone());
            }
            if to == asset {
                upstream.push(edge);
            } else {
                downstream.push(edge);
            }
        }
        let rendered: Vec<String> = nodes
            .iter()
            .map(|node| {
                let name = node.rsplit_once('.').map_or(node.as_str(), |(_, name)| name);
                reference("table", &full(node), name)
            })
            .collect();
        Ok(json(
            200,
            format!(
                "{{\"entity\":{},\"nodes\":[{}],\"upstreamEdges\":[{}],\"downstreamEdges\":[{}]}}",
                reference("table", &full(&asset), &table),
                rendered.join(","),
                upstream.join(","),
                downstream.join(",")
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_uuids_and_types_map() {
        let id = entity_id("eggdb.eggdb.public.customers");
        assert_eq!(id.len(), 36);
        assert_eq!(id, entity_id("eggdb.eggdb.public.customers"));
        assert_eq!(&id[14..15], "8");
        assert_eq!(data_type("integer"), "INT");
        assert_eq!(data_type("timestamp with time zone"), "TIMESTAMPZ");
        assert_eq!(data_type("whatever"), "UNKNOWN");
    }
}
