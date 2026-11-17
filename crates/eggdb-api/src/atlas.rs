//! A read-only API in Apache Atlas's shape (D1202) under `/atlas/api/atlas/v2`: basic search
//! for tables and columns, an entity by its guid with its columns, and a table's lineage, so a
//! catalog that reads Atlas (Atlas itself, Purview's Atlas API clients) finds EggDB's tables.
//! Entities use Atlas's `rdbms_table` and `rdbms_column` types; qualified names are
//! `<database>.<schema>.<table>@eggdb`, and guids are D1201's stable UUIDs.

use crate::openmetadata::entity_id;
use crate::request::{literal, percent_decode};
use crate::{Endpoint, failure, json, json_string};
use eggdb_http::{Request, Response};
use eggdb_sql::{KeyValueStore, Value};

const CLUSTER: &str = "eggdb";

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::Text(text)) => text.clone(),
        _ => String::new(),
    }
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

struct Table {
    schema: String,
    name: String,
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    fn atlas_rows(&self, sql: &str) -> Result<Vec<Vec<Value>>, Response> {
        let failed = |error: eggdb_sql::SqlError| failure(500, error.state().code(), &error.to_string());
        let mut session = self.session().map_err(failed)?;
        let mut last = Vec::new();
        session.execute(sql, &mut |result| last = result.rows).map_err(failed)?;
        Ok(last)
    }

    fn atlas_name(&self, table: &Table) -> String {
        format!("{}.{}.{}@{CLUSTER}", self.database, table.schema, table.name)
    }

    fn atlas_tables(&self) -> Result<Vec<Table>, Response> {
        Ok(self
            .atlas_rows("SELECT table_schema, table_name FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema', 'pg_toast') ORDER BY 1, 2")?
            .iter()
            .map(|row| Table {
                schema: text_of(row.first()),
                name: text_of(row.get(1)),
            })
            .collect())
    }

    fn atlas_header(&self, table: &Table) -> String {
        let qualified = self.atlas_name(table);
        format!(
            "{{\"guid\":\"{}\",\"typeName\":\"rdbms_table\",\"status\":\"ACTIVE\",\"displayText\":{},\"attributes\":{{\"qualifiedName\":{},\"name\":{}}}}}",
            entity_id(&qualified),
            json_string(&table.name),
            json_string(&qualified),
            json_string(&table.name)
        )
    }

    /// Answers `atlas/api/atlas/v2/...` (the path after `atlas/`).
    pub(crate) fn atlas(&self, path: &str, request: &Request) -> Response {
        if request.method != "GET" {
            return failure(405, "PGRST117", "the Atlas API here is read-only");
        }
        let Some(rest) = path.strip_prefix("api/atlas/v2/") else {
            return failure(404, "PGRST125", "Atlas's API is at /atlas/api/atlas/v2");
        };
        let answered = match rest {
            "search/basic" => self.atlas_search(request),
            _ => match (rest.strip_prefix("entity/guid/"), rest.strip_prefix("lineage/")) {
                (Some(guid), _) => self.atlas_entity(guid),
                (_, Some(guid)) => self.atlas_lineage(guid),
                _ => Err(failure(404, "PGRST125", &format!("no Atlas resource {rest}"))),
            },
        };
        answered.unwrap_or_else(|response| response)
    }

    fn atlas_search(&self, request: &Request) -> Result<Response, Response> {
        let type_name = query_value(request, "typeName").unwrap_or_else(|| "rdbms_table".to_owned());
        if type_name != "rdbms_table" {
            return Ok(json(200, "{\"queryType\":\"BASIC\",\"entities\":[]}".to_owned()));
        }
        let wanted = query_value(request, "query").map(|text| text.to_ascii_lowercase());
        let found: Vec<String> = self
            .atlas_tables()?
            .iter()
            .filter(|table| {
                wanted
                    .as_deref()
                    .is_none_or(|wanted| wanted == "*" || table.name.to_ascii_lowercase().contains(wanted))
            })
            .map(|table| self.atlas_header(table))
            .collect();
        Ok(json(
            200,
            format!("{{\"queryType\":\"BASIC\",\"entities\":[{}]}}", found.join(",")),
        ))
    }

    fn atlas_table_of(&self, guid: &str) -> Result<Table, Response> {
        self.atlas_tables()?
            .into_iter()
            .find(|table| entity_id(&self.atlas_name(table)) == guid)
            .ok_or_else(|| failure(404, "ATLAS-404-00-005", &format!("no entity with guid {guid}")))
    }

    fn atlas_entity(&self, guid: &str) -> Result<Response, Response> {
        let table = self.atlas_table_of(guid)?;
        let qualified = self.atlas_name(&table);
        let columns = self.atlas_rows(&format!(
            "SELECT column_name, data_type FROM information_schema.columns WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
            literal(&table.schema),
            literal(&table.name)
        ))?;
        let mut references = Vec::new();
        let mut referred = Vec::new();
        for row in &columns {
            let name = text_of(row.first());
            let column_name = format!("{}.{}.{}.{name}@{CLUSTER}", self.database, table.schema, table.name);
            let column_guid = entity_id(&column_name);
            references.push(format!("{{\"guid\":\"{column_guid}\",\"typeName\":\"rdbms_column\"}}"));
            referred.push(format!(
                "\"{column_guid}\":{{\"guid\":\"{column_guid}\",\"typeName\":\"rdbms_column\",\"status\":\"ACTIVE\",\"attributes\":{{\"qualifiedName\":{},\"name\":{},\"data_type\":{},\"table\":{{\"guid\":\"{guid}\",\"typeName\":\"rdbms_table\"}}}}}}",
                json_string(&column_name),
                json_string(&name),
                json_string(&text_of(row.get(1)))
            ));
        }
        Ok(json(
            200,
            format!(
                "{{\"entity\":{{\"guid\":\"{guid}\",\"typeName\":\"rdbms_table\",\"status\":\"ACTIVE\",\"attributes\":{{\"qualifiedName\":{},\"name\":{},\"columns\":[{}]}}}},\"referredEntities\":{{{}}}}}",
                json_string(&qualified),
                json_string(&table.name),
                references.join(","),
                referred.join(",")
            ),
        ))
    }

    fn atlas_lineage(&self, guid: &str) -> Result<Response, Response> {
        let table = self.atlas_table_of(guid)?;
        let asset = format!("{}.{}", table.schema, table.name);
        let edges = self
            .atlas_rows(&format!(
                "SELECT source_asset, target_asset FROM eggdb_catalog.lineage WHERE source_asset = {0} OR target_asset = {0}",
                literal(&asset)
            ))
            .unwrap_or_default();
        let guid_of = |asset: &str| entity_id(&format!("{}.{asset}@{CLUSTER}", self.database));
        let relations: Vec<String> = edges
            .iter()
            .map(|row| {
                format!(
                    "{{\"fromEntityId\":\"{}\",\"toEntityId\":\"{}\"}}",
                    guid_of(&text_of(row.first())),
                    guid_of(&text_of(row.get(1)))
                )
            })
            .collect();
        Ok(json(
            200,
            format!(
                "{{\"baseEntityGuid\":\"{guid}\",\"lineageDirection\":\"BOTH\",\"lineageDepth\":1,\"relations\":[{}],\"guidEntityMap\":{{}}}}",
                relations.join(",")
            ),
        ))
    }
}
