import CEggDB
import Foundation

public struct CollectionStats: Equatable {
    public let entries: UInt64
    public let keyBytes: UInt64
    public let valueBytes: UInt64
}

public struct ScanRange {
    public var start: [UInt8]?
    public var end: [UInt8]?
    public var prefix: [UInt8]?
    public var reverse: Bool
    public var endInclusive: Bool
    public var limit: Int

    public init(start: [UInt8]? = nil, end: [UInt8]? = nil, prefix: [UInt8]? = nil, reverse: Bool = false, endInclusive: Bool = false, limit: Int = 0) {
        self.start = start
        self.end = end
        self.prefix = prefix
        self.reverse = reverse
        self.endInclusive = endInclusive
        self.limit = limit
    }
}

private func keyValueCheck(_ status: Int32) throws {
    if status != EGGDB_OK {
        let message = eggdb_last_error().map { String(cString: $0) } ?? "EggDB failed"
        let state = eggdb_last_sqlstate().map { String(cString: $0) }
        throw EggDBError(status: status, sqlState: state?.isEmpty == false ? state : nil, message: message)
    }
}

private func ended(_ what: String) -> EggDBError {
    EggDBError(status: EGGDB_ERR_INVALID_ARGUMENT, sqlState: nil, message: what)
}

private func withOptionalName<T>(_ name: String?, _ body: (UnsafePointer<CChar>?) throws -> T) rethrows -> T {
    guard let name else { return try body(nil) }
    return try name.withCString { try body($0) }
}

private func withOptionalBytes<T>(_ bytes: [UInt8]?, _ body: (UnsafePointer<UInt8>?, Int) throws -> T) rethrows -> T {
    guard let bytes, !bytes.isEmpty else { return try body(nil, 0) }
    return try bytes.withUnsafeBufferPointer { try body($0.baseAddress, $0.count) }
}

public enum PowerSource: UInt32 {
    case mains = 0
    case battery = 1
    case lowBattery = 2
}

public final class KeyValue {
    private var database: OpaquePointer?
    public let readOnly: Bool

    public init(path: String, readOnly: Bool = false) throws {
        var options = eggdb_options()
        if readOnly {
            options.flags = UInt32(EGGDB_OPEN_READ_ONLY)
        }
        try keyValueCheck(eggdb_open(path, &options, &database))
        self.readOnly = readOnly
    }

    deinit {
        if let database {
            eggdb_close(database)
        }
    }

    public func close() throws {
        guard let closing = database else { return }
        database = nil
        try keyValueCheck(eggdb_close(closing))
    }

    public func suspend() throws {
        guard let database else { throw ended("the store is closed") }
        try keyValueCheck(eggdb_suspend(database))
    }

    public func resume() throws {
        guard let database else { throw ended("the store is closed") }
        try keyValueCheck(eggdb_resume(database))
    }

    public func setPowerSource(_ source: PowerSource) throws {
        guard let database else { throw ended("the store is closed") }
        try keyValueCheck(eggdb_set_power_source(database, source.rawValue))
    }

    public func begin(writing: Bool) throws -> Transaction {
        guard let database else { throw ended("the store is closed") }
        var handle: OpaquePointer?
        try keyValueCheck(writing ? eggdb_begin_write(database, &handle) : eggdb_begin_read(database, &handle))
        return Transaction(handle: handle, writing: writing)
    }

    public func read<T>(_ work: (Transaction) throws -> T) throws -> T {
        let transaction = try begin(writing: false)
        defer { transaction.rollback() }
        return try work(transaction)
    }

    public func write<T>(_ work: (Transaction) throws -> T) throws -> T {
        let transaction = try begin(writing: true)
        defer { transaction.rollback() }
        let result = try work(transaction)
        try transaction.commit()
        return result
    }
}

public final class Transaction {
    private var handle: OpaquePointer?
    public let writing: Bool

    init(handle: OpaquePointer?, writing: Bool) {
        self.handle = handle
        self.writing = writing
    }

    deinit {
        rollback()
    }

    private func live() throws -> OpaquePointer {
        guard let handle else { throw ended("the transaction has ended") }
        return handle
    }

    public func get(_ key: [UInt8], collection: String? = nil) throws -> [UInt8]? {
        let txn = try live()
        var data: UnsafeMutablePointer<UInt8>?
        var length = 0
        try withOptionalName(collection) { name in
            try withOptionalBytes(key) { keyData, keyLength in
                try keyValueCheck(eggdb_get(txn, name, keyData, keyLength, &data, &length))
            }
        }
        guard let data else { return nil }
        defer { eggdb_free(data, length) }
        return Array(UnsafeBufferPointer(start: data, count: length))
    }

    public func getText(_ key: String, collection: String? = nil) throws -> String? {
        try get(Array(key.utf8), collection: collection).map { String(decoding: $0, as: UTF8.self) }
    }

