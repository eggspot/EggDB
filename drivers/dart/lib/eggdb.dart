import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

final class _Options extends Struct {
  @Uint32()
  external int flags;
  @Uint32()
  external int durability;
  @Uint64()
  external int cacheBytes;
}

final class _Value extends Struct {
  @Int32()
  external int kind;
  @Int64()
  external int integer;
  @Double()
  external double real;
  external Pointer<Uint8> data;
  @Size()
  external int len;
}

final class _Parameter extends Struct {
  external Pointer<Uint8> text;
  @Size()
  external int len;
}

final class _Result extends Struct {
  external Pointer<Uint8> tag;
  @Size()
  external int columnCount;
  external Pointer<Pointer<Uint8>> columnNames;
  @Size()
  external int rowCount;
  external Pointer<_Value> values;
}

const processLibrary = ':process:';

DynamicLibrary _openLibrary(String name) =>
    name == processLibrary ? DynamicLibrary.process() : DynamicLibrary.open(name);

class EggDbException implements Exception {
  EggDbException(this.message, this.sqlState, this.status);
  final String message;
  final String? sqlState;
  final int status;
  @override
  String toString() => 'EggDbException($status, $sqlState): $message';
}

class StatementResult {
  StatementResult(this.tag, this.columns, this.rows);
  final String tag;
  final List<String> columns;
  final List<List<Object?>> rows;
}

String _cString(Pointer<Uint8> pointer) {
  if (pointer.address == 0) return '';
  var length = 0;
  while (pointer[length] != 0) {
    length++;
  }
  return utf8.decode(pointer.asTypedList(length));
}

class _Api {
  _Api(DynamicLibrary library)
      : open = library.lookupFunction<Int32 Function(Pointer<Uint8>, Pointer<_Options>, Pointer<Pointer<Void>>),
            int Function(Pointer<Uint8>, Pointer<_Options>, Pointer<Pointer<Void>>)>('eggdb_open'),
        close = library.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>('eggdb_close'),
        sqlOpen = library.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Pointer<Void>>),
            int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Pointer<Void>>)>('eggdb_sql_open'),
        sqlConnect = library.lookupFunction<Int32 Function(Pointer<Uint8>, Pointer<Pointer<Void>>),
            int Function(Pointer<Uint8>, Pointer<Pointer<Void>>)>('eggdb_sql_connect'),
        execute = library.lookupFunction<
            Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<_Parameter>, Size, Pointer<Pointer<_Result>>, Pointer<Size>),
            int Function(Pointer<Void>, Pointer<Uint8>, Pointer<_Parameter>, int, Pointer<Pointer<_Result>>,
                Pointer<Size>)>('eggdb_sql_execute_with_parameters'),
        sqlClose = library.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>('eggdb_sql_close'),
        lastError = library.lookupFunction<Pointer<Uint8> Function(), Pointer<Uint8> Function()>('eggdb_last_error'),
        lastSqlState = library.lookupFunction<Pointer<Uint8> Function(), Pointer<Uint8> Function()>('eggdb_last_sqlstate'),
        malloc = DynamicLibrary.process().lookupFunction<Pointer<Void> Function(Size), Pointer<Void> Function(int)>('malloc'),
        free = DynamicLibrary.process().lookupFunction<Void Function(Pointer<Void>), void Function(Pointer<Void>)>('free');

  final int Function(Pointer<Uint8>, Pointer<_Options>, Pointer<Pointer<Void>>) open;
  final int Function(Pointer<Void>) close;
  final int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Pointer<Void>>) sqlOpen;
  final int Function(Pointer<Uint8>, Pointer<Pointer<Void>>) sqlConnect;
  final int Function(Pointer<Void>, Pointer<Uint8>, Pointer<_Parameter>, int, Pointer<Pointer<_Result>>, Pointer<Size>) execute;
  final int Function(Pointer<Void>) sqlClose;
  final Pointer<Uint8> Function() lastError;
  final Pointer<Uint8> Function() lastSqlState;
  final Pointer<Void> Function(int) malloc;
  final void Function(Pointer<Void>) free;

  Pointer<Uint8> text(String value) {
    final bytes = utf8.encode(value);
    final pointer = malloc(bytes.length + 1).cast<Uint8>();
    pointer.asTypedList(bytes.length + 1)
      ..setAll(0, bytes)
      ..[bytes.length] = 0;
    return pointer;
  }

  void check(int status) {
    if (status == 0) return;
    final message = _cString(lastError());
    final state = _cString(lastSqlState());
    throw EggDbException(message.isEmpty ? 'EggDB status $status' : message, state.isEmpty ? null : state, status);
  }
}

