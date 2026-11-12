import 'dart:io';

import 'package:eggdb/eggdb.dart';

export 'package:eggdb/eggdb.dart';

String bundledLibrary() {
  if (Platform.isAndroid) return 'libeggdb_capi.so';
  if (Platform.isIOS || Platform.isMacOS) return processLibrary;
  if (Platform.isWindows) return 'eggdb_capi.dll';
  return 'libeggdb_capi.so';
}

Database openDatabase(String path, {String dialect = 'sqlite'}) =>
    Database.open(path, dialect: dialect, library: bundledLibrary());

KeyValue openKeyValue(String path, {bool readOnly = false}) =>
    KeyValue.open(path, readOnly: readOnly, library: bundledLibrary());