    public func put(_ key: [UInt8], _ value: [UInt8], collection: String? = nil, timeToLiveMs: UInt64? = nil) throws {
        let txn = try live()
        try withOptionalName(collection) { name in
            try withOptionalBytes(key) { keyData, keyLength in
                try withOptionalBytes(value) { valueData, valueLength in
                    if let timeToLiveMs, timeToLiveMs > 0 {
                        try keyValueCheck(eggdb_put_with_expiry(txn, name, keyData, keyLength, valueData, valueLength, timeToLiveMs))
                    } else {
                        try keyValueCheck(eggdb_put(txn, name, keyData, keyLength, valueData, valueLength))
                    }
                }
            }
        }
    }

    public func put(_ key: String, _ value: String, collection: String? = nil, timeToLiveMs: UInt64? = nil) throws {
        try put(Array(key.utf8), Array(value.utf8), collection: collection, timeToLiveMs: timeToLiveMs)
    }

    @discardableResult
    public func delete(_ key: [UInt8], collection: String? = nil) throws -> Bool {
        let txn = try live()
        var deleted: Int32 = 0
        try withOptionalName(collection) { name in
            try withOptionalBytes(key) { keyData, keyLength in
                try keyValueCheck(eggdb_delete(txn, name, keyData, keyLength, &deleted))
            }
        }
        return deleted != 0
    }

    public func scan(_ range: ScanRange = ScanRange(), collection: String? = nil) throws -> [(key: [UInt8], value: [UInt8])] {
        let txn = try live()
        var flags: UInt32 = 0
        if range.reverse { flags |= UInt32(EGGDB_SCAN_REVERSE) }
        if range.endInclusive { flags |= UInt32(EGGDB_SCAN_END_INCLUSIVE) }
        var start = range.start
        var end = range.end
        if let prefix = range.prefix {
            flags |= UInt32(EGGDB_SCAN_PREFIX)
            start = prefix
            end = nil
        }
        var scan: OpaquePointer?
        try withOptionalName(collection) { name in
            try withOptionalBytes(start) { startData, startLength in
                try withOptionalBytes(end) { endData, endLength in
                    try keyValueCheck(eggdb_scan_open(txn, name, startData, startLength, endData, endLength, flags, &scan))
                }
            }
        }
        defer { eggdb_scan_free(scan) }
        var gathered: [(key: [UInt8], value: [UInt8])] = []
        while range.limit <= 0 || gathered.count < range.limit {
            var entries: UnsafePointer<eggdb_entry>?
            var count = 0
            try keyValueCheck(eggdb_scan_next(txn, scan, 0, &entries, &count))
            guard count > 0, let entries else { break }
            for index in 0..<count {
                if range.limit > 0 && gathered.count == range.limit { break }
                let entry = entries[index]
                gathered.append((
                    key: Array(UnsafeBufferPointer(start: entry.key, count: entry.key_len)),
                    value: Array(UnsafeBufferPointer(start: entry.value, count: entry.value_len))
                ))
            }
        }
        return gathered
    }

    @discardableResult
    public func createCollection(_ name: String) throws -> Bool {
        let txn = try live()
        var created: Int32 = 0
        try keyValueCheck(eggdb_create_collection(txn, name, &created))
        return created != 0
    }

    @discardableResult
    public func dropCollection(_ name: String) throws -> Bool {
        let txn = try live()
        var dropped: Int32 = 0
        try keyValueCheck(eggdb_drop_collection(txn, name, &dropped))
        return dropped != 0
    }

    public func renameCollection(_ name: String, to newName: String) throws {
        let txn = try live()
        try keyValueCheck(eggdb_rename_collection(txn, name, newName))
    }

    public func collections() throws -> [String] {
        let txn = try live()
        var data: UnsafeMutablePointer<UInt8>?
        var length = 0
        var count = 0
        try keyValueCheck(eggdb_collection_names(txn, &data, &length, &count))
        guard let data else { return [] }
        defer { eggdb_free(data, length) }
        let joined = UnsafeBufferPointer(start: data, count: length)
        return joined.split(separator: 0, omittingEmptySubsequences: false)
            .prefix(count)
            .map { String(decoding: $0, as: UTF8.self) }
    }

    public func collectionStats(_ name: String) throws -> CollectionStats {
        let txn = try live()
        var stats = eggdb_collection_stats()
        try keyValueCheck(eggdb_collection_stats(txn, name, &stats))
        return CollectionStats(entries: stats.entries, keyBytes: stats.key_bytes, valueBytes: stats.value_bytes)
    }

    @discardableResult
    public func commit() throws -> UInt64 {
        let txn = try live()
        handle = nil
        var stamp: UInt64 = 0
        try keyValueCheck(eggdb_commit(txn, &stamp))
        return stamp
    }

    public func rollback() {
        guard let ending = handle else { return }
        handle = nil
        eggdb_rollback(ending)
    }
}