class Database {
  Database._(this._api, this._database, this._session, this.dialect);

  factory Database.open(String pathOrUrl, {String dialect = 'sqlite', String? library}) {
    final name = library ??
        Platform.environment['EGGDB_LIBRARY'] ??
        (Platform.isMacOS ? 'libeggdb_capi.dylib' : Platform.isWindows ? 'eggdb_capi.dll' : 'libeggdb_capi.so');
    final api = _Api(_openLibrary(name));
    final out = api.malloc(sizeOf<Pointer<Void>>()).cast<Pointer<Void>>();
    final path = api.text(pathOrUrl);
    try {
      if (pathOrUrl.startsWith('postgresql://') || pathOrUrl.startsWith('postgres://')) {
        api.check(api.sqlConnect(path, out));
        return Database._(api, nullptr, out.value, 'postgresql');
      }
      final options = api.malloc(sizeOf<_Options>()).cast<_Options>();
      options.ref
        ..flags = 0
        ..durability = 0
        ..cacheBytes = 0;
      final status = api.open(path, options, out);
      api.free(options.cast());
      api.check(status);
      final database = out.value;
      final dialectText = api.text(dialect);
      final opened = api.sqlOpen(database, dialectText, out);
      api.free(dialectText.cast());
      if (opened != 0) {
        api.close(database);
        api.check(opened);
      }
      return Database._(api, database, out.value, dialect);
    } finally {
      api.free(path.cast());
      api.free(out.cast());
    }
  }

  final _Api _api;
  final Pointer<Void> _database;
  Pointer<Void> _session;
  final String dialect;

  String _numbered(String sql) {
    if (dialect != 'postgresql') return sql;
    final out = StringBuffer();
    String? quote;
    var number = 0;
    for (final character in sql.split('')) {
      if (quote != null) {
        if (character == quote) quote = null;
        out.write(character);
      } else if (character == "'" || character == '"') {
        quote = character;
        out.write(character);
      } else if (character == '?') {
        number++;
        out.write('\$$number');
      } else {
        out.write(character);
      }
    }
    return out.toString();
  }

  List<StatementResult> execute(String sql, [List<Object?> parameters = const []]) {
    final count = parameters.length;
    final array = _api.malloc(sizeOf<_Parameter>() * (count == 0 ? 1 : count)).cast<_Parameter>();
    final kept = <Pointer<Uint8>>[];
    for (var index = 0; index < count; index++) {
      final value = parameters[index];
      final slot = (array + index).ref;
      if (value == null) {
        slot
          ..text = nullptr
          ..len = 0;
        continue;
      }
      final text = value is bool ? (dialect == 'postgresql' ? '$value' : (value ? '1' : '0')) : '$value';
      final pointer = _api.text(text);
      kept.add(pointer);
      slot
        ..text = pointer
        ..len = utf8.encode(text).length;
    }
    final results = _api.malloc(sizeOf<Pointer<_Result>>()).cast<Pointer<_Result>>();
    final resultCount = _api.malloc(sizeOf<Size>()).cast<Size>();
    final statement = _api.text(_numbered(sql));
    try {
      _api.check(_api.execute(_session, statement, array, count, results, resultCount));
      final gathered = <StatementResult>[];
      for (var index = 0; index < resultCount.value; index++) {
        final result = (results.value + index).ref;
        final columns = [for (var column = 0; column < result.columnCount; column++) _cString(result.columnNames[column])];
        final rows = <List<Object?>>[];
        for (var row = 0; row < result.rowCount; row++) {
          final cells = <Object?>[];
          for (var column = 0; column < result.columnCount; column++) {
            final cell = (result.values + row * result.columnCount + column).ref;
            cells.add(switch (cell.kind) {
              1 => cell.integer,
              2 => cell.real,
              3 => utf8.decode(cell.data.asTypedList(cell.len)),
              4 => Uint8List.fromList(cell.data.asTypedList(cell.len)),
              _ => null,
            });
          }
          rows.add(cells);
        }
        gathered.add(StatementResult(_cString(result.tag), columns, rows));
      }
      return gathered;
    } finally {
      for (final pointer in kept) {
        _api.free(pointer.cast());
      }
      _api.free(array.cast());
      _api.free(results.cast());
      _api.free(resultCount.cast());
      _api.free(statement.cast());
    }
  }

