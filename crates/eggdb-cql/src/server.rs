use crate::cql::{Operator, Selection, Statement, Term, parse};
use crate::frame::{
    AUTH_RESPONSE, AUTH_SUCCESS, AUTHENTICATE, BATCH, Body, ERROR, EXECUTE, OPTIONS, Out, PREPARE, QUERY, READY, REGISTER, RESULT, Request, STARTUP, SUPPORTED, VERSION,
    response, take_request,
};
use crate::translate::{Catalog, Outcome, TableInfo, literal, translate};
use crate::types::{CqlType, encode_value, parameter_text};
use eggdb_sql::{Dialect, KeyValueStore, QueryResult, Session, SqlError, Value};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const MAX_PREPARED: usize = 10_000;
const SYSTEM_KEYSPACES: [&str; 2] = ["system", "system_schema"];

const SERVER_ERROR: i32 = 0x0000;
const PROTOCOL_ERROR: i32 = 0x000a;
const BAD_CREDENTIALS: i32 = 0x0100;
const SYNTAX_ERROR: i32 = 0x2000;
const INVALID: i32 = 0x2200;
const ALREADY_EXISTS: i32 = 0x2400;
const UNPREPARED: i32 = 0x2500;

/// How the CQL server runs.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The user clients log in as with Cassandra's password authenticator.
    pub user: String,
    /// The SHA-256 of that user's password.
    pub password_sha256: [u8; 32],
    /// The database sessions run in, as the SQL listeners name it.
    pub database: String,
    /// The most connections served at once.
    pub max_connections: usize,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig").field("user", &self.user).finish_non_exhaustive()
    }
}

struct Failure {
    code: i32,
    message: String,
    extra: Vec<u8>,
}

fn failure(code: i32, message: impl Into<String>) -> Failure {
    Failure { code, message: message.into(), extra: Vec::new() }
}

fn invalid(message: String) -> Failure {
    failure(INVALID, message)
}

fn from_sql(error: &SqlError, keyspace: &str, table: &str) -> Failure {
    match error.state().code() {
        "42601" => failure(SYNTAX_ERROR, error.message()),
        "42P06" | "42P07" => {
            let mut extra = Out::default();
            extra.string(keyspace).string(if error.state().code() == "42P07" { table } else { "" });
            Failure { code: ALREADY_EXISTS, message: error.message().to_owned(), extra: extra.bytes }
        }
        _ => failure(INVALID, error.message()),
    }
}

#[derive(Clone, Copy)]
enum ColumnKind {
    Simple(CqlType),
    Set(CqlType),
    Map(CqlType, CqlType),
}

fn write_kind(out: &mut Out, kind: ColumnKind) {
    match kind {
        ColumnKind::Simple(cql) => {
            out.short(cql.id());
        }
        ColumnKind::Set(element) => {
            out.short(0x0022).short(element.id());
        }
        ColumnKind::Map(key, value) => {
            out.short(0x0021).short(key.id()).short(value.id());
        }
    }
}

type Cell = Option<Vec<u8>>;

fn rows_result(keyspace: &str, table: &str, columns: &[(String, ColumnKind)], rows: &[Vec<Cell>]) -> Vec<u8> {
    let mut out = Out::default();
    out.int(2).int(1).int(i32::try_from(columns.len()).unwrap_or(i32::MAX)).string(keyspace).string(table);
    for (name, kind) in columns {
        out.string(name);
        write_kind(&mut out, *kind);
    }
    out.int(i32::try_from(rows.len()).unwrap_or(i32::MAX));
    for row in rows {
        for cell in row {
            out.bytes(cell.as_deref());
        }
    }
    out.bytes
}

fn text(value: &str) -> Cell {
    Some(value.as_bytes().to_vec())
}

fn collection(items: &[&[u8]]) -> Cell {
    let mut out = Out::default();
    out.int(i32::try_from(items.len()).unwrap_or(i32::MAX));
    for item in items {
        out.bytes(Some(item));
    }
    Some(out.bytes)
}

