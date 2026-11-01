import 'dart:io';

import '../lib/eggdb.dart';

void require(bool condition, String what) {
  if (!condition) {
    stderr.writeln('dart driver check failed: $what');
    exit(1);
  }
}

void main() {
  final db = Database.open(':memory:');
  db.execute('CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
  db.execute('INSERT INTO people (name, score) VALUES (?, ?)', ['Ada', 1.5]);
  db.execute('INSERT INTO people (name, score) VALUES (?, ?)', [null, 2]);
  final rows = db.execute('SELECT id, name, score FROM people ORDER BY id').last.rows;
  require(rows[0][0] == 1 && rows[0][1] == 'Ada' && rows[0][2] == 1.5, 'the first row');
  require(rows[1][1] == null, 'a NULL name');
  try {
    db.execute('SELECT * FROM nowhere');
    require(false, 'a missing table fails');
  } on EggDbException catch (error) {
    require(error.sqlState?.length == 5, 'the error has a SQLSTATE');
  }
  db.close();
  print('dart driver check: passed');
}