  void close() {
    if (_session == nullptr) return;
    _api.sqlClose(_session);
    if (_database != nullptr) _api.close(_database);
    _session = nullptr;
  }
}

final class _Entry extends Struct {
  external Pointer<Uint8> key;
  @Size()
  external int keyLen;
  external Pointer<Uint8> value;
  @Size()
  external int valueLen;
}

final class _CollectionStats extends Struct {
  @Uint64()
  external int entries;
  @Uint64()
  external int keyBytes;
  @Uint64()
  external int valueBytes;
}

typedef _TxnBytesNative = Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, Size, Pointer<Pointer<Uint8>>, Pointer<Size>);
typedef _TxnBytes = int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, int, Pointer<Pointer<Uint8>>, Pointer<Size>);
typedef _PutNative = Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, Size, Pointer<Uint8>, Size);
typedef _Put = int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, int, Pointer<Uint8>, int);
typedef _PutWithExpiryNative = Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, Size, Pointer<Uint8>, Size, Uint64);
typedef _PutWithExpiry = int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, int, Pointer<Uint8>, int, int);
typedef _DeleteNative = Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, Size, Pointer<Int32>);
typedef _Delete = int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, int, Pointer<Int32>);
typedef _ScanOpenNative = Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, Size, Pointer<Uint8>, Size, Uint32, Pointer<Pointer<Void>>);
typedef _ScanOpen = int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>, int, Pointer<Uint8>, int, int, Pointer<Pointer<Void>>);
typedef _ScanNextNative = Int32 Function(Pointer<Void>, Pointer<Void>, Size, Pointer<Pointer<_Entry>>, Pointer<Size>);
typedef _ScanNext = int Function(Pointer<Void>, Pointer<Void>, int, Pointer<Pointer<_Entry>>, Pointer<Size>);
typedef _NamedFlagNative = Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Int32>);
typedef _NamedFlag = int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Int32>);

