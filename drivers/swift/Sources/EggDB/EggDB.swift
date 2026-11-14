import CEggDB
import Foundation

public enum Value: Equatable {
    case null
    case integer(Int64)
    case real(Double)
    case text(String)
    case blob([UInt8])
}

public struct EggDBError: Error {
    public let status: Int32
    public let sqlState: String?
    public let message: String
}

public struct Result {
    public let tag: String
    public let columns: [String]
    public let rows: [[Value]]
}

private func check(_ status: Int32) throws {
    if status != EGGDB_OK {
        let message = eggdb_last_error().map { String(cString: $0) } ?? "EggDB failed"
        let state = eggdb_last_sqlstate().map { String(cString: $0) }
        throw EggDBError(status: status, sqlState: state?.isEmpty == false ? state : nil, message: message)
    }
}

public final class Database {
    private var database: OpaquePointer?
    private var session: OpaquePointer?
    public let dialect: String

    public init(path: String, dialect: String = "sqlite") throws {
        if path.hasPrefix("postgresql://") || path.hasPrefix("postgres://") {
            try check(eggdb_sql_connect(path, &session))
            self.dialect = "postgresql"
            return
        }
        var options = eggdb_options()
        try check(eggdb_open(path, &options, &database))
        let status = eggdb_sql_open(database, dialect, &session)
        if status != EGGDB_OK {
            eggdb_close(database)
            database = nil
            try check(status)
        }
        self.dialect = dialect
    }

    deinit {
        close()
    }

    public func close() {
        if let session {
            eggdb_sql_close(session)
            self.session = nil
        }
        if let database {
            eggdb_close(database)
            self.database = nil
        }
    }

    private func numbered(_ sql: String) -> String {
        guard dialect == "postgresql" else { return sql }
        var out = ""
        var quote: Character?
        var number = 0
        for character in sql {
            if let open = quote {
                if character == open { quote = nil }
                out.append(character)
            } else if character == "'" || character == "\"" {
                quote = character
                out.append(character)
            } else if character == "?" {
                number += 1
                out += "$\(number)"
            } else {
                out.append(character)
            }
        }
        return out
    }

    @discardableResult
    public func execute(_ sql: String, _ parameters: [String?] = []) throws -> [Result] {
        var joined: [UInt8] = []
        var spans: [(offset: Int, length: Int)?] = []
        for parameter in parameters {
            guard let parameter else {
                spans.append(nil)
                continue
            }
            let bytes = Array(parameter.utf8)
            spans.append((joined.count, bytes.count))
            joined.append(contentsOf: bytes)
        }
        if joined.isEmpty { joined.append(0) }
        var gathered: [Result] = []
        try joined.withUnsafeBufferPointer { storage in
            var bound = spans.map { span -> eggdb_parameter in
                guard let span else { return eggdb_parameter(text: nil, len: 0) }
                return eggdb_parameter(text: storage.baseAddress! + span.offset, len: span.length)
            }
            var results: UnsafePointer<eggdb_result>?
            var count = 0
            try check(eggdb_sql_execute_with_parameters(session, numbered(sql), &bound, bound.count, &results, &count))
            gathered = gatheredResults(results, count)
        }
        return gathered
    }

    private func openSession() throws -> OpaquePointer {
        guard let session else { throw EggDBError(status: EGGDB_ERR_INVALID_ARGUMENT, sqlState: nil, message: "the database is closed") }
        return session
    }

    @discardableResult
    public func copyFrom(_ sql: String, pieces: [[UInt8]]) throws -> Int {
        var copy: OpaquePointer?
        try check(eggdb_sql_copy_in_begin(try openSession(), sql, &copy))
        defer { eggdb_copy_free(copy) }
        for piece in pieces where !piece.isEmpty {
            try piece.withUnsafeBufferPointer { buffer in
                try check(eggdb_copy_put(copy, buffer.baseAddress, buffer.count))
            }
        }
        return copiedRows(try endCopy(copy))
    }

    @discardableResult
    public func copyFrom(_ sql: String, text: String) throws -> Int {
        try copyFrom(sql, pieces: [Array(text.utf8)])
    }

    public func copyTo(_ sql: String, onPiece: ([UInt8]) throws -> Void) throws {
        var copy: OpaquePointer?
        try check(eggdb_sql_copy_out_begin(try openSession(), sql, &copy))
        defer { eggdb_copy_free(copy) }
        while true {
            var data: UnsafePointer<UInt8>?
            var length = 0
            try check(eggdb_copy_read(copy, &data, &length))
            if length == 0 { break }
            try onPiece(Array(UnsafeBufferPointer(start: data, count: length)))
        }
        _ = try endCopy(copy)
    }

    public func copyTo(_ sql: String) throws -> [UInt8] {
        var out: [UInt8] = []
        try copyTo(sql) { out.append(contentsOf: $0) }
        return out
    }

    private func endCopy(_ copy: OpaquePointer?) throws -> [Result] {
        var results: UnsafePointer<eggdb_result>?
        var count = 0
        try check(eggdb_copy_end(copy, &results, &count))
        return gatheredResults(results, count)
    }

