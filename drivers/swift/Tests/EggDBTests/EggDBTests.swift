import EggDB
import XCTest

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
}
