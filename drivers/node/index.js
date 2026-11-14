'use strict';

const path = require('node:path');

const DIALECTS = new Set(['sqlite', 'postgresql', 'mysql', 'oracle', 'sqlserver']);

function loadAddon() {
  const candidates = [
    process.env.EGGDB_NODE_ADDON,
    path.join(__dirname, 'build', 'Release', 'eggdb.node'),
    path.join(__dirname, 'eggdb.node'),
  ].filter(Boolean);
  let lastError;
  for (const candidate of candidates) {
    try {
      return require(candidate);
    } catch (error) {
      lastError = error;
    }
  }
  throw new Error(`cannot load EggDB's Node.js addon; build it or set EGGDB_NODE_ADDON (${lastError && lastError.message})`);
}

const addon = loadAddon();

class EggDbError extends Error {
  constructor(cause) {
    super(cause.message);
    this.name = 'EggDbError';
    this.sqlstate = cause.code || null;
    this.status = cause.status;
  }
}

function parameterText(value, dialect) {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value === 'boolean') {
    if (dialect === 'postgresql') {
      return value ? 'true' : 'false';
    }
    return value ? '1' : '0';
  }
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) {
      throw new TypeError(`cannot bind ${value}`);
    }
    return String(value);
  }
  if (typeof value === 'bigint') {
    return value.toString();
  }
  if (value instanceof Uint8Array) {
    return `\\x${hexOf(value)}`;
  }
  if (value instanceof Date) {
    return value.toISOString().replace('T', ' ').replace('Z', '');
  }
  return String(value);
}

function hexOf(value) {
  return Buffer.from(value.buffer, value.byteOffset, value.byteLength).toString('hex');
}

function bound(sql, dialect, parameters) {
  const postgresql = dialect === 'postgresql';
  const kept = [];
  let out = '';
  let quote = null;
  let given = 0;
  for (const character of sql) {
    if (quote) {
      if (character === quote) {
        quote = null;
      }
      out += character;
    } else if (character === "'" || character === '"') {
      quote = character;
      out += character;
    } else if (character === '?') {
      const value = parameters[given];
      given += 1;
      if (!postgresql && value instanceof Uint8Array) {
        out += `X'${hexOf(value)}'`;
      } else {
        kept.push(value);
        out += postgresql ? `$${kept.length}` : '?';
      }
    } else {
      out += character;
    }
  }
  return { sql: out, parameters: kept.concat(parameters.slice(given)) };
}

function changedRows(tag) {
  const words = tag.split(' ');
  if (words.length > 1 && ['INSERT', 'UPDATE', 'DELETE'].includes(words[0])) {
    const count = Number(words[words.length - 1]);
    return Number.isInteger(count) ? count : 0;
  }
  return 0;
}

class Database {
  constructor(file, options = {}) {
    const server = /^postgres(ql)?:\/\//.test(String(file));
    const dialect = server ? 'postgresql' : options.dialect || 'sqlite';
    if (!DIALECTS.has(dialect)) {
      throw new TypeError(`unknown dialect ${dialect}`);
    }
    this.dialect = dialect;
    this.readOnly = Boolean(options.readOnly);
    this.inTransaction = false;
    this.statements = new Set();
    try {
      this.handle = addon.open(String(file), dialect, this.readOnly);
    } catch (error) {
      throw new EggDbError(error);
    }
  }

  execute(sql, parameters = []) {
    if (!this.handle) {
      throw new EggDbError({ message: 'the database is closed', code: '08003' });
    }
    const statement = bound(sql, this.dialect, parameters);
    const texts = statement.parameters.map((value) => parameterText(value, this.dialect));
    try {
      return addon.execute(this.handle, statement.sql, texts);
    } catch (error) {
      throw new EggDbError(error);
    }
  }

  query(sql, parameters = []) {
    const results = this.execute(sql, parameters);
    for (let index = results.length - 1; index >= 0; index -= 1) {
      const { columns, rows } = results[index];
      if (columns.length > 0) {
        return rows.map((cells) => Object.fromEntries(columns.map((column, at) => [column, cells[at]])));
      }
    }
    return [];
  }

  run(sql, parameters = []) {
    const results = this.execute(sql, parameters);
    return { changes: results.reduce((total, result) => total + changedRows(result.tag), 0) };
  }

  transaction(body) {
    if (this.inTransaction) {
      return body(this);
    }
    this.execute('BEGIN');
    this.inTransaction = true;
    try {
      const value = body(this);
      this.execute('COMMIT');
      return value;
    } catch (error) {
      this.execute('ROLLBACK');
      throw error;
    } finally {
      this.inTransaction = false;
    }
  }

  live() {
    if (!this.handle) {
      throw new EggDbError({ message: 'the database is closed', code: '08003' });
    }
    return this.handle;
  }

  prepare(sql) {
    const numbered = bound(sql, this.dialect, []).sql;
    const statement = new Statement(this, native(() => addon.prepare(this.live(), numbered)));
    this.statements.add(statement);
    return statement;
  }

  *iterate(sql, parameters = []) {
    const statement = this.prepare(sql);
    try {
      yield* statement.iterate(parameters);
    } finally {
      statement.finalize();
    }
  }

  copyFrom(sql, data) {
    const pieces = typeof data === 'string' || data instanceof Uint8Array ? [data] : Array.from(data);
    const results = native(() => addon.copyIn(this.live(), sql, pieces));
    return copiedRows(results);
  }

