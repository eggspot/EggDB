'use strict';

const { Database } = require('./index.js');

function columnTypes() {
  const { ColumnTypeEnum } = require('@prisma/driver-adapter-utils');
  return ColumnTypeEnum;
}

function changedRows(results) {
  return results.reduce((total, result) => {
    const words = String(result.tag || '').split(' ');
    const count = Number(words[words.length - 1]);
    return ['INSERT', 'UPDATE', 'DELETE'].includes(words[0]) && Number.isFinite(count) ? total + count : total;
  }, 0);
}

function columnTypeOf(rows, column, types) {
  for (const row of rows) {
    const value = row[column];
    if (value === null || value === undefined) {
      continue;
    }
    if (typeof value === 'bigint') {
      return types.Int64;
    }
    if (typeof value === 'number') {
      return Number.isInteger(value) ? types.Int64 : types.Double;
    }
    if (value instanceof Uint8Array) {
      return types.Bytes;
    }
    return types.Text;
  }
  return types.Int32;
}

function argument(value) {
  if (typeof value === 'boolean') {
    return value ? 1 : 0;
  }
  if (value instanceof Date) {
    return value.toISOString();
  }
  return value;
}

class Queryable {
  constructor(database) {
    this.database = database;
    this.provider = 'sqlite';
    this.adapterName = 'eggdb';
  }

  run(query) {
    return this.database.execute(query.sql, (query.args || []).map(argument));
  }

  async queryRaw(query) {
    const results = this.run(query);
    for (let index = results.length - 1; index >= 0; index -= 1) {
      const { columns, rows } = results[index];
      if (columns.length > 0) {
        const types = columnTypes();
        return {
          columnNames: columns,
          columnTypes: columns.map((_, at) => columnTypeOf(rows, at, types)),
          rows: rows.map((row) => row.map((value) => (typeof value === 'bigint' ? value.toString() : value))),
        };
      }
    }
    return { columnNames: [], columnTypes: [], rows: [] };
  }

  async executeRaw(query) {
    return changedRows(this.run(query));
  }
}

class Transaction extends Queryable {
  constructor(database, release) {
    super(database);
    this.options = { usePhantomQuery: false };
    this.release = release;
  }

  async commit() {
    try {
      this.database.execute('COMMIT');
    } finally {
      this.release();
    }
  }

  async rollback() {
    try {
      this.database.execute('ROLLBACK');
    } finally {
      this.release();
    }
  }
}

class PrismaEggDbAdapter extends Queryable {
  constructor(database) {
    super(database);
    this.inTransaction = false;
  }

  async executeScript(script) {
    this.database.execute(script);
  }

  async startTransaction(isolationLevel) {
    if (isolationLevel && isolationLevel !== 'SERIALIZABLE') {
      throw new Error(`EggDB runs every transaction SERIALIZABLE, not ${isolationLevel}`);
    }
    if (this.inTransaction) {
      throw new Error('EggDB has one transaction open on this connection already');
    }
    this.database.execute('BEGIN');
    this.inTransaction = true;
    return new Transaction(this.database, () => {
      this.inTransaction = false;
    });
  }

  getConnectionInfo() {
    return { maxBindValues: 32766, supportsRelationJoins: false };
  }

  async dispose() {
    this.database.close();
  }
}

class PrismaEggDb {
  constructor(options = {}) {
    this.provider = 'sqlite';
    this.adapterName = 'eggdb';
    this.url = String(options.url || ':memory:').replace(/^file:/, '');
    this.readOnly = Boolean(options.readOnly);
  }

  async connect() {
    return new PrismaEggDbAdapter(new Database(this.url, { dialect: 'sqlite', readOnly: this.readOnly }));
  }
}

module.exports = { PrismaEggDb, PrismaEggDbAdapter };
