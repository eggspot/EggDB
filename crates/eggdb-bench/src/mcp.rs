use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::database::failed;
use crate::measure;
use eggdb::sql::{Dialect, OwnedStore};
use eggdb::{Database, Options};
use eggdb_mcp::audit::{AuditLog, audit_path_for};
use eggdb_mcp::{DatabaseOpener, OpenRefusal, Server, ServerConfig};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ROWS: usize = 1_000;
const QUERY: &str = "SELECT id, name, amount FROM rows WHERE id <= 100";

pub(crate) const TOOLS_CALL: &str = "mcp.tools_call";
pub(crate) const WRITE_CALL: &str = "mcp.write_call";
const FIRST_WRITTEN_ID: u64 = 1_000_000;

struct Opener(PathBuf);

impl Opener {
    fn open(&self) -> eggdb::Result<Database> {
        Database::open_with(&self.0, Options::new().read_only(true))
    }

    fn open_for_writing(&self) -> eggdb::Result<Database> {
        Database::open_with(&self.0, Options::new().create_if_missing(false))
    }
}

impl DatabaseOpener for Opener {
    type Store = OwnedStore;

    fn open_read_only(&self) -> Result<OwnedStore, OpenRefusal> {
        self.open()
            .map(OwnedStore)
            .map_err(|error| OpenRefusal::Failed(error.to_string()))
    }

    fn open_read_write(&self) -> Result<OwnedStore, OpenRefusal> {
        self.open_for_writing()
            .map(OwnedStore)
            .map_err(|error| OpenRefusal::Failed(error.to_string()))
    }

    fn close(&self, store: OwnedStore) -> Result<(), OpenRefusal> {
        store
            .close()
            .map_err(|error| OpenRefusal::Failed(error.to_string()))
    }
}

fn database(path: &Path) -> Result<(), String> {
    let db = Database::open(path).map_err(failed("open"))?;
    let mut sql = String::from(
        "CREATE TABLE rows (id integer PRIMARY KEY, name text, amount integer); INSERT INTO rows VALUES ",
    );
    for row in 0..ROWS {
        if row > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!("({row}, 'customer {row}', {})", row % 1000));
    }
    db.sql_store()
        .session(Dialect::PostgreSql)
        .execute(&sql, &mut |_| {})
        .map_err(failed("load"))?;
    db.close().map_err(failed("close"))
}

fn open_and_select(opener: &Opener, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        let db = opener.open().map_err(failed("open read-only"))?;
        db.sql_store()
            .session(Dialect::PostgreSql)
            .execute(QUERY, &mut |result| {
                black_box(result);
            })
            .map_err(failed(QUERY))?;
        db.close().map_err(failed("close"))?;
    }
    Ok(())
}

pub(crate) fn tools_call(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("mcp")?;
    database(&file.path)?;
    let audit_path = audit_path_for(&file.path);
    let _ = std::fs::remove_file(&audit_path);
    let opener = Opener(file.path.clone());
    let mut server = Server::new(
        &opener,
        ServerConfig {
            database_name: "bench.db".to_owned(),
            server_version: "bench".to_owned(),
            time_limit: Duration::from_secs(60),
            audit: AuditLog::new(audit_path.clone()),
            allow_write: false,
            tables: None,
            call_gap: Duration::ZERO,
            exit_process: None,
        },
    )
    .map_err(|failure| format!("start the MCP server: {failure:?}"))?;
    server.handle(br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#);
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"run_sql","arguments":{{"sql":"{QUERY}"}}}}}}"#
    );
    let first = server.handle(request.as_bytes()).unwrap_or_default();
    if !first.contains("\"isError\":false") {
        let _ = std::fs::remove_file(&audit_path);
        return Err(format!("the MCP call failed: {first}"));
    }
    let measured = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |iterations| {
            for _ in 0..iterations {
                black_box(server.handle(request.as_bytes()));
            }
            Ok(())
        }),
        &mut measure::timed(&mut |iterations| open_and_select(&opener, iterations)),
    );
    let _ = std::fs::remove_file(&audit_path);
    let measured = measured?;
    Ok(vec![
        Metric::ratio(TOOLS_CALL, "ratio_to_open_and_select", measured.ratio),
        Metric::reported(TOOLS_CALL, "ns_per_call", measured.subject_ns),
        Metric::reported(TOOLS_CALL, "reference_ns_per_call", measured.reference_ns),
    ])
}

fn insert(id: u64) -> String {
    format!("INSERT INTO rows VALUES ({id}, 'written {id}', 1)")
}

fn open_and_insert(opener: &Opener, next: &mut u64, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        let db = opener.open_for_writing().map_err(failed("open"))?;
        let statement = insert(*next);
        *next += 1;
        db.sql_store()
            .session(Dialect::PostgreSql)
            .execute(&statement, &mut |result| {
                black_box(result);
            })
            .map_err(failed("insert"))?;
        db.close().map_err(failed("close"))?;
    }
    Ok(())
}

pub(crate) fn write_call(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("mcp-write")?;
    database(&file.path)?;
    let audit_path = audit_path_for(&file.path);
    let _ = std::fs::remove_file(&audit_path);
    let opener = Opener(file.path.clone());
    let mut server = Server::new(
        &opener,
        ServerConfig {
            database_name: "bench.db".to_owned(),
            server_version: "bench".to_owned(),
            time_limit: Duration::from_secs(60),
            audit: AuditLog::new(audit_path.clone()),
            allow_write: true,
            tables: None,
            call_gap: Duration::ZERO,
            exit_process: None,
        },
    )
    .map_err(|failure| format!("start the MCP server: {failure:?}"))?;
    server.handle(br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#);
    let mut next_call = FIRST_WRITTEN_ID;
    let mut next_direct = FIRST_WRITTEN_ID * 2;
    let mut call = |id: u64| {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"write_sql","arguments":{{"sql":"{}"}}}}}}"#,
            insert(id)
        );
        server.handle(request.as_bytes()).unwrap_or_default()
    };
    let first = call(next_call);
    next_call += 1;
    if !first.contains("\"isError\":false") {
        let _ = std::fs::remove_file(&audit_path);
        return Err(format!("the MCP write failed: {first}"));
    }
    let measured = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |iterations| {
            for _ in 0..iterations {
                black_box(call(next_call));
                next_call += 1;
            }
            Ok(())
        }),
        &mut measure::timed(&mut |iterations| {
            open_and_insert(&opener, &mut next_direct, iterations)
        }),
    );
    let _ = std::fs::remove_file(&audit_path);
    let measured = measured?;
    Ok(vec![
        Metric::ratio(WRITE_CALL, "ratio_to_open_and_insert", measured.ratio),
        Metric::reported(WRITE_CALL, "ns_per_call", measured.subject_ns),
        Metric::reported(WRITE_CALL, "reference_ns_per_call", measured.reference_ns),
    ])
}
