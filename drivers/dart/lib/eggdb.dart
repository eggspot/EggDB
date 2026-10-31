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
    final api = _Api(DynamicLibrary.open(name));
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