  copyTo(sql, onPiece) {
    if (onPiece) {
      native(() => addon.copyOut(this.live(), sql, onPiece));
      return undefined;
    }
    const pieces = [];
    native(() => addon.copyOut(this.live(), sql, (piece) => pieces.push(piece)));
    return Buffer.concat(pieces);
  }

  close() {
    if (this.handle) {
      for (const statement of this.statements) {
        statement.finalize();
      }
      addon.close(this.handle);
      this.handle = null;
    }
  }
}

function copiedRows(results) {
  for (let index = results.length - 1; index >= 0; index -= 1) {
    const words = results[index].tag.split(' ');
    if (words.length === 2 && words[0] === 'COPY') {
      return Number(words[1]);
    }
  }
  return -1;
}

class Statement {
  constructor(database, handle) {
    this.database = database;
    this.handle = handle;
    this.parameterCount = native(() => addon.stmtParameterCount(handle));
  }

  live() {
    if (!this.handle) {
      throw new EggDbError({ message: 'the statement is finalized', code: '26000' });
    }
    return this.handle;
  }

  bind(parameters = []) {
    const values = parameters.map((value) => (value instanceof Date ? parameterText(value, this.database.dialect) : value));
    native(() => addon.stmtBind(this.live(), values));
    return this;
  }

  execute(parameters = []) {
    this.bind(parameters);
    return native(() => addon.stmtExecute(this.handle));
  }

  run(parameters = []) {
    const results = this.execute(parameters);
    return { changes: results.reduce((total, result) => total + changedRows(result.tag), 0) };
  }

  columns() {
    return native(() => addon.stmtColumns(this.live()));
  }

  *iterate(parameters = []) {
    this.bind(parameters);
    try {
      for (;;) {
        const row = native(() => addon.stmtStep(this.live()));
        if (row === null) {
          return;
        }
        yield row;
      }
    } finally {
      if (this.handle) {
        native(() => addon.stmtReset(this.handle));
      }
    }
  }

  all(parameters = []) {
    const columns = this.columns();
    return Array.from(this.iterate(parameters), (cells) => Object.fromEntries(columns.map((column, at) => [column, cells[at]])));
  }

  finalize() {
    if (this.handle) {
      addon.stmtFinalize(this.handle);
      this.handle = null;
      this.database.statements.delete(this);
    }
  }
}

const SCAN_REVERSE = 1;
const SCAN_END_INCLUSIVE = 2;
const SCAN_PREFIX = 4;

function native(call) {
  try {
    return call();
  } catch (error) {
    throw error && error.status !== undefined ? new EggDbError(error) : error;
  }
}

function bytesOf(value) {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value === 'string') {
    return Buffer.from(value, 'utf8');
  }
  if (value instanceof Uint8Array) {
    return Buffer.from(value.buffer, value.byteOffset, value.byteLength);
  }
  throw new TypeError('keys and values are strings, Buffers or Uint8Arrays');
}

class Transaction {
  constructor(handle, writing) {
    this.handle = handle;
    this.writing = writing;
  }

  get(key, collection = null) {
    return native(() => addon.kvGet(this.handle, collection, bytesOf(key)));
  }

  put(key, value, collection = null, ttlMs = null) {
    native(() => addon.kvPut(this.handle, collection, bytesOf(key), bytesOf(value), ttlMs));
  }

  delete(key, collection = null) {
    return native(() => addon.kvDelete(this.handle, collection, bytesOf(key)));
  }

  scan(options = {}) {
    const { start = null, end = null, prefix = null, reverse = false, endInclusive = false, collection = null } = options;
    let flags = (reverse ? SCAN_REVERSE : 0) | (endInclusive ? SCAN_END_INCLUSIVE : 0);
    let from = start;
    let to = end;
    if (prefix !== null && prefix !== undefined) {
      flags |= SCAN_PREFIX;
      from = prefix;
      to = null;
    }
    return native(() => addon.kvScan(this.handle, collection, bytesOf(from), bytesOf(to), flags));
  }

  createCollection(name) {
    return native(() => addon.kvCollection(this.handle, 'create', name));
  }

  dropCollection(name) {
    return native(() => addon.kvCollection(this.handle, 'drop', name));
  }

  renameCollection(name, newName) {
    native(() => addon.kvCollection(this.handle, 'rename', name, newName));
  }

  collectionStats(name) {
    return native(() => addon.kvCollection(this.handle, 'stats', name));
  }

  collections() {
    return native(() => addon.kvCollections(this.handle));
  }

  commit() {
    return native(() => addon.kvEnd(this.handle, true));
  }

  rollback() {
    native(() => addon.kvEnd(this.handle, false));
  }
}

class KeyValue {
  constructor(file, options = {}) {
    this.readOnly = Boolean(options.readOnly);
    this.handle = native(() => addon.kvOpen(String(file), this.readOnly));
  }

  begin(writing) {
    if (!this.handle) {
      throw new EggDbError({ message: 'the store is closed', code: null, status: 100 });
    }
    return new Transaction(native(() => addon.kvBegin(this.handle, writing)), writing);
  }

  read(body) {
    const transaction = this.begin(false);
    try {
      return body(transaction);
    } finally {
      transaction.rollback();
    }
  }

  write(body) {
    const transaction = this.begin(true);
    let value;
    try {
      value = body(transaction);
    } catch (error) {
      transaction.rollback();
      throw error;
    }
    transaction.commit();
    return value;
  }

  close() {
    if (this.handle) {
      addon.kvClose(this.handle);
      this.handle = null;
    }
  }
}

module.exports = { Database, EggDbError, KeyValue, Statement, Transaction };