fn inet(address: IpAddr) -> Cell {
    Some(match address {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    })
}

fn fixed_uuid(seed: &str) -> [u8; 16] {
    let digest = eggdb_crypto::sha256(seed.as_bytes());
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&digest[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    uuid
}

struct Prepared {
    text: String,
    keyspace: Option<String>,
}

struct Connection<'c, 's, S: KeyValueStore + 's> {
    config: &'c ServerConfig,
    session: Session<'s, S>,
    address: IpAddr,
    started: bool,
    authenticated: bool,
    keyspace: Option<String>,
    prepared: BTreeMap<Vec<u8>, Prepared>,
    tables: BTreeMap<(String, String), TableInfo>,
}

fn run_sql<S: KeyValueStore>(session: &mut Session<'_, S>, sql: &str) -> Result<Vec<QueryResult>, SqlError> {
    let mut results = Vec::new();
    session.execute(sql, &mut |result| results.push(result))?;
    Ok(results)
}

fn text_rows<S: KeyValueStore>(session: &mut Session<'_, S>, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let results = run_sql(session, sql).map_err(|error| error.message().to_owned())?;
    Ok(results
        .into_iter()
        .filter(|result| result.columns.is_some())
        .last()
        .map(|result| result.rows.iter().map(|row| row.iter().map(|value| value.to_text().unwrap_or_default()).collect()).collect())
        .unwrap_or_default())
}

struct SessionCatalog<'c, 's, S: KeyValueStore + 's> {
    session: &'c mut Session<'s, S>,
    tables: &'c mut BTreeMap<(String, String), TableInfo>,
}

fn lines(text: &str) -> Vec<String> {
    text.split('\n').filter(|line| !line.is_empty()).map(str::to_owned).collect()
}

impl<S: KeyValueStore> Catalog for SessionCatalog<'_, '_, S> {
    fn table(&mut self, keyspace: &str, table: &str) -> Result<TableInfo, String> {
        let key = (keyspace.to_owned(), table.to_owned());
        if let Some(found) = self.tables.get(&key) {
            return Ok(found.clone());
        }
        let (schema, name) = (literal(keyspace), literal(table));
        let columns = text_rows(
            self.session,
            &format!("SELECT column_name, data_type FROM information_schema.columns WHERE table_schema = {schema} AND table_name = {name} ORDER BY ordinal_position"),
        )?;
        if columns.is_empty() {
            return Err(format!("unconfigured table {table}"));
        }
        let mut columns: Vec<(String, CqlType)> = columns.into_iter().filter_map(|row| Some((row.first()?.clone(), CqlType::from_data_type(row.get(1)?)))).collect();
        let keys: Vec<String> = text_rows(
            self.session,
            &format!(
                "SELECT k.column_name FROM information_schema.table_constraints c JOIN information_schema.key_column_usage k ON k.constraint_name = c.constraint_name AND k.table_schema = c.table_schema AND k.table_name = c.table_name WHERE c.constraint_type = 'PRIMARY KEY' AND c.table_schema = {schema} AND c.table_name = {name} ORDER BY k.ordinal_position"
            ),
        )?
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect();
        let described = text_rows(
            self.session,
            &format!("SELECT partition_key, clustering_key, descending, column_types FROM eggdb_cql.tables WHERE keyspace_name = {schema} AND table_name = {name}"),
        )
        .unwrap_or_default();
        let info = match described.into_iter().next() {
            Some(row) if row.len() == 4 => {
                for entry in lines(&row[3]) {
                    if let Some((column, type_name)) = entry.split_once('\t')
                        && let Some(cql) = CqlType::from_name(type_name)
                        && let Some(found) = columns.iter_mut().find(|(name, _)| name == column)
                    {
                        found.1 = cql;
                    }
                }
                TableInfo { keyspace: keyspace.to_owned(), table: table.to_owned(), columns, partition: lines(&row[0]), clustering: lines(&row[1]), descending: lines(&row[2]) }
            }
            _ => {
                let partition = keys.first().cloned().into_iter().collect();
                let clustering = keys.iter().skip(1).cloned().collect();
                TableInfo { keyspace: keyspace.to_owned(), table: table.to_owned(), columns, partition, clustering, descending: Vec::new() }
            }
        };
        self.tables.insert(key, info.clone());
        Ok(info)
    }
}