class _KeyValueApi {
  _KeyValueApi(DynamicLibrary library)
      : api = _Api(library),
        freeBuffer = library.lookupFunction<Void Function(Pointer<Uint8>, Size), void Function(Pointer<Uint8>, int)>('eggdb_free'),
        beginRead = library.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Pointer<Void>>),
            int Function(Pointer<Void>, Pointer<Pointer<Void>>)>('eggdb_begin_read'),
        beginWrite = library.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Pointer<Void>>),
            int Function(Pointer<Void>, Pointer<Pointer<Void>>)>('eggdb_begin_write'),
        commit = library.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Uint64>), int Function(Pointer<Void>, Pointer<Uint64>)>(
            'eggdb_commit'),
        rollback = library.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>('eggdb_rollback'),
        get = library.lookupFunction<_TxnBytesNative, _TxnBytes>('eggdb_get'),
        put = library.lookupFunction<_PutNative, _Put>('eggdb_put'),
        putWithExpiry = library.lookupFunction<_PutWithExpiryNative, _PutWithExpiry>('eggdb_put_with_expiry'),
        delete = library.lookupFunction<_DeleteNative, _Delete>('eggdb_delete'),
        scanOpen = library.lookupFunction<_ScanOpenNative, _ScanOpen>('eggdb_scan_open'),
        scanNext = library.lookupFunction<_ScanNextNative, _ScanNext>('eggdb_scan_next'),
        scanFree = library.lookupFunction<Void Function(Pointer<Void>), void Function(Pointer<Void>)>('eggdb_scan_free'),
        createCollection = library.lookupFunction<_NamedFlagNative, _NamedFlag>('eggdb_create_collection'),
        dropCollection = library.lookupFunction<_NamedFlagNative, _NamedFlag>('eggdb_drop_collection'),
        renameCollection = library.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>),
            int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>)>('eggdb_rename_collection'),
        collectionNames = library.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Size>),
            int Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Size>)>('eggdb_collection_names'),
        collectionStats = library.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Uint8>, Pointer<_CollectionStats>),
            int Function(Pointer<Void>, Pointer<Uint8>, Pointer<_CollectionStats>)>('eggdb_collection_stats');

  final _Api api;
  final void Function(Pointer<Uint8>, int) freeBuffer;
  final int Function(Pointer<Void>, Pointer<Pointer<Void>>) beginRead;
  final int Function(Pointer<Void>, Pointer<Pointer<Void>>) beginWrite;
  final int Function(Pointer<Void>, Pointer<Uint64>) commit;
  final int Function(Pointer<Void>) rollback;
  final _TxnBytes get;
  final _Put put;
  final _PutWithExpiry putWithExpiry;
  final _Delete delete;
  final _ScanOpen scanOpen;
  final _ScanNext scanNext;
  final void Function(Pointer<Void>) scanFree;
  final _NamedFlag createCollection;
  final _NamedFlag dropCollection;
  final int Function(Pointer<Void>, Pointer<Uint8>, Pointer<Uint8>) renameCollection;
  final int Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Size>) collectionNames;
  final int Function(Pointer<Void>, Pointer<Uint8>, Pointer<_CollectionStats>) collectionStats;

  Pointer<Uint8> bytes(List<int>? data) {
    if (data == null || data.isEmpty) return nullptr;
    final pointer = api.malloc(data.length).cast<Uint8>();
    pointer.asTypedList(data.length).setAll(0, data);
    return pointer;
  }

  Pointer<Uint8> name(String? collection) => collection == null ? nullptr : api.text(collection);

  void release(Pointer<Uint8> pointer) {
    if (pointer != nullptr) api.free(pointer.cast());
  }
}

class CollectionStats {
  CollectionStats(this.entries, this.keyBytes, this.valueBytes);
  final int entries;
  final int keyBytes;
  final int valueBytes;
}

class KeyValueEntry {
  KeyValueEntry(this.key, this.value);
  final Uint8List key;
  final Uint8List value;
}

class KeyValue {
  KeyValue._(this._api, this._database, this.readOnly);

  factory KeyValue.open(String path, {bool readOnly = false, String? library}) {
    final name = library ??
        Platform.environment['EGGDB_LIBRARY'] ??
        (Platform.isMacOS ? 'libeggdb_capi.dylib' : Platform.isWindows ? 'eggdb_capi.dll' : 'libeggdb_capi.so');
    final kv = _KeyValueApi(_openLibrary(name));
    final api = kv.api;
    final out = api.malloc(sizeOf<Pointer<Void>>()).cast<Pointer<Void>>();
    final options = api.malloc(sizeOf<_Options>()).cast<_Options>();
    final text = api.text(path);
    try {
      options.ref
        ..flags = readOnly ? 1 : 0
        ..durability = 0
        ..cacheBytes = 0;
      api.check(api.open(text, options, out));
      return KeyValue._(kv, out.value, readOnly);
    } finally {
      api.free(text.cast());
      api.free(options.cast());
      api.free(out.cast());
    }
  }

  final _KeyValueApi _api;
  Pointer<Void> _database;
  final bool readOnly;

