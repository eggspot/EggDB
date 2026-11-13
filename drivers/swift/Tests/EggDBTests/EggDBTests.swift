import EggDB
import Foundation
import XCTest
#if canImport(SQLite3)
import SQLite3
#endif

final class EggDBTests: XCTestCase {
    func testRowsParametersAndErrors() throws {
        let db = try Database(path: ":memory:")
        try db.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
        try db.execute("INSERT INTO people (name, score) VALUES (?, ?)", ["Ada", "1.5"])
        try db.execute("INSERT INTO people (name, score) VALUES (?, ?)", [nil, "2"])
        let rows = try db.execute("SELECT id, name, score FROM people ORDER BY id")[0].rows
        XCTAssertEqual(rows[0], [.integer(1), .text("Ada"), .real(1.5)])
        XCTAssertEqual(rows[1][1], .null)
        XCTAssertThrowsError(try db.execute("SELECT * FROM nowhere")) { error in
            XCTAssertEqual((error as? EggDBError)?.sqlState?.count, 5)
        }
    }

    func testKeyValueTransactionsAndCollections() throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("eggdb-swift-kv-\(UUID().uuidString).eggdb").path
        defer { try? FileManager.default.removeItem(atPath: path) }
        let store = try KeyValue(path: path)
        try store.write { txn in
            try txn.put("a", "1")
            try txn.put(Array("b".utf8), [0, 2])
            XCTAssertTrue(try txn.createCollection("users"))
            try txn.put("u1", "Ada", collection: "users")
        }
        try store.read { txn in
            XCTAssertEqual(try txn.getText("a"), "1")
            XCTAssertNil(try txn.get(Array("missing".utf8)))
            let all = try txn.scan()
            XCTAssertEqual(all.count, 2)
            XCTAssertEqual(all[1].value, [0, 2])
            XCTAssertEqual(try txn.scan(ScanRange(reverse: true, limit: 1)).map { $0.key }, [Array("b".utf8)])
            XCTAssertEqual(try txn.collections(), ["users"])
            XCTAssertEqual(try txn.collectionStats("users").entries, 1)
        }
        try store.write { txn in
            XCTAssertTrue(try txn.delete(Array("a".utf8)))
            try txn.renameCollection("users", to: "people")
        }
        try store.read { txn in
            XCTAssertNil(try txn.getText("a"))
            XCTAssertEqual(try txn.getText("u1", collection: "people"), "Ada")
        }
        try store.close()
    }

    func testPreparedStatementsAndTransactions() throws {
        let db = try Database(path: ":memory:")
        try db.execute("CREATE TABLE eggs (id INTEGER PRIMARY KEY, name TEXT, weight REAL, shell BLOB)")
        let insert = try db.prepare("INSERT INTO eggs VALUES (?, ?, ?, ?)")
        try db.transaction { _ in
            for id in 1...100 {
                try insert.execute([.integer(Int64(id)), .text("egg \(id)"), .real(Double(id) / 2), .blob([UInt8(id)])])
            }
        }
        XCTAssertThrowsError(try db.transaction { _ in try insert.execute([.integer(1), .null, .null, .null]) })
        let select = try db.prepare("SELECT name, weight, shell FROM eggs WHERE id = ?")
        XCTAssertEqual(try select.query([.integer(42)]), [[.text("egg 42"), .real(21), .blob([42])]])
        XCTAssertEqual(try select.query([.integer(1000)]), [])
        try db.beginTransaction()
        try db.execute("DELETE FROM eggs")
        try db.rollback()
        XCTAssertEqual(try db.execute("SELECT count(*) FROM eggs")[0].rows, [[.integer(100)]])
        insert.finalize()
        XCTAssertThrowsError(try insert.execute([]))
    }

    #if canImport(SQLite3)
    func testPreparedStatementsAgainstSQLitesLibrary() throws {
        let rows = 5_000
        let lookups = 2_000
        let slowestRatio = 2.0
        func seconds(_ work: () throws -> Void) rethrows -> Double {
            let started = DispatchTime.now().uptimeNanoseconds
            try work()
            return Double(DispatchTime.now().uptimeNanoseconds - started) / 1e9
        }
        func eggdb() throws -> (Double, Double) {
            let db = try Database(path: ":memory:")
            try db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
            let insert = try seconds {
                let statement = try db.prepare("INSERT INTO t VALUES (?, ?, ?)")
                try db.transaction { _ in
                    for id in 1...rows {
                        try statement.execute([.integer(Int64(id)), .text("name \(id)"), .real(Double(id) * 0.25)])
                    }
                }
            }
            let select = try seconds {
                let statement = try db.prepare("SELECT name FROM t WHERE id = ?")
                for at in 1...lookups {
                    _ = try statement.query([.integer(Int64(at * 7 % rows + 1))])
                }
            }
            return (insert, select)
        }
        func sqlite() -> (Double, Double) {
            var db: OpaquePointer?
            sqlite3_open(":memory:", &db)
            defer { sqlite3_close(db) }
            sqlite3_exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)", nil, nil, nil)
            let transient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)
            let insert = seconds {
                var statement: OpaquePointer?
                sqlite3_prepare_v2(db, "INSERT INTO t VALUES (?, ?, ?)", -1, &statement, nil)
                sqlite3_exec(db, "BEGIN", nil, nil, nil)
                for id in 1...rows {
                    sqlite3_bind_int64(statement, 1, Int64(id))
                    sqlite3_bind_text(statement, 2, "name \(id)", -1, transient)
                    sqlite3_bind_double(statement, 3, Double(id) * 0.25)
                    sqlite3_step(statement)
                    sqlite3_reset(statement)
                }
                sqlite3_exec(db, "COMMIT", nil, nil, nil)
                sqlite3_finalize(statement)
            }
            let select = seconds {
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
            return (insert, select)
        }
        _ = try eggdb()
        _ = sqlite()
        let (eggInsert, eggSelect) = try eggdb()
        let (liteInsert, liteSelect) = sqlite()
        let report = String(format: "swift binding insert_ms eggdb=%.1f sqlite=%.1f select_ms eggdb=%.1f sqlite=%.1f", eggInsert * 1000, liteInsert * 1000, eggSelect * 1000, liteSelect * 1000)
        print(report)
        XCTAssertLessThanOrEqual(eggInsert, liteInsert * slowestRatio, report)
        XCTAssertLessThanOrEqual(eggSelect, liteSelect * slowestRatio, report)
    }
    #endif
}