fn read_values(body: &mut Body<'_>) -> Result<Vec<Cell>, String> {
    let count = body.short()?;
    (0..count).map(|_| body.bytes()).collect()
}

fn query_parameters(body: &mut Body<'_>) -> Result<Vec<Cell>, String> {
    body.short()?;
    let flags = body.byte()?;
    if flags & 0x40 != 0 {
        return Err("named bind values are not supported yet".to_owned());
    }
    let values = if flags & 0x01 != 0 { read_values(body)? } else { Vec::new() };
    if flags & 0x04 != 0 {
        body.int()?;
    }
    if flags & 0x08 != 0 {
        body.bytes()?;
    }
    if flags & 0x10 != 0 {
        body.short()?;
    }
    if flags & 0x20 != 0 {
        body.long()?;
    }
    Ok(values)
}

fn prepared_id(keyspace: Option<&str>, text: &str) -> Vec<u8> {
    eggdb_crypto::sha256(format!("{}\0{text}", keyspace.unwrap_or_default()).as_bytes())[..16].to_vec()
}

fn system_target(statement: &Statement, current: Option<&str>) -> Option<(String, String)> {
    let Statement::Select { table, .. } = statement else {
        return None;
    };
    let keyspace = table.keyspace.as_deref().or(current)?;
    SYSTEM_KEYSPACES.contains(&keyspace).then(|| (keyspace.to_owned(), table.table.clone()))
}