  Transaction begin({required bool writing}) {
    if (_database == nullptr) throw EggDbException('the store is closed', null, 0);
    final out = _api.api.malloc(sizeOf<Pointer<Void>>()).cast<Pointer<Void>>();
    try {
      _api.api.check(writing ? _api.beginWrite(_database, out) : _api.beginRead(_database, out));
      return Transaction._(_api, out.value, writing);
    } finally {
      _api.api.free(out.cast());
    }
  }

  T read<T>(T Function(Transaction) work) {
    final transaction = begin(writing: false);
    try {
      return work(transaction);
    } finally {
      transaction.rollback();
    }
  }

  T write<T>(T Function(Transaction) work) {
    final transaction = begin(writing: true);
    try {
      final result = work(transaction);
      transaction.commit();
      return result;
    } finally {
      transaction.rollback();
    }
  }

  void close() {
    if (_database == nullptr) return;
    final closing = _database;
    _database = nullptr;
    _api.api.check(_api.api.close(closing));
  }
}

class Transaction {
  Transaction._(this._api, this._handle, this.writing);

  final _KeyValueApi _api;
  Pointer<Void> _handle;
  final bool writing;

  static const _scanReverse = 1;
  static const _scanEndInclusive = 2;
  static const _scanPrefix = 4;

  Pointer<Void> _live() {
    if (_handle == nullptr) throw EggDbException('the transaction has ended', null, 0);
    return _handle;
  }

  Uint8List? get(List<int> key, {String? collection}) {
    final txn = _live();
    final api = _api.api;
    final name = _api.name(collection);
    final keyData = _api.bytes(key);
    final data = api.malloc(sizeOf<Pointer<Uint8>>()).cast<Pointer<Uint8>>();
    final length = api.malloc(sizeOf<Size>()).cast<Size>();
    try {
      api.check(_api.get(txn, name, keyData, key.length, data, length));
      if (data.value == nullptr) return null;
      final value = Uint8List.fromList(data.value.asTypedList(length.value));
      _api.freeBuffer(data.value, length.value);
      return value;
    } finally {
      _api.release(name);
      _api.release(keyData);
      api.free(data.cast());
      api.free(length.cast());
    }
  }

  String? getText(String key, {String? collection}) {
    final value = get(utf8.encode(key), collection: collection);
    return value == null ? null : utf8.decode(value);
  }

  void put(List<int> key, List<int> value, {String? collection, Duration? timeToLive}) {
    final txn = _live();
    final name = _api.name(collection);
    final keyData = _api.bytes(key);
    final valueData = _api.bytes(value);
    try {
      final milliseconds = timeToLive?.inMilliseconds ?? 0;
      _api.api.check(milliseconds > 0
          ? _api.putWithExpiry(txn, name, keyData, key.length, valueData, value.length, milliseconds)
          : _api.put(txn, name, keyData, key.length, valueData, value.length));
    } finally {
      _api.release(name);
      _api.release(keyData);
      _api.release(valueData);
    }
  }

  void putText(String key, String value, {String? collection, Duration? timeToLive}) =>
      put(utf8.encode(key), utf8.encode(value), collection: collection, timeToLive: timeToLive);

  bool delete(List<int> key, {String? collection}) {
    final txn = _live();
    final name = _api.name(collection);
    final keyData = _api.bytes(key);
    final deleted = _api.api.malloc(sizeOf<Int32>()).cast<Int32>();
    try {
      _api.api.check(_api.delete(txn, name, keyData, key.length, deleted));
      return deleted.value != 0;
    } finally {
      _api.release(name);
      _api.release(keyData);
      _api.api.free(deleted.cast());
    }
  }

