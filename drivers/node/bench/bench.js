'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { DatabaseSync } = require('node:sqlite');
const { Database } = require('..');

const ROWS = Number(process.env.DRIVER_BENCH_ROWS || 20000);
const LOOKUPS = Number(process.env.DRIVER_BENCH_LOOKUPS || 5000);

function timed(work) {
  const started = process.hrtime.bigint();
  work();
  return Number(process.hrtime.bigint() - started) / 1e6;
}

function keys() {
  return Array.from({ length: LOOKUPS }, (_, at) => ((at + 1) * 7) % ROWS + 1);
}

function eggdbSide(file) {
  const db = new Database(file);
  db.run('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
  const insert = timed(() => {
    db.transaction(() => {
      const statement = db.prepare('INSERT INTO t VALUES (?, ?, ?)');
      for (let at = 1; at <= ROWS; at += 1) {
        statement.run([at, `name ${at}`, at * 0.25]);
      }
      statement.finalize();
    });
  });
  const lookup = timed(() => {
    const statement = db.prepare('SELECT name FROM t WHERE id = ?');
    for (const key of keys()) {
      for (const row of statement.iterate([key])) {
        void row;
      }
    }
    statement.finalize();
  });
  const scan = timed(() => {
    let total = 0;
    for (const row of db.iterate('SELECT name FROM t')) {
      total += row[0].length;
    }
    return total;
  });
  db.close();
  return [insert, lookup, scan];
}

function sqliteSide(file) {
  const db = new DatabaseSync(file);
  db.exec('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
  const insert = timed(() => {
    db.exec('BEGIN');
    const statement = db.prepare('INSERT INTO t VALUES (?, ?, ?)');
    for (let at = 1; at <= ROWS; at += 1) {
      statement.run(at, `name ${at}`, at * 0.25);
    }
    db.exec('COMMIT');
  });
  const lookup = timed(() => {
    const statement = db.prepare('SELECT name FROM t WHERE id = ?');
    for (const key of keys()) {
      statement.all(key);
    }
  });
  const scan = timed(() => {
    let total = 0;
    for (const row of db.prepare('SELECT name FROM t').iterate()) {
      total += row.name.length;
    }
    return total;
  });
  db.close();
  return [insert, lookup, scan];
}

const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'eggdb-node-bench-'));
const egg = eggdbSide(path.join(directory, 'bench.eggdb'));
const lite = sqliteSide(path.join(directory, 'bench.sqlite'));
['insert', 'lookup', 'scan'].forEach((workload, at) => {
  console.log(`node ${workload} ${egg[at].toFixed(2)} ${lite[at].toFixed(2)}`);
});
fs.rmSync(directory, { recursive: true, force: true });
