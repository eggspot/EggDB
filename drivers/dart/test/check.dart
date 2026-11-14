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
  final insert = db.prepare('INSERT INTO people (name, score) VALUES (?, ?)');
  require(insert.parameterCount == 2, 'two parameters');
  insert.execute(['Grace', 3.25]);
  insert.execute(['Edsger', 4]);
  insert.finalize();
  final names = [for (final row in db.iterate('SELECT name FROM people WHERE score > ? ORDER BY id', [3])) row[0]];
  require(names.join(',') == 'Grace,Edsger', 'stepped rows');
  db.close();

  final pg = Database.open(':memory:', dialect: 'postgresql');
  pg.execute('CREATE TABLE t (id integer, name text)');
  require(pg.copyFromText('COPY t FROM STDIN', '1\tada\n2\tgrace\n') == 2, 'COPY loads two rows');
  final copied = pg.copyTo('COPY t TO STDOUT').expand((piece) => piece).toList();
  require(String.fromCharCodes(copied) == '1\tada\n2\tgrace\n', 'COPY reads the rows back');
  pg.close();

  final path = '${Directory.systemTemp.path}/eggdb-dart-kv-$pid.eggdb';
  final store = KeyValue.open(path);
  store.write((txn) {
    txn.putText('a', '1');
    txn.put('b'.codeUnits, [0, 2]);
    require(txn.createCollection('users'), 'a collection is created');
    txn.putText('u1', 'Ada', collection: 'users');
  });
  store.read((txn) {
    require(txn.getText('a') == '1', 'a stored value');
    require(txn.get('missing'.codeUnits) == null, 'a missing key');
    final all = txn.scan();
    require(all.length == 2 && all[1].value.length == 2 && all[1].value[0] == 0, 'a scan with binary values');
    require(String.fromCharCodes(txn.scan(reverse: true, limit: 1).single.key) == 'b', 'a reversed scan');
    require(txn.collections().join(',') == 'users', 'the collection names');
    require(txn.collectionStats('users').entries == 1, 'the collection stats');
  });
  store.write((txn) {
    require(txn.delete('a'.codeUnits), 'a key is deleted');
    txn.renameCollection('users', 'people');
  });
  store.read((txn) {
    require(txn.getText('a') == null, 'the deleted key is gone');
    require(txn.getText('u1', collection: 'people') == 'Ada', 'the renamed collection keeps its keys');
  });
  store.close();
  File(path).deleteSync();
  print('dart driver check: passed');
}
