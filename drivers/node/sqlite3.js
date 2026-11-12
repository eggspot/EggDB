'use strict';

const { EventEmitter } = require('node:events');
const { Database: EggDatabase } = require('./index.js');

const OPEN_READONLY = 0x00000001;
const OPEN_READWRITE = 0x00000002;
const OPEN_CREATE = 0x00000004;
const OPEN_FULLMUTEX = 0x00010000;
const OPEN_URI = 0x00000040;
const OPEN_SHAREDCACHE = 0x00020000;
const OPEN_PRIVATECACHE = 0x00040000;

function changedRows(tag) {
  const words = String(tag || '').split(' ');
  if (['INSERT', 'UPDATE', 'DELETE'].includes(words[0])) {
    const count = Number(words[words.length - 1]);
    return Number.isFinite(count) ? count : 0;
  }
  return 0;
}

function placeholderName(sql, at) {
  let end = at + 1;
  while (end < sql.length && /[A-Za-z0-9_]/.test(sql[end])) {
    end += 1;
  }
  return sql.slice(at, end);
}

function positional(sql, parameters) {
  if (parameters === undefined || parameters === null) {
    return { sql, values: [] };
  }
  const named = !Array.isArray(parameters) && typeof parameters === 'object';
  const list = Array.isArray(parameters) ? parameters : [];
  let out = '';
  const values = [];
  let quote = null;
  let next = 0;
  for (let at = 0; at < sql.length; at += 1) {
    const character = sql[at];
    if (quote) {
      if (character === quote) {
        quote = null;
      }
      out += character;
      continue;
    }
    if (character === "'" || character === '"' || character === '`') {
      quote = character;
      out += character;
      continue;
    }
    if (character === '?') {
      const number = placeholderName(sql, at).slice(1);
      at += number.length;
      values.push(number ? list[Number(number) - 1] : list[next]);
      next += 1;
      out += '?';
      continue;
    }
    if ((character === '$' || character === ':' || character === '@') && /[A-Za-z0-9_]/.test(sql[at + 1] || '')) {
      const name = placeholderName(sql, at);
      at += name.length - 1;
      const key = name.slice(1);
      if (named) {
        values.push(name in parameters ? parameters[name] : parameters[key]);
      } else if (/^\d+$/.test(key)) {
        values.push(list[Number(key) - 1]);
      } else {
        values.push(list[next]);
      }
      next += 1;
      out += '?';
      continue;
    }
    out += character;
  }
  return { sql: out, values };
}

function argumentsOf(parameters, callback) {
  if (typeof parameters === 'function') {
    return { parameters: [], callback: parameters };
  }
  return { parameters, callback };
}

class Statement {
  constructor(database, sql) {
    this.database = database;
    this.sql = sql;
  }

  run(parameters, callback) {
    this.database.run(this.sql, parameters, callback);
    return this;
  }

  all(parameters, callback) {
    this.database.all(this.sql, parameters, callback);
    return this;
  }

  get(parameters, callback) {
    this.database.get(this.sql, parameters, callback);
    return this;
  }

  finalize(callback) {
    if (callback) {
      setImmediate(callback);
    }
    return this;
  }
}

class Database extends EventEmitter {
  constructor(filename, mode, callback) {
    super();
    const opened = typeof mode === 'function' ? mode : callback;
    const flags = typeof mode === 'number' ? mode : OPEN_READWRITE | OPEN_CREATE;
    this.filename = filename;
    try {
      this.database = new EggDatabase(filename, { dialect: 'sqlite', readOnly: (flags & OPEN_READWRITE) === 0 });
      setImmediate(() => {
        this.emit('open');
        if (opened) {
          opened.call(this, null);
        }
      });
    } catch (error) {
      setImmediate(() => {
        this.emit('error', error);
        if (opened) {
          opened.call(this, error);
        }
      });
    }
  }

  answer(callback, error, context, value) {
    setImmediate(() => {
      if (callback) {
        callback.call(context, error, value);
      } else if (error) {
        this.emit('error', error);
      }
    });
  }

  execute(sql, parameters) {
    const statement = positional(sql, parameters);
    return this.database.execute(statement.sql, statement.values);
  }

  rowsOf(results) {
    for (let index = results.length - 1; index >= 0; index -= 1) {
      const { columns, rows } = results[index];
      if (columns.length > 0) {
        return rows.map((cells) => Object.fromEntries(columns.map((column, at) => [column, cells[at]])));
      }
    }
    return [];
  }

  run(sql, parameters, callback) {
    const given = argumentsOf(parameters, callback);
    const context = { sql, lastID: 0, changes: 0 };
    try {
      const results = this.execute(sql, given.parameters);
      context.changes = results.reduce((total, result) => total + changedRows(result.tag), 0);
      if (/^\s*(insert|replace)\b/i.test(sql)) {
        const [answer] = this.database.execute('SELECT last_insert_rowid()');
        context.lastID = answer && answer.rows[0] ? Number(answer.rows[0][0]) : 0;
      }
      this.answer(given.callback, null, context);
    } catch (error) {
      this.answer(given.callback, error, context);
    }
    return this;
  }

  all(sql, parameters, callback) {
    const given = argumentsOf(parameters, callback);
    try {
      this.answer(given.callback, null, this, this.rowsOf(this.execute(sql, given.parameters)));
    } catch (error) {
      this.answer(given.callback, error, this);
    }
    return this;
  }

  get(sql, parameters, callback) {
    const given = argumentsOf(parameters, callback);
    try {
      this.answer(given.callback, null, this, this.rowsOf(this.execute(sql, given.parameters))[0]);
    } catch (error) {
      this.answer(given.callback, error, this);
    }
    return this;
  }

  each(sql, parameters, callback, complete) {
    const given = argumentsOf(parameters, callback);
    const done = typeof parameters === 'function' ? callback : complete;
    try {
      const rows = this.rowsOf(this.execute(sql, given.parameters));
      setImmediate(() => {
        for (const row of rows) {
          given.callback.call(this, null, row);
        }
        if (done) {
          done.call(this, null, rows.length);
        }
      });
    } catch (error) {
      this.answer(done || given.callback, error, this);
    }
    return this;
  }

  exec(sql, callback) {
    try {
      this.database.execute(sql);
      this.answer(callback, null, this);
    } catch (error) {
      this.answer(callback, error, this);
    }
    return this;
  }

  prepare(sql, parameters, callback) {
    const statement = new Statement(this, sql);
    const given = argumentsOf(parameters, callback);
    if (given.callback) {
      setImmediate(() => given.callback.call(statement, null));
    }
    return statement;
  }

  serialize(work) {
    if (work) {
      work();
    }
    return this;
  }

  parallelize(work) {
    return this.serialize(work);
  }

  configure() {
    return this;
  }

  loadExtension(path, callback) {
    this.answer(callback, new Error('EggDB does not load SQLite extensions'), this);
    return this;
  }

  close(callback) {
    try {
      this.database.close();
      setImmediate(() => {
        this.emit('close');
        if (callback) {
          callback.call(this, null);
        }
      });
    } catch (error) {
      this.answer(callback, error, this);
    }
  }
}

function verbose() {
  return module.exports;
}

module.exports = {
  Database,
  Statement,
  OPEN_READONLY,
  OPEN_READWRITE,
  OPEN_CREATE,
  OPEN_FULLMUTEX,
  OPEN_URI,
  OPEN_SHAREDCACHE,
  OPEN_PRIVATECACHE,
  verbose,
  positional,
};