  List<KeyValueEntry> scan({
    List<int>? start,
    List<int>? end,
    List<int>? prefix,
    bool reverse = false,
    bool endInclusive = false,
    int limit = 0,
    String? collection,
  }) {
    final txn = _live();
    final api = _api.api;
    var flags = (reverse ? _scanReverse : 0) | (endInclusive ? _scanEndInclusive : 0);
    if (prefix != null) {
      flags |= _scanPrefix;
      start = prefix;
      end = null;
    }
    final name = _api.name(collection);
    final startData = _api.bytes(start);
    final endData = _api.bytes(end);
    final out = api.malloc(sizeOf<Pointer<Void>>()).cast<Pointer<Void>>();
    final entries = api.malloc(sizeOf<Pointer<_Entry>>()).cast<Pointer<_Entry>>();
    final count = api.malloc(sizeOf<Size>()).cast<Size>();
    final gathered = <KeyValueEntry>[];
    try {
      api.check(_api.scanOpen(txn, name, startData, start?.length ?? 0, endData, end?.length ?? 0, flags, out));
      final scan = out.value;
      try {
        while (limit <= 0 || gathered.length < limit) {
          api.check(_api.scanNext(txn, scan, 0, entries, count));
          final total = count.value;
          if (total == 0) break;
          for (var index = 0; index < total && (limit <= 0 || gathered.length < limit); index++) {
            final entry = (entries.value + index).ref;
            gathered.add(KeyValueEntry(
              entry.keyLen == 0 ? Uint8List(0) : Uint8List.fromList(entry.key.asTypedList(entry.keyLen)),
              entry.valueLen == 0 ? Uint8List(0) : Uint8List.fromList(entry.value.asTypedList(entry.valueLen)),
            ));
          }
        }
      } finally {
        _api.scanFree(scan);
      }
      return gathered;
    } finally {
      _api.release(name);
      _api.release(startData);
      _api.release(endData);
      api.free(out.cast());
      api.free(entries.cast());
      api.free(count.cast());
    }
  }

  bool _namedFlag(String collection, _NamedFlag call) {
    final txn = _live();
    final name = _api.api.text(collection);
    final flag = _api.api.malloc(sizeOf<Int32>()).cast<Int32>();
    try {
      _api.api.check(call(txn, name, flag));
      return flag.value != 0;
    } finally {
      _api.api.free(name.cast());
      _api.api.free(flag.cast());
    }
  }

  bool createCollection(String name) => _namedFlag(name, _api.createCollection);

  bool dropCollection(String name) => _namedFlag(name, _api.dropCollection);

  void renameCollection(String name, String newName) {
    final txn = _live();
    final from = _api.api.text(name);
    final to = _api.api.text(newName);
    try {
      _api.api.check(_api.renameCollection(txn, from, to));
    } finally {
      _api.api.free(from.cast());
      _api.api.free(to.cast());
    }
  }

  List<String> collections() {
    final txn = _live();
    final api = _api.api;
    final data = api.malloc(sizeOf<Pointer<Uint8>>()).cast<Pointer<Uint8>>();
    final length = api.malloc(sizeOf<Size>()).cast<Size>();
    final count = api.malloc(sizeOf<Size>()).cast<Size>();
    try {
      api.check(_api.collectionNames(txn, data, length, count));
      if (data.value == nullptr) return [];
      final joined = utf8.decode(data.value.asTypedList(length.value));
      _api.freeBuffer(data.value, length.value);
      return joined.split('\u0000').take(count.value).toList();
    } finally {
      api.free(data.cast());
      api.free(length.cast());
      api.free(count.cast());
    }
  }

  CollectionStats collectionStats(String name) {
    final txn = _live();
    final api = _api.api;
    final text = api.text(name);
    final stats = api.malloc(sizeOf<_CollectionStats>()).cast<_CollectionStats>();
    try {
      api.check(_api.collectionStats(txn, text, stats));
      return CollectionStats(stats.ref.entries, stats.ref.keyBytes, stats.ref.valueBytes);
    } finally {
      api.free(text.cast());
      api.free(stats.cast());
    }
  }

  int commit() {
    final txn = _live();
    _handle = nullptr;
    final stamp = _api.api.malloc(sizeOf<Uint64>()).cast<Uint64>();
    try {
      _api.api.check(_api.commit(txn, stamp));
      return stamp.value;
    } finally {
      _api.api.free(stamp.cast());
    }
  }

  void rollback() {
    if (_handle == nullptr) return;
    final ending = _handle;
    _handle = nullptr;
    _api.rollback(ending);
  }
}