    public func prepare(_ sql: String) throws -> Statement {
        guard let session else { throw EggDBError(status: EGGDB_ERR_INVALID_ARGUMENT, sqlState: nil, message: "the database is closed") }
        var statement: OpaquePointer?
        let text = Array(numbered(sql).utf8)
        try text.withUnsafeBufferPointer { bytes in
            try bytes.withMemoryRebound(to: CChar.self) { chars in
                try check(eggdb_sql_prepare(session, chars.baseAddress, chars.count, &statement))
            }
        }
        return Statement(handle: statement)
    }

    public func beginTransaction() throws {
        try execute("BEGIN")
    }

    public func commit() throws {
        try execute("COMMIT")
    }

    public func rollback() throws {
        try execute("ROLLBACK")
    }

    public func transaction<T>(_ work: (Database) throws -> T) throws -> T {
        try beginTransaction()
        do {
            let result = try work(self)
            try commit()
            return result
        } catch {
            try? rollback()
            throw error
        }
    }
}

private func converted(_ cell: eggdb_value) -> Value {
    switch Int32(cell.kind) {
    case EGGDB_INTEGER: return .integer(cell.integer)
    case EGGDB_REAL: return .real(cell.real)
    case EGGDB_TEXT: return .text(String(decoding: UnsafeBufferPointer(start: cell.data, count: cell.len), as: UTF8.self))
    case EGGDB_BLOB: return .blob(Array(UnsafeBufferPointer(start: cell.data, count: cell.len)))
    default: return .null
    }
}

private func gatheredResults(_ results: UnsafePointer<eggdb_result>?, _ count: Int) -> [Result] {
    var gathered: [Result] = []
    for index in 0..<count {
        let result = results![index]
        var columns: [String] = []
        for column in 0..<result.column_count {
            columns.append(String(cString: result.column_names[column]!))
        }
        var rows: [[Value]] = []
        for row in 0..<result.row_count {
            var cells: [Value] = []
            for column in 0..<result.column_count {
                cells.append(converted(result.values[row * result.column_count + column]))
            }
            rows.append(cells)
        }
        gathered.append(Result(tag: String(cString: result.tag), columns: columns, rows: rows))
    }
    return gathered
}

private func copiedRows(_ results: [Result]) -> Int {
    for result in results.reversed() {
        let words = result.tag.split(separator: " ")
        if words.count == 2 && words[0] == "COPY" { return Int(words[1]) ?? -1 }
    }
    return -1
}

public final class Statement {
    private var handle: OpaquePointer?

    init(handle: OpaquePointer?) {
        self.handle = handle
    }

    deinit {
        finalize()
    }

    public func finalize() {
        if let handle {
            eggdb_stmt_finalize(handle)
            self.handle = nil
        }
    }

    private func live() throws -> OpaquePointer {
        guard let handle else { throw EggDBError(status: EGGDB_ERR_INVALID_ARGUMENT, sqlState: nil, message: "the statement is finalized") }
        return handle
    }

    public func bind(_ values: [Value]) throws {
        let statement = try live()
        try check(eggdb_stmt_clear_bindings(statement))
        for (offset, value) in values.enumerated() {
            let index = offset + 1
            switch value {
            case .null:
                try check(eggdb_stmt_bind_null(statement, index))
            case .integer(let number):
                try check(eggdb_stmt_bind_int64(statement, index, number))
            case .real(let number):
                try check(eggdb_stmt_bind_double(statement, index, number))
            case .text(let text):
                let bytes = Array(text.utf8)
                try bytes.withUnsafeBufferPointer { buffer in
                    try buffer.withMemoryRebound(to: CChar.self) { chars in
                        try check(eggdb_stmt_bind_text(statement, index, chars.baseAddress, chars.count))
                    }
                }
            case .blob(let bytes):
                try bytes.withUnsafeBufferPointer { buffer in
                    try check(eggdb_stmt_bind_blob(statement, index, buffer.baseAddress, buffer.count))
                }
            }
        }
    }

    @discardableResult
    public func execute(_ values: [Value] = []) throws -> Int {
        try bind(values)
        var results: UnsafePointer<eggdb_result>?
        var count = 0
        try check(eggdb_stmt_execute(try live(), &results, &count))
        return count
    }

    public func query(_ values: [Value] = []) throws -> [[Value]] {
        var rows: [[Value]] = []
        try forEachRow(values) { rows.append($0) }
        return rows
    }

    public func forEachRow(_ values: [Value] = [], _ body: ([Value]) throws -> Void) throws {
        let statement = try live()
        try check(eggdb_stmt_reset(statement))
        try bind(values)
        defer { eggdb_stmt_reset(statement) }
        var hasRow: Int32 = 0
        while true {
            try check(eggdb_stmt_step(statement, &hasRow))
            if hasRow == 0 { break }
            var cells: UnsafePointer<eggdb_value>?
            var count = 0
            try check(eggdb_stmt_row(statement, &cells, &count))
            try body((0..<count).map { converted(cells![$0]) })
        }
    }

    public func columns() throws -> [String] {
        let statement = try live()
        var count = 0
        try check(eggdb_stmt_column_count(statement, &count))
        return try (0..<count).map { column in
            var name: UnsafePointer<CChar>?
            try check(eggdb_stmt_column_name(statement, column, &name))
            return name.map { String(cString: $0) } ?? ""
        }
    }
}