impl<'s, S: KeyValueStore> Connection<'_, 's, S> {
    fn catalog(&mut self) -> SessionCatalog<'_, 's, S> {
        SessionCatalog { session: &mut self.session, tables: &mut self.tables }
    }

    fn handshake(&mut self, request: &Request) -> Result<(u8, Vec<u8>), Failure> {
        let mut body = Body::new(&request.body);
        match request.opcode {
            OPTIONS => {
                let mut out = Out::default();
                out.string_multimap(&[("CQL_VERSION", &["3.4.5"]), ("COMPRESSION", &[]), ("PROTOCOL_VERSIONS", &["4/v4"])]);
                Ok((SUPPORTED, out.bytes))
            }
            STARTUP => {
                let options = body.string_map().map_err(|message| failure(PROTOCOL_ERROR, message))?;
                if options.iter().any(|(name, _)| name == "COMPRESSION") {
                    return Err(failure(PROTOCOL_ERROR, "compression is not supported yet"));
                }
                self.started = true;
                let mut out = Out::default();
                out.string("org.apache.cassandra.auth.PasswordAuthenticator");
                Ok((AUTHENTICATE, out.bytes))
            }
            AUTH_RESPONSE if self.started => {
                let token = body.bytes().map_err(|message| failure(PROTOCOL_ERROR, message))?.unwrap_or_default();
                let mut parts = token.split(|&byte| byte == 0).skip(1);
                let (user, password) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
                let authorized = eggdb_crypto::constant_time_eq(user, self.config.user.as_bytes())
                    && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password), &self.config.password_sha256);
                if !authorized {
                    return Err(failure(BAD_CREDENTIALS, "Provided username and/or password are incorrect"));
                }
                self.authenticated = true;
                let mut out = Out::default();
                out.bytes(None);
                Ok((AUTH_SUCCESS, out.bytes))
            }
            _ => Err(failure(PROTOCOL_ERROR, "the connection must STARTUP and authenticate first")),
        }
    }

    fn answer(&mut self, request: &Request) -> Result<(u8, Vec<u8>), Failure> {
        if !self.authenticated || matches!(request.opcode, OPTIONS | STARTUP | AUTH_RESPONSE) {
            return self.handshake(request);
        }
        let mut body = Body::new(&request.body);
        let protocol = |message: String| failure(PROTOCOL_ERROR, message);
        match request.opcode {
            REGISTER => Ok((READY, Vec::new())),
            QUERY => {
                let text = body.long_string().map_err(protocol)?;
                let values = query_parameters(&mut body).map_err(protocol)?;
                let current = self.keyspace.clone();
                Ok((RESULT, self.run_text(&text, current.as_deref(), &values)?))
            }
            PREPARE => {
                let text = body.long_string().map_err(protocol)?;
                Ok((RESULT, self.prepare(&text)?))
            }
            EXECUTE => {
                let id = body.short_bytes().map_err(protocol)?;
                let values = query_parameters(&mut body).map_err(protocol)?;
                let (text, keyspace) = self.prepared_text(&id)?;
                Ok((RESULT, self.run_text(&text, keyspace.as_deref(), &values)?))
            }
            BATCH => Ok((RESULT, self.batch(&mut body)?)),
            other => Err(failure(PROTOCOL_ERROR, format!("opcode 0x{other:02x} is not a request"))),
        }
    }

    fn prepared_text(&self, id: &[u8]) -> Result<(String, Option<String>), Failure> {
        match self.prepared.get(id) {
            Some(prepared) => Ok((prepared.text.clone(), prepared.keyspace.clone())),
            None => {
                let mut extra = Out::default();
                extra.short_bytes(id);
                Err(Failure { code: UNPREPARED, message: "Prepared query with ID not found".to_owned(), extra: extra.bytes })
            }
        }
    }

    fn prepare(&mut self, text: &str) -> Result<Vec<u8>, Failure> {
        let (statement, count) = parse(text).map_err(|message| failure(SYNTAX_ERROR, message))?;
        let current = self.keyspace.clone();
        let (keyspace, table, markers) = match system_target(&statement, current.as_deref()) {
            Some((keyspace, table)) => (keyspace, table, vec![(String::new(), CqlType::Varchar); count]),
            None => {
                let translation = translate(&statement, count, current.as_deref(), &mut self.catalog()).map_err(invalid)?;
                let markers = translation.markers.iter().map(|marker| (marker.name.clone(), marker.cql)).collect();
                (translation.keyspace, translation.table, markers)
            }
        };
        let id = prepared_id(current.as_deref(), text);
        if self.prepared.len() >= MAX_PREPARED {
            self.prepared.clear();
        }
        self.prepared.insert(id.clone(), Prepared { text: text.to_owned(), keyspace: current });
        let mut out = Out::default();
        out.int(4).short_bytes(&id).int(1).int(i32::try_from(markers.len()).unwrap_or(i32::MAX)).int(0).string(&keyspace).string(&table);
        for (name, cql) in &markers {
            out.string(name).short(cql.id());
        }
        out.int(4).int(0);
        Ok(out.bytes)
    }

    fn batch(&mut self, body: &mut Body<'_>) -> Result<Vec<u8>, Failure> {
        let protocol = |message: String| failure(PROTOCOL_ERROR, message);
        body.byte().map_err(protocol)?;
        let count = body.short().map_err(protocol)?;
        let mut children = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let child = match body.byte().map_err(protocol)? {
                0 => (body.long_string().map_err(protocol)?, self.keyspace.clone()),
                1 => {
                    let id = body.short_bytes().map_err(protocol)?;
                    self.prepared_text(&id)?
                }
                other => return Err(failure(PROTOCOL_ERROR, format!("batch statement kind {other} is unknown"))),
            };
            children.push((child, read_values(body).map_err(protocol)?));
        }
        run_sql(&mut self.session, "BEGIN").map_err(|error| from_sql(&error, "", ""))?;
        for ((text, keyspace), values) in children {
            let outcome = parse(&text).map_err(|message| failure(SYNTAX_ERROR, message)).and_then(|(statement, markers)| {
                if !matches!(statement, Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. }) {
                    return Err(invalid("only INSERT, UPDATE and DELETE statements are allowed in a batch".to_owned()));
                }
                self.run(&statement, markers, keyspace.as_deref(), &values)
            });
            if let Err(error) = outcome {
                let _ = run_sql(&mut self.session, "ROLLBACK");
                return Err(error);
            }
        }
        run_sql(&mut self.session, "COMMIT").map_err(|error| from_sql(&error, "", ""))?;
        Ok(1i32.to_be_bytes().to_vec())
    }

    fn run_text(&mut self, text: &str, current: Option<&str>, values: &[Cell]) -> Result<Vec<u8>, Failure> {
        let (statement, markers) = parse(text).map_err(|message| failure(SYNTAX_ERROR, message))?;
        self.run(&statement, markers, current, values)
    }

    fn run(&mut self, statement: &Statement, markers: usize, current: Option<&str>, values: &[Cell]) -> Result<Vec<u8>, Failure> {
        if values.len() != markers {
            return Err(invalid(format!("There were {markers} markers(?) in CQL but {} bound variables", values.len())));
        }
        if let Some((keyspace, table)) = system_target(statement, current) {
            return self.system(&keyspace, &table, statement, values);
        }
        if let Statement::Use(keyspace) = statement {
            let known = SYSTEM_KEYSPACES.contains(&keyspace.as_str())
                || !text_rows(&mut self.session, &format!("SELECT 1 FROM information_schema.schemata WHERE schema_name = {}", literal(keyspace))).map_err(invalid)?.is_empty();
            if !known {
                return Err(invalid(format!("Keyspace '{keyspace}' does not exist")));
            }
            self.keyspace = Some(keyspace.clone());
            let mut out = Out::default();
            out.int(3).string(keyspace);
            return Ok(out.bytes);
        }
        let translation = translate(statement, markers, current, &mut self.catalog()).map_err(invalid)?;
        let texts = values
            .iter()
            .zip(&translation.markers)
            .map(|(value, marker)| parameter_text(value.as_deref(), marker.cql))
            .collect::<Result<Vec<_>, _>>()
            .map_err(invalid)?;
        let sql = translation.render(&texts);
        let results = if translation.is_empty() { Ok(Vec::new()) } else { run_sql(&mut self.session, &sql) };
        if matches!(translation.outcome, Outcome::SchemaChange { .. }) {
            self.tables.clear();
        }
        let results = results.map_err(|error| {
            if matches!(statement, Statement::Batch(_)) {
                let _ = run_sql(&mut self.session, "ROLLBACK");
            }
            from_sql(&error, &translation.keyspace, &translation.table)
        })?;
        let mut out = Out::default();
        match translation.outcome {
            Outcome::Void => {
                out.int(1);
            }
            Outcome::SetKeyspace(keyspace) => {
                out.int(3).string(&keyspace);
            }
            Outcome::SchemaChange { change, target, keyspace, name } => {
                out.int(5).string(change).string(target).string(&keyspace);
                if let Some(name) = name {
                    out.string(&name);
                }
            }
            Outcome::Applied(keyspace, table) => {
                let applied = results.iter().any(|result| !result.rows.is_empty());
                return Ok(rows_result(&keyspace, &table, &[("[applied]".to_owned(), ColumnKind::Simple(CqlType::Boolean))], &[vec![Some(vec![u8::from(applied)])]]));
            }
            Outcome::Rows(info) => {
                let Some(result) = results.into_iter().rev().find(|result| result.columns.is_some()) else {
                    return Err(failure(SERVER_ERROR, "the query returned no rows result"));
                };
                let columns = result.columns.unwrap_or_default();
                let kinds: Vec<CqlType> = columns
                    .iter()
                    .map(|column| if column.name == "count" && column.sql_type == Some(eggdb_sql::SqlType::BigInt) { CqlType::BigInt } else { info.column_type(&column.name).unwrap_or_else(|| CqlType::from_sql_type(column.sql_type.as_ref())) })
                    .collect();
                let mut rows = Vec::with_capacity(result.rows.len());
                for row in &result.rows {
                    let cells = row.iter().zip(&kinds).map(|(value, cql)| encode_value(value, *cql)).collect::<Result<Vec<_>, _>>().map_err(|message| failure(SERVER_ERROR, message))?;
                    rows.push(cells);
                }
                let described: Vec<(String, ColumnKind)> = columns.iter().zip(&kinds).map(|(column, cql)| (column.name.clone(), ColumnKind::Simple(*cql))).collect();
                return Ok(rows_result(&info.keyspace, &info.table, &described, &rows));
            }
        }
        Ok(out.bytes)
    }

    fn system(&mut self, keyspace: &str, table: &str, statement: &Statement, values: &[Cell]) -> Result<Vec<u8>, Failure> {
        let Statement::Select { selection, relations, .. } = statement else {
            return Err(invalid("only SELECT reads the system keyspaces".to_owned()));
        };
        let (columns, rows) = self.system_table(keyspace, table)?;
        let wanted = |row: &Vec<Cell>| {
            relations.iter().all(|relation| {
                let Some(at) = columns.iter().position(|(name, _)| *name == relation.column) else {
                    return false;
                };
                if !matches!(relation.operator, Operator::Equal | Operator::In) {
                    return true;
                }
                relation.terms.iter().any(|term| {
                    let expected = match term {
                        Term::Text(text) => Some(text.as_bytes().to_vec()),
                        Term::Marker(index) => values.get(*index).cloned().flatten(),
                        _ => None,
                    };
                    expected.is_some_and(|expected| row[at].as_deref() == Some(expected.as_slice()))
                })
            })
        };
        let rows: Vec<Vec<Cell>> = rows.into_iter().filter(wanted).collect();
        let (columns, rows) = match selection {
            Selection::All => (columns, rows),
            Selection::Count => (
                vec![("count".to_owned(), ColumnKind::Simple(CqlType::BigInt))],
                vec![vec![Some(i64::try_from(rows.len()).unwrap_or(i64::MAX).to_be_bytes().to_vec())]],
            ),
            Selection::Columns(chosen) => {
                let mut picked = Vec::new();
                let mut positions = Vec::new();
                for (name, alias) in chosen {
                    let at = columns.iter().position(|(column, _)| column == name).ok_or_else(|| invalid(format!("Undefined column name {name}")))?;
                    picked.push((alias.clone().unwrap_or_else(|| name.clone()), columns[at].1));
                    positions.push(at);
                }
                let rows = rows.into_iter().map(|row| positions.iter().map(|at| row[*at].clone()).collect()).collect();
                (picked, rows)
            }
        };
        Ok(rows_result(keyspace, table, &columns, &rows))
    }

    fn user_keyspaces(&mut self) -> Result<Vec<String>, Failure> {
        Ok(text_rows(&mut self.session, "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name")
            .map_err(invalid)?
            .into_iter()
            .filter_map(|row| row.into_iter().next())
            .filter(|name| !name.starts_with("pg_") && name != "information_schema" && name != "eggdb_cql")
            .collect())
    }

    fn user_tables(&mut self) -> Result<Vec<(String, String)>, Failure> {
        let keyspaces = self.user_keyspaces()?;
        Ok(text_rows(&mut self.session, "SELECT table_schema, table_name FROM information_schema.tables WHERE table_type = 'BASE TABLE' ORDER BY table_schema, table_name")
            .map_err(invalid)?
            .into_iter()
            .filter_map(|row| Some((row.first()?.clone(), row.get(1)?.clone())))
            .filter(|(keyspace, _)| keyspaces.contains(keyspace))
            .collect())
    }

    fn system_table(&mut self, keyspace: &str, table: &str) -> Result<(Vec<(String, ColumnKind)>, Vec<Vec<Cell>>), Failure> {
        let simple = |name: &str, cql: CqlType| (name.to_owned(), ColumnKind::Simple(cql));
        let address = self.address;
        Ok(match (keyspace, table) {
            ("system", "local") => (
                vec![
                    simple("key", CqlType::Varchar),
                    simple("bootstrapped", CqlType::Varchar),
                    simple("broadcast_address", CqlType::Inet),
                    simple("cluster_name", CqlType::Varchar),
                    simple("cql_version", CqlType::Varchar),
                    simple("data_center", CqlType::Varchar),
                    simple("gossip_generation", CqlType::Int),
                    simple("host_id", CqlType::Uuid),
                    simple("listen_address", CqlType::Inet),
                    simple("native_protocol_version", CqlType::Varchar),
                    simple("partitioner", CqlType::Varchar),
                    simple("rack", CqlType::Varchar),
                    simple("release_version", CqlType::Varchar),
                    simple("rpc_address", CqlType::Inet),
                    simple("schema_version", CqlType::Uuid),
                    ("tokens".to_owned(), ColumnKind::Set(CqlType::Varchar)),
                ],
                vec![vec![
                    text("local"),
                    text("COMPLETED"),
                    inet(address),
                    text("EggDB"),
                    text("3.4.5"),
                    text("datacenter1"),
                    Some(0i32.to_be_bytes().to_vec()),
                    Some(fixed_uuid("eggdb-host").to_vec()),
                    inet(address),
                    text("4"),
                    text("org.apache.cassandra.dht.Murmur3Partitioner"),
                    text("rack1"),
                    text("3.11.17"),
                    inet(address),
                    Some(fixed_uuid("eggdb-schema").to_vec()),
                    collection(&[b"0"]),
                ]],
            ),
            ("system", "peers") => (
                vec![
                    simple("peer", CqlType::Inet),
                    simple("data_center", CqlType::Varchar),
                    simple("host_id", CqlType::Uuid),
                    simple("preferred_ip", CqlType::Inet),
                    simple("rack", CqlType::Varchar),
                    simple("release_version", CqlType::Varchar),
                    simple("rpc_address", CqlType::Inet),
                    simple("schema_version", CqlType::Uuid),
                    ("tokens".to_owned(), ColumnKind::Set(CqlType::Varchar)),
                ],
                Vec::new(),
            ),
            ("system_schema", "keyspaces") => {
                let rows = self
                    .user_keyspaces()?
                    .into_iter()
                    .map(|name| {
                        let mut map = Out::default();
                        map.int(2)
                            .bytes(Some(b"class"))
                            .bytes(Some(b"org.apache.cassandra.locator.SimpleStrategy"))
                            .bytes(Some(b"replication_factor"))
                            .bytes(Some(b"1"));
                        vec![text(&name), Some(vec![1]), Some(map.bytes)]
                    })
                    .collect();
                (
                    vec![simple("keyspace_name", CqlType::Varchar), simple("durable_writes", CqlType::Boolean), ("replication".to_owned(), ColumnKind::Map(CqlType::Varchar, CqlType::Varchar))],
                    rows,
                )
            }
            ("system_schema", "tables") => {
                let rows = self
                    .user_tables()?
                    .into_iter()
                    .map(|(keyspace, name)| {
                        let id = fixed_uuid(&format!("{keyspace}.{name}"));
                        vec![text(&keyspace), text(&name), text(""), Some(0i32.to_be_bytes().to_vec()), collection(&[b"compound"]), Some(864_000i32.to_be_bytes().to_vec()), Some(id.to_vec())]
                    })
                    .collect();
                (
                    vec![
                        simple("keyspace_name", CqlType::Varchar),
                        simple("table_name", CqlType::Varchar),
                        simple("comment", CqlType::Varchar),
                        simple("default_time_to_live", CqlType::Int),
                        ("flags".to_owned(), ColumnKind::Set(CqlType::Varchar)),
                        simple("gc_grace_seconds", CqlType::Int),
                        simple("id", CqlType::Uuid),
                    ],
                    rows,
                )
            }
            ("system_schema", "columns") => {
                let mut rows = Vec::new();
                for (keyspace, name) in self.user_tables()? {
                    let Ok(info) = self.catalog().table(&keyspace, &name) else {
                        continue;
                    };
                    for (column, cql) in &info.columns {
                        let (kind, position, order) = if let Some(at) = info.partition.iter().position(|key| key == column) {
                            ("partition_key", at, "none")
                        } else if let Some(at) = info.clustering.iter().position(|key| key == column) {
                            ("clustering", at, if info.descending.contains(column) { "desc" } else { "asc" })
                        } else {
                            ("regular", usize::MAX, "none")
                        };
                        let position = i32::try_from(position).unwrap_or(-1);
                        rows.push(vec![text(&keyspace), text(&name), text(column), text(order), Some(column.as_bytes().to_vec()), text(kind), Some(position.to_be_bytes().to_vec()), text(cql.name())]);
                    }
                }
                (
                    vec![
                        simple("keyspace_name", CqlType::Varchar),
                        simple("table_name", CqlType::Varchar),
                        simple("column_name", CqlType::Varchar),
                        simple("clustering_order", CqlType::Varchar),
                        simple("column_name_bytes", CqlType::Blob),
                        simple("kind", CqlType::Varchar),
                        simple("position", CqlType::Int),
                        simple("type", CqlType::Varchar),
                    ],
                    rows,
                )
            }
            ("system_schema", "types" | "functions" | "aggregates" | "triggers" | "indexes" | "views" | "dropped_columns") => (vec![simple("keyspace_name", CqlType::Varchar)], Vec::new()),
            _ => return Err(invalid(format!("unconfigured table {table}"))),
        })
    }
}

