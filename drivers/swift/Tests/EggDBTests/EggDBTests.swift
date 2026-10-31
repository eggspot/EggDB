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
}
