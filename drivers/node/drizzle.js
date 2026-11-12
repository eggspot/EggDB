'use strict';

function lastRows(results) {
  for (let index = results.length - 1; index >= 0; index -= 1) {
    if (results[index].columns.length > 0) {
      return results[index].rows;
    }
  }
  return [];
}

function proxyCallback(database) {
  return async (sql, params, method) => {
    const rows = lastRows(database.execute(sql, params));
    if (method === 'run') {
      return { rows: [] };
    }
    if (method === 'get') {
      return { rows: rows[0] || [] };
    }
    return { rows };
  };
}

function drizzle(database, config) {
  const proxy = require('drizzle-orm/sqlite-proxy');
  const callback = proxyCallback(database);
  const batch = async (queries) => {
    const answers = [];
    database.transaction(() => {
      for (const query of queries) {
        const rows = lastRows(database.execute(query.sql, query.params));
        answers.push({ rows: query.method === 'get' ? rows[0] || [] : rows });
      }
    });
    return answers;
  };
  return proxy.drizzle(callback, batch, config);
}

module.exports = { drizzle, proxyCallback };
