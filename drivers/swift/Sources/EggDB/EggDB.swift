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
                        let cell = result.values[row * result.column_count + column]
                        switch Int32(cell.kind) {
                        case EGGDB_INTEGER: cells.append(.integer(cell.integer))
                        case EGGDB_REAL: cells.append(.real(cell.real))
                        case EGGDB_TEXT:
                            let bytes = UnsafeBufferPointer(start: cell.data, count: cell.len)
                            cells.append(.text(String(decoding: bytes, as: UTF8.self)))
                        case EGGDB_BLOB: cells.append(.blob(Array(UnsafeBufferPointer(start: cell.data, count: cell.len))))
                        default: cells.append(.null)
                        }
                    }
                    rows.append(cells)
                }
                gathered.append(Result(tag: String(cString: result.tag), columns: columns, rows: rows))
            }
        }
        return gathered
    }
}
