'use strict';

const ClientSqlite3 = require('knex/lib/dialects/sqlite3');

function changedRows(tag) {
  const words = String(tag || '').split(' ');
  if (['INSERT', 'UPDATE', 'DELETE'].includes(words[0])) {
    const count = Number(words[words.length - 1]);
    return Number.isFinite(count) ? count : 0;
  }
  return 0;
}

class ClientEggDb extends ClientSqlite3 {
  _driver() {
    return require('./index.js');
  }

  async acquireRawConnection() {
    const filename = (this.connectionSettings && this.connectionSettings.filename) || ':memory:';
    return new this.driver.Database(filename, { dialect: 'sqlite' });
  }

  async destroyRawConnection(connection) {
    connection.close();
  }

  async _query(connection, query) {
    if (!query.sql) {
      throw new Error('The query is empty');
    }
    const results = connection.execute(query.sql, query.bindings || []);
    let rows = [];
    for (let index = results.length - 1; index >= 0; index -= 1) {
      const { columns, rows: cells } = results[index];
      if (columns.length > 0) {
        rows = cells.map((row) => Object.fromEntries(columns.map((column, at) => [column, row[at]])));
        break;
      }
    }
    let lastID;
    if (query.method === 'insert') {
      const [answer] = connection.execute('SELECT last_insert_rowid()');
      lastID = answer && answer.rows[0] ? answer.rows[0][0] : undefined;
    }
    query.response = rows;
    query.context = {
      lastID,
      changes: results.reduce((total, result) => total + changedRows(result.tag), 0),
    };
    return query;
  }

  _stream() {
    throw new Error('streaming queries are not supported by the EggDB Knex client yet');
  }
}

Object.assign(ClientEggDb.prototype, { dialect: 'sqlite3', driverName: 'eggdb' });

module.exports = ClientEggDb;
