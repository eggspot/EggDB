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

  close() {
    if (this.handle) {
      addon.close(this.handle);
      this.handle = null;
    }
  }
}

module.exports = { Database, EggDbError };