fn error_body(failure: &Failure) -> Vec<u8> {
    let mut out = Out::default();
    out.int(failure.code).string(&failure.message);
    out.bytes.extend_from_slice(&failure.extra);
    out.bytes
}

pub(crate) fn serve_stream<S: KeyValueStore>(store: &S, config: &ServerConfig, address: IpAddr, mut reader: impl Read, writer: &mut impl Write, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    session.identify(&config.user, &config.database);
    let mut connection = Connection {
        config,
        session,
        address,
        started: false,
        authenticated: false,
        keyspace: None,
        prepared: BTreeMap::new(),
        tables: BTreeMap::new(),
    };
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        loop {
            match take_request(&mut buffer) {
                Ok(Some(request)) => {
                    let version = request.version;
                    let (opcode, body) = if version != VERSION {
                        (ERROR, error_body(&failure(PROTOCOL_ERROR, format!("Invalid or unsupported protocol version ({version}); supported versions are (4/v4)"))))
                    } else if request.flags & 0x05 != 0 {
                        (ERROR, error_body(&failure(PROTOCOL_ERROR, "compressed frames and custom payloads are not supported yet")))
                    } else {
                        match connection.answer(&request) {
                            Ok(answer) => answer,
                            Err(failure) => (ERROR, error_body(&failure)),
                        }
                    };
                    writer.write_all(&response(version, request.stream, opcode, &body))?;
                }
                Ok(None) => break,
                Err(message) => {
                    writer.write_all(&response(VERSION, 0, ERROR, &error_body(&failure(PROTOCOL_ERROR, message))))?;
                    return Ok(());
                }
            }
        }
        if stop() {
            return Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
            Err(error) => return Err(error),
        }
    }
}

/// Serves CQL clients on `listener` from the SQL tables in `store` until `stop` turns true.
pub fn serve<S: KeyValueStore + Sync>(store: &S, listener: &TcpListener, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?.ip();
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(READ_POLL)).is_err() || stream.set_nodelay(true).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new().name("eggdb-cql".to_owned()).spawn_scoped(scope, move || {
                if let Ok(mut writer) = stream.try_clone() {
                    let _ = serve_stream(store, config, address, ReadHalf(stream), &mut writer, stop);
                }
                active.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}

struct ReadHalf(TcpStream);

impl Read for ReadHalf {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}
