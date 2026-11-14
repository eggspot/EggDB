import EggDB
import Foundation
#if canImport(SQLite3)
import SQLite3

let rows = Int(ProcessInfo.processInfo.environment["DRIVER_BENCH_ROWS"] ?? "") ?? 20000
let lookups = Int(ProcessInfo.processInfo.environment["DRIVER_BENCH_LOOKUPS"] ?? "") ?? 5000
let transient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

func timed(_ work: () throws -> Void) rethrows -> Double {
    let started = DispatchTime.now().uptimeNanoseconds
    try work()
    return Double(DispatchTime.now().uptimeNanoseconds - started) / 1e6
}

func eggdbSide(_ path: String) throws -> [Double] {
    let db = try Database(path: path)
    try db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
    let insert = try timed {
        let statement = try db.prepare("INSERT INTO t VALUES (?, ?, ?)")
        try db.transaction { _ in
            for at in 1...rows {
                try statement.execute([.integer(Int64(at)), .text("name \(at)"), .real(Double(at) * 0.25)])
            }
        }
        statement.finalize()
    }
    let lookup = try timed {
        let statement = try db.prepare("SELECT name FROM t WHERE id = ?")
        for at in 1...lookups {
            try statement.forEachRow([.integer(Int64(at * 7 % rows + 1))]) { _ in }
        }
        statement.finalize()
    }
    let scan = try timed {
        let statement = try db.prepare("SELECT name FROM t")
        var total = 0
        try statement.forEachRow { row in
            if case .text(let name) = row[0] { total += name.utf8.count }
        }
        statement.finalize()
    }
    db.close()
    return [insert, lookup, scan]
}

func sqliteSide(_ path: String) -> [Double] {
    var db: OpaquePointer?
    sqlite3_open(path, &db)
    sqlite3_exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)", nil, nil, nil)
    let insert = timed {
        sqlite3_exec(db, "BEGIN", nil, nil, nil)
        var statement: OpaquePointer?
        sqlite3_prepare_v2(db, "INSERT INTO t VALUES (?, ?, ?)", -1, &statement, nil)
        for at in 1...rows {
            sqlite3_bind_int64(statement, 1, Int64(at))
            sqlite3_bind_text(statement, 2, "name \(at)", -1, transient)
            sqlite3_bind_double(statement, 3, Double(at) * 0.25)
            sqlite3_step(statement)
            sqlite3_reset(statement)
        }
        sqlite3_finalize(statement)
        sqlite3_exec(db, "COMMIT", nil, nil, nil)
    }
    let lookup = timed {
        var statement: OpaquePointer?
        sqlite3_prepare_v2(db, "SELECT name FROM t WHERE id = ?", -1, &statement, nil)
        for at in 1...lookups {
            sqlite3_bind_int64(statement, 1, Int64(at * 7 % rows + 1))
            while sqlite3_step(statement) == SQLITE_ROW {
                _ = String(cString: sqlite3_column_text(statement, 0))
            }
            sqlite3_reset(statement)
        }
        sqlite3_finalize(statement)
    }
    let scan = timed {
        var statement: OpaquePointer?
        sqlite3_prepare_v2(db, "SELECT name FROM t", -1, &statement, nil)
        var total = 0
        while sqlite3_step(statement) == SQLITE_ROW {
            total += String(cString: sqlite3_column_text(statement, 0)).utf8.count
        }
        sqlite3_finalize(statement)
    }
    sqlite3_close(db)
    return [insert, lookup, scan]
}

let directory = FileManager.default.temporaryDirectory.appendingPathComponent("eggdb-swift-bench-\(getpid())")
try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
let mine = try eggdbSide(directory.appendingPathComponent("bench.eggdb").path)
let theirs = sqliteSide(directory.appendingPathComponent("bench.sqlite").path)
for (at, workload) in ["insert", "lookup", "scan"].enumerated() {
    print(String(format: "swift %@ %.2f %.2f", workload, mine[at], theirs[at]))
}
try? FileManager.default.removeItem(at: directory)
#else
print("swift bench: SQLite3 is not available on this system")
#endif
