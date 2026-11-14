import 'dart:io';

import 'package:eggdb/eggdb.dart' as egg;
import 'package:sqlite3/sqlite3.dart' as lite;

final rows = int.tryParse(Platform.environment['DRIVER_BENCH_ROWS'] ?? '') ?? 20000;
final lookups = int.tryParse(Platform.environment['DRIVER_BENCH_LOOKUPS'] ?? '') ?? 5000;

double timed(void Function() work) {
  final watch = Stopwatch()..start();
  work();
  return watch.elapsedMicroseconds / 1000;
}

List<double> eggdbSide(String path) {
  final db = egg.Database.open(path);
  db.execute('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
  final insert = timed(() {
    db.execute('BEGIN');
    final statement = db.prepare('INSERT INTO t VALUES (?, ?, ?)');
    for (var at = 1; at <= rows; at++) {
      statement.execute([at, 'name $at', at * 0.25]);
    }
    statement.finalize();
    db.execute('COMMIT');
  });
  final lookup = timed(() {
    final statement = db.prepare('SELECT name FROM t WHERE id = ?');
    for (var at = 1; at <= lookups; at++) {
      statement.rows([at * 7 % rows + 1]).length;
    }
    statement.finalize();
  });
  final scan = timed(() {
    var total = 0;
    for (final row in db.iterate('SELECT name FROM t')) {
      total += (row[0] as String).length;
    }
    if (total < 0) throw StateError('impossible');
  });
  db.close();
  return [insert, lookup, scan];
}

List<double> sqliteSide(String path) {
  final db = lite.sqlite3.open(path);
  db.execute('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
  final insert = timed(() {
    db.execute('BEGIN');
    final statement = db.prepare('INSERT INTO t VALUES (?, ?, ?)');
    for (var at = 1; at <= rows; at++) {
      statement.execute([at, 'name $at', at * 0.25]);
    }
    statement.dispose();
    db.execute('COMMIT');
  });
  final lookup = timed(() {
    final statement = db.prepare('SELECT name FROM t WHERE id = ?');
    for (var at = 1; at <= lookups; at++) {
      statement.select([at * 7 % rows + 1]).length;
    }
    statement.dispose();
  });
  final scan = timed(() {
    var total = 0;
    final statement = db.prepare('SELECT name FROM t');
    final cursor = statement.selectCursor();
    while (cursor.moveNext()) {
      total += (cursor.current['name'] as String).length;
    }
    statement.dispose();
    if (total < 0) throw StateError('impossible');
  });
  db.dispose();
  return [insert, lookup, scan];
}

void main() {
  final directory = Directory.systemTemp.createTempSync('eggdb-dart-bench');
  final mine = eggdbSide('${directory.path}/bench.eggdb');
  final theirs = sqliteSide('${directory.path}/bench.sqlite');
  const workloads = ['insert', 'lookup', 'scan'];
  for (var at = 0; at < workloads.length; at++) {
    print('dart ${workloads[at]} ${mine[at].toStringAsFixed(2)} ${theirs[at].toStringAsFixed(2)}');
  }
  directory.deleteSync(recursive: true);
}
