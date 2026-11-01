'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { Database, EggDbError } = require('..');

function temporaryDatabase() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'eggdb-node-'));
  return path.join(directory, 'app.db');
}

test('rows come back with their JavaScript types', () => {
  const db = new Database(temporaryDatabase());
  db.run('CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, photo BLOB, big INTEGER)');
  const inserted = db.run('INSERT INTO people (name, score, photo, big) VALUES (?, ?, ?, ?)', [
    'Ada',
    1.5,
    Buffer.from([0, 255]),
    9007199254740993n,
  ]);
  assert.equal(inserted.changes, 1);
  const [row] = db.query('SELECT id, name, score, photo, big FROM people');
  assert.equal(row.id, 1);
  assert.equal(row.name, 'Ada');
  assert.equal(row.score, 1.5);
  assert.deepEqual([...row.photo], [0, 255]);
  assert.equal(row.big, 9007199254740993n);
  db.close();
});

test('a transaction that throws leaves nothing behind', () => {
  const db = new Database(temporaryDatabase());
  db.run('CREATE TABLE t (n INTEGER)');
  db.transaction(() => db.run('INSERT INTO t VALUES (?)', [1]));
  assert.throws(() =>
    db.transaction(() => {
      db.run('INSERT INTO t VALUES (?)', [2]);
      throw new Error('stop');
    }),
  );
  assert.deepEqual(db.query('SELECT n FROM t'), [{ n: 1 }]);
  db.close();
});

test('errors carry the SQLSTATE', () => {
  const db = new Database(temporaryDatabase());
  assert.throws(
    () => db.query('SELECT * FROM nowhere'),
    (error) => error instanceof EggDbError && /nowhere/.test(error.message) && error.sqlstate.length === 5,
  );
  db.close();
  assert.throws(() => db.query('SELECT 1'), EggDbError);
});

test('question marks are numbered for the PostgreSQL dialect', () => {
  const db = new Database(':memory:', { dialect: 'postgresql' });
  assert.deepEqual(db.query("SELECT CAST(? AS integer) + CAST(? AS integer) AS sum, '?' AS mark", [2, 3]), [{ sum: 5, mark: '?' }]);
  db.close();
});
