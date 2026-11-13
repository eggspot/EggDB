const SCHEMA = [
  "CREATE TABLE IF NOT EXISTS idb_databases (name TEXT PRIMARY KEY, version INTEGER NOT NULL)",
  "CREATE TABLE IF NOT EXISTS idb_stores (db TEXT NOT NULL, store TEXT NOT NULL, key_path TEXT, auto_increment INTEGER NOT NULL, next_key INTEGER NOT NULL, PRIMARY KEY (db, store))",
  "CREATE TABLE IF NOT EXISTS idb_indexes (db TEXT NOT NULL, store TEXT NOT NULL, name TEXT NOT NULL, key_path TEXT NOT NULL, is_unique INTEGER NOT NULL, multi_entry INTEGER NOT NULL, PRIMARY KEY (db, store, name))",
  "CREATE TABLE IF NOT EXISTS idb_records (db TEXT NOT NULL, store TEXT NOT NULL, key TEXT NOT NULL, key_value TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (db, store, key))",
  "CREATE TABLE IF NOT EXISTS idb_index_entries (db TEXT NOT NULL, store TEXT NOT NULL, idx TEXT NOT NULL, key TEXT NOT NULL, key_value TEXT NOT NULL, primary_key TEXT NOT NULL, PRIMARY KEY (db, store, idx, key, primary_key))",
].join("; ");

const MAX_GENERATED_KEY = 2 ** 53;

function domError(name, message) {
  const error = new Error(message);
  error.name = name;
  return error;
}

function literal(text) {
  return `'${String(text).replaceAll("'", "''")}'`;
}

function hex(value, digits) {
  return value.toString(16).padStart(digits, "0");
}

function numberCode(number) {
  const view = new DataView(new ArrayBuffer(8));
  view.setFloat64(0, number === 0 ? 0 : number);
  let high = view.getUint32(0);
  let low = view.getUint32(4);
  if (high & 0x80000000) {
    high = ~high >>> 0;
    low = ~low >>> 0;
  } else {
    high = (high | 0x80000000) >>> 0;
  }
  return hex(high, 8) + hex(low, 8);
}

function bytesOf(value) {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
}

function isBinary(value) {
  return value instanceof ArrayBuffer || ArrayBuffer.isView(value);
}

export function encodeKey(key, seen = new Set()) {
  if (typeof key === "number") {
    if (Number.isNaN(key)) throw domError("DataError", "NaN is not a valid key");
    return `1${numberCode(key)}`;
  }
  if (key instanceof Date) {
    const time = key.getTime();
    if (Number.isNaN(time)) throw domError("DataError", "an invalid Date is not a valid key");
    return `2${numberCode(time)}`;
  }
  if (typeof key === "string") {
    let out = "3";
    for (let at = 0; at < key.length; at++) out += hex(key.charCodeAt(at) + 1, 5);
    return `${out}00000`;
  }
  if (isBinary(key)) {
    let out = "4";
    for (const byte of bytesOf(key)) out += hex(byte + 1, 3);
    return `${out}000`;
  }
  if (Array.isArray(key)) {
    if (seen.has(key)) throw domError("DataError", "an array key cannot contain itself");
    seen.add(key);
    let out = "5";
    for (let at = 0; at < key.length; at++) {
      if (!(at in key)) throw domError("DataError", "an array key cannot have holes");
      out += encodeKey(key[at], seen);
    }
    seen.delete(key);
    return `${out}0`;
  }
  throw domError("DataError", `${typeof key} is not a valid key`);
}

export function cmp(first, second) {
  const a = encodeKey(first);
  const b = encodeKey(second);
  return a < b ? -1 : a > b ? 1 : 0;
}

function base64(bytes) {
  let text = "";
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text);
}

function unbase64(text) {
  const raw = atob(text);
  const bytes = new Uint8Array(raw.length);
  for (let at = 0; at < raw.length; at++) bytes[at] = raw.charCodeAt(at);
  return bytes;
}

const TYPED_ARRAYS = {
  Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array,
  Float32Array, Float64Array, BigInt64Array, BigUint64Array, DataView,
};

function cloneOut(value, seen) {
  if (value === null || typeof value === "boolean" || typeof value === "string") return value;
  if (typeof value === "undefined") return { $: "undefined" };
  if (typeof value === "number") {
    if (Number.isFinite(value) && !Object.is(value, -0)) return value;
    return { $: "number", v: String(Object.is(value, -0) ? "-0" : value) };
  }
  if (typeof value === "bigint") return { $: "bigint", v: value.toString() };
  if (typeof value !== "object") throw domError("DataCloneError", `a ${typeof value} cannot be stored`);
  if (seen.has(value)) throw domError("DataCloneError", "values that contain themselves are not supported");
  seen.add(value);
  let out;
  if (value instanceof Date) out = { $: "date", v: value.getTime() };
  else if (value instanceof RegExp) out = { $: "regexp", v: value.source, f: value.flags };
  else if (value instanceof ArrayBuffer) out = { $: "buffer", v: base64(new Uint8Array(value)) };
  else if (ArrayBuffer.isView(value)) out = { $: "view", t: value.constructor.name, v: base64(bytesOf(value)) };
  else if (value instanceof Map) out = { $: "map", v: [...value].map(([key, item]) => [cloneOut(key, seen), cloneOut(item, seen)]) };
  else if (value instanceof Set) out = { $: "set", v: [...value].map((item) => cloneOut(item, seen)) };
  else if (Array.isArray(value)) out = { $: "array", v: value.map((item) => cloneOut(item, seen)) };
  else if (typeof Blob !== "undefined" && value instanceof Blob) throw domError("DataCloneError", "Blob values are not supported yet");
  else {
    const fields = {};
    for (const [key, item] of Object.entries(value)) fields[key] = cloneOut(item, seen);
    out = { $: "object", v: fields };
  }
  seen.delete(value);
  return out;
}

function cloneIn(stored) {
  if (stored === null || typeof stored !== "object") return stored;
  switch (stored.$) {
    case "undefined": return undefined;
    case "number": return stored.v === "-0" ? -0 : Number(stored.v);
    case "bigint": return BigInt(stored.v);
    case "date": return new Date(stored.v);
    case "regexp": return new RegExp(stored.v, stored.f);
    case "buffer": return unbase64(stored.v).buffer;
    case "view": {
      const bytes = unbase64(stored.v);
      const Kind = TYPED_ARRAYS[stored.t] ?? Uint8Array;
      if (Kind === DataView) return new DataView(bytes.buffer);
      return new Kind(bytes.buffer, 0, bytes.byteLength / (Kind.BYTES_PER_ELEMENT ?? 1));
    }
    case "map": return new Map(stored.v.map(([key, item]) => [cloneIn(key), cloneIn(item)]));
    case "set": return new Set(stored.v.map(cloneIn));
    case "array": return stored.v.map(cloneIn);
    default: {
      const out = {};
      for (const [key, item] of Object.entries(stored.v ?? {})) out[key] = cloneIn(item);
      return out;
    }
  }
}

function serialize(value) {
  return JSON.stringify(cloneOut(value, new Set()));
}

function deserialize(text) {
  return cloneIn(JSON.parse(text));
}

function valueAtPath(value, path) {
  if (Array.isArray(path)) return path.map((part) => valueAtPath(value, part));
  if (path === "") return value;
  let current = value;
  for (const part of path.split(".")) {
    if (current === null || typeof current !== "object" || !(part in current)) return undefined;
    current = current[part];
  }
  return current;
}

function setAtPath(value, path, key) {
  const parts = path.split(".");
  let current = value;
  for (const part of parts.slice(0, -1)) {
    if (!(part in current)) current[part] = {};
    current = current[part];
  }
  current[parts[parts.length - 1]] = key;
}

function validKey(key) {
  try {
    encodeKey(key);
    return true;
  } catch {
    return false;
  }
}

export class IDBKeyRange {
  constructor(lower, upper, lowerOpen, upperOpen) {
    this.lower = lower;
    this.upper = upper;
    this.lowerOpen = lowerOpen;
    this.upperOpen = upperOpen;
  }

  static only(value) {
    encodeKey(value);
    return new IDBKeyRange(value, value, false, false);
  }

  static lowerBound(lower, open = false) {
    encodeKey(lower);
    return new IDBKeyRange(lower, undefined, open, true);
  }

  static upperBound(upper, open = false) {
    encodeKey(upper);
    return new IDBKeyRange(undefined, upper, true, open);
  }

  static bound(lower, upper, lowerOpen = false, upperOpen = false) {
    if (cmp(lower, upper) > 0 || (cmp(lower, upper) === 0 && (lowerOpen || upperOpen))) {
      throw domError("DataError", "the lower bound is above the upper bound");
    }
    return new IDBKeyRange(lower, upper, lowerOpen, upperOpen);
  }

  includes(key) {
    const code = encodeKey(key);
    if (this.lower !== undefined) {
      const lower = encodeKey(this.lower);
      if (code < lower || (this.lowerOpen && code === lower)) return false;
    }
    if (this.upper !== undefined) {
      const upper = encodeKey(this.upper);
      if (code > upper || (this.upperOpen && code === upper)) return false;
    }
    return true;
  }
}

function rangeOf(query) {
  if (query === undefined || query === null) return null;
  return query instanceof IDBKeyRange ? query : IDBKeyRange.only(query);
}

function rangeSql(column, query) {
  const range = rangeOf(query);
  if (!range) return "";
  let sql = "";
  if (range.lower !== undefined) sql += ` AND ${column} ${range.lowerOpen ? ">" : ">="} ${literal(encodeKey(range.lower))}`;
  if (range.upper !== undefined) sql += ` AND ${column} ${range.upperOpen ? "<" : "<="} ${literal(encodeKey(range.upper))}`;
  return sql;
}

class EventSource extends EventTarget {
  constructor(types) {
    super();
    for (const type of types) {
      this[`on${type}`] = null;
      this.addEventListener(type, (event) => this[`on${type}`]?.call(this, event));
    }
  }

  fire(type, fields = {}) {
    const event = new Event(type, { cancelable: type === "error" });
    Object.assign(event, fields);
    this.dispatchEvent(event);
    return event;
  }
}

export class IDBRequest extends EventSource {
  constructor(source, transaction) {
    super(["success", "error"]);
    this.source = source;
    this.transaction = transaction;
    this.readyState = "pending";
    this.result = undefined;
    this.error = null;
  }

  succeed(result) {
    this.readyState = "done";
    this.result = result;
    this.fire("success");
  }

  failWith(error) {
    this.readyState = "done";
    this.error = error;
    return this.fire("error");
  }
}

export class IDBOpenDBRequest extends IDBRequest {
  constructor() {
    super(null, null);
    for (const type of ["upgradeneeded", "blocked"]) {
      this[`on${type}`] = null;
      this.addEventListener(type, (event) => this[`on${type}`]?.call(this, event));
    }
  }
}

class Engine {
  constructor(db) {
    this.db = db;
    this.turn = Promise.resolve();
    this.ready = null;
  }

  async sql(text) {
    const out = await this.db.execute(text);
    if (out.error) throw domError("UnknownError", out.error);
    return out.results;
  }

  async rows(text) {
    const results = await this.sql(text);
    return results[results.length - 1]?.rows ?? [];
  }

  schema() {
    this.ready ??= this.sql(SCHEMA);
    return this.ready;
  }

  exclusive(work) {
    const run = this.turn.then(async () => {
      await this.schema();
      return work();
    });
    this.turn = run.catch(() => {});
    return run;
  }
}

export class IDBTransaction extends EventSource {
  constructor(database, names, mode) {
    super(["complete", "error", "abort"]);
    this.db = database;
    this.mode = mode;
    this.objectStoreNames = list(names);
    this.error = null;
    this.operations = [];
    this.pending = 0;
    this.state = "active";
    this.started = false;
    this.begun = false;
    this.turn = Promise.resolve();
    this.done = new Promise((resolve) => (this.finished = resolve));
    queueMicrotask(() => this.settleIfIdle());
  }

  get engine() {
    return this.db.engine;
  }

  objectStore(name) {
    if (!this.objectStoreNames.contains(name)) throw domError("NotFoundError", `the transaction does not include ${name}`);
    const meta = this.db.stores.get(name);
    if (!meta) throw domError("NotFoundError", `no object store named ${name}`);
    return new IDBObjectStore(this, name, meta);
  }

  request(source, work) {
    if (this.state !== "active") throw domError("TransactionInactiveError", "the transaction has finished");
    const request = new IDBRequest(source, this);
    this.pending++;
    this.operations.push({ request, work });
    this.pump();
    return request;
  }

  async pump() {
    if (this.running) return;
    this.running = true;
    if (!this.started) {
      this.started = true;
      await this.turn;
      if (this.state !== "active") {
        this.running = false;
        return;
      }
      if (!this.external) {
        await this.engine.sql("BEGIN");
        this.begun = true;
      }
    }
    while (this.operations.length && this.state === "active") {
      const { request, work } = this.operations.shift();
      try {
        const result = await work();
        this.pending--;
        request.succeed(result);
      } catch (error) {
        this.pending--;
        const event = request.failWith(error);
        if (!event.defaultPrevented) {
          this.error = error;
          await this.abortWith(error);
        }
      }
    }
    this.running = false;
    queueMicrotask(() => this.settleIfIdle());
  }

  async settleIfIdle() {
    if (this.state !== "active" || this.pending > 0 || this.running || this.operations.length) return;
    this.state = "committing";
    try {
      if (this.begun) await this.engine.sql("COMMIT");
      if (this.afterCommit) await this.afterCommit();
      this.state = "finished";
      this.fire("complete");
    } catch (error) {
      this.state = "active";
      await this.abortWith(error);
      return;
    }
    this.finished();
  }

  abort() {
    if (this.state !== "active") throw domError("InvalidStateError", "the transaction has finished");
    this.abortWith(domError("AbortError", "the transaction was aborted"));
  }

  async abortWith(error) {
    if (this.state === "finished") return;
    this.state = "finished";
    this.error ??= error;
    for (const { request } of this.operations) request.failWith(domError("AbortError", "the transaction was aborted"));
    this.operations = [];
    if (this.begun) await this.engine.sql("ROLLBACK").catch(() => {});
    if (this.afterAbort) await this.afterAbort();
    this.fire("abort");
    this.finished();
  }

  commit() {
    queueMicrotask(() => this.settleIfIdle());
  }
}

function list(names) {
  const sorted = [...names].sort();
  sorted.contains = (name) => sorted.includes(name);
  sorted.item = (at) => sorted[at] ?? null;
  return sorted;
}

class Reader {
  constructor(transaction, store, index) {
    this.transaction = transaction;
    this.storeName = store;
    this.indexName = index;
  }

  get engine() {
    return this.transaction.engine;
  }

  get dbName() {
    return this.transaction.db.name;
  }

  where() {
    return `db = ${literal(this.dbName)} AND store = ${literal(this.storeName)}`;
  }

  async records(query, count, direction = "next", columns = "r.key_value, r.value") {
    const descending = direction.startsWith("prev");
    const limit = count ? ` LIMIT ${Math.floor(count)}` : "";
    if (!this.indexName) {
      return this.engine.rows(`SELECT ${columns.replaceAll("r.", "")}, key FROM idb_records WHERE ${this.where()}${rangeSql("key", query)} ORDER BY key ${descending ? "DESC" : "ASC"}${limit}`);
    }
    return this.engine.rows(
      `SELECT ${columns}, e.key_value, e.key FROM idb_index_entries e JOIN idb_records r ON r.db = e.db AND r.store = e.store AND r.key = e.primary_key WHERE e.db = ${literal(this.dbName)} AND e.store = ${literal(this.storeName)} AND e.idx = ${literal(this.indexName)}${rangeSql("e.key", query)} ORDER BY e.key ${descending ? "DESC" : "ASC"}, e.primary_key ${descending ? "DESC" : "ASC"}${limit}`,
    );
  }

  get(query) {
    return this.transaction.request(this, async () => {
      const [row] = await this.records(query, 1);
      return row ? deserialize(row[1]) : undefined;
    });
  }

  getKey(query) {
    return this.transaction.request(this, async () => {
      const [row] = await this.records(query, 1);
      return row ? deserialize(row[0]) : undefined;
    });
  }

  getAll(query, count) {
    return this.transaction.request(this, async () => (await this.records(query, count)).map((row) => deserialize(row[1])));
  }

  getAllKeys(query, count) {
    return this.transaction.request(this, async () => (await this.records(query, count)).map((row) => deserialize(row[0])));
  }

  count(query) {
    return this.transaction.request(this, async () => (await this.records(query)).length);
  }

  openCursor(query, direction = "next") {
    return this.cursor(query, direction, true);
  }

  openKeyCursor(query, direction = "next") {
    return this.cursor(query, direction, false);
  }

  cursor(query, direction, withValue) {
    if (!["next", "nextunique", "prev", "prevunique"].includes(direction)) throw new TypeError(`unknown cursor direction ${direction}`);
    const cursor = new IDBCursor(this, rangeOf(query), direction, withValue);
    const request = this.transaction.request(this, () => cursor.step());
    cursor.request = request;
    return request;
  }
}

export class IDBObjectStore extends Reader {
  constructor(transaction, name, meta) {
    super(transaction, name, null);
    this.name = name;
    this.keyPath = meta.keyPath;
    this.autoIncrement = meta.autoIncrement;
    this.meta = meta;
    this.indexNames = list(meta.indexes.keys());
  }

  writable() {
    if (this.transaction.mode === "readonly") throw domError("ReadOnlyError", "the transaction is read-only");
  }

  async keyFor(value, key, overwrite) {
    if (this.keyPath !== null && key !== undefined) throw domError("DataError", "a store with a key path takes no separate key");
    let chosen = this.keyPath !== null ? valueAtPath(value, this.keyPath) : key;
    if (chosen === undefined) {
      if (!this.autoIncrement) throw domError("DataError", "the value has no key and the store generates none");
      const [[next]] = await this.engine.rows(`SELECT next_key FROM idb_stores WHERE ${this.where()}`);
      if (Number(next) > MAX_GENERATED_KEY) throw domError("ConstraintError", "the key generator ran out of keys");
      chosen = Number(next);
      if (this.keyPath !== null) setAtPath(value, this.keyPath, chosen);
    }
    if (!validKey(chosen)) throw domError("DataError", "the key is not a valid key");
    if (this.autoIncrement && typeof chosen === "number") {
      await this.engine.sql(`UPDATE idb_stores SET next_key = ${Math.max(Math.floor(chosen) + 1, 1)} WHERE ${this.where()} AND next_key <= ${Math.floor(chosen)}`);
    }
    if (!overwrite) {
      const [found] = await this.engine.rows(`SELECT 1 FROM idb_records WHERE ${this.where()} AND key = ${literal(encodeKey(chosen))}`);
      if (found) throw domError("ConstraintError", "a record with this key already exists");
    }
    return chosen;
  }

  async writeIndexes(key, value) {
    const primary = literal(encodeKey(key));
    await this.engine.sql(`DELETE FROM idb_index_entries WHERE ${this.where()} AND primary_key = ${primary}`);
    for (const [name, index] of this.meta.indexes) {
      const indexed = valueAtPath(value, index.keyPath);
      const keys = index.multiEntry && Array.isArray(indexed) ? [...new Map(indexed.filter(validKey).map((item) => [encodeKey(item), item])).values()] : validKey(indexed) ? [indexed] : [];
      for (const entry of keys) {
        const code = literal(encodeKey(entry));
        if (index.unique) {
          const [clash] = await this.engine.rows(`SELECT 1 FROM idb_index_entries WHERE ${this.where()} AND idx = ${literal(name)} AND key = ${code} AND primary_key <> ${primary}`);
          if (clash) throw domError("ConstraintError", `the unique index ${name} already has this key`);
        }
        await this.engine.sql(`INSERT INTO idb_index_entries (db, store, idx, key, key_value, primary_key) VALUES (${literal(this.dbName)}, ${literal(this.name)}, ${literal(name)}, ${code}, ${literal(serialize(entry))}, ${primary})`);
      }
    }
  }

  store(value, key, overwrite) {
    this.writable();
    const copy = cloneIn(cloneOut(value, new Set()));
    return this.transaction.request(this, async () => {
      const chosen = await this.keyFor(copy, key, overwrite);
      const code = literal(encodeKey(chosen));
      await this.engine.sql(`DELETE FROM idb_records WHERE ${this.where()} AND key = ${code}`);
      await this.engine.sql(`INSERT INTO idb_records (db, store, key, key_value, value) VALUES (${literal(this.dbName)}, ${literal(this.name)}, ${code}, ${literal(serialize(chosen))}, ${literal(serialize(copy))})`);
      await this.writeIndexes(chosen, copy);
      return chosen;
    });
  }

  put(value, key) {
    return this.store(value, key, true);
  }

  add(value, key) {
    return this.store(value, key, false);
  }

  delete(query) {
    this.writable();
    return this.transaction.request(this, async () => {
      const range = rangeSql("key", query);
      await this.engine.sql(`DELETE FROM idb_index_entries WHERE ${this.where()} AND primary_key IN (SELECT key FROM idb_records WHERE ${this.where()}${range})`);
      await this.engine.sql(`DELETE FROM idb_records WHERE ${this.where()}${range}`);
      return undefined;
    });
  }

  clear() {
    this.writable();
    return this.transaction.request(this, async () => {
      await this.engine.sql(`DELETE FROM idb_index_entries WHERE ${this.where()}; DELETE FROM idb_records WHERE ${this.where()}`);
      return undefined;
    });
  }

  index(name) {
    if (!this.meta.indexes.has(name)) throw domError("NotFoundError", `no index named ${name}`);
    return new IDBIndex(this, name, this.meta.indexes.get(name));
  }

  createIndex(name, keyPath, { unique = false, multiEntry = false } = {}) {
    if (this.transaction.mode !== "versionchange") throw domError("InvalidStateError", "indexes are created in an upgrade");
    if (this.meta.indexes.has(name)) throw domError("ConstraintError", `an index named ${name} exists`);
    const index = { keyPath, unique, multiEntry };
    this.meta.indexes.set(name, index);
    this.indexNames = list(this.meta.indexes.keys());
    this.transaction.request(this, async () => {
      await this.engine.sql(`INSERT INTO idb_indexes (db, store, name, key_path, is_unique, multi_entry) VALUES (${literal(this.dbName)}, ${literal(this.name)}, ${literal(name)}, ${literal(JSON.stringify(keyPath))}, ${unique ? 1 : 0}, ${multiEntry ? 1 : 0})`);
      for (const [keyText, valueText] of (await this.records(undefined)).map((row) => [row[0], row[1]])) {
        await this.writeIndexes(deserialize(keyText), deserialize(valueText));
      }
    });
    return new IDBIndex(this, name, index);
  }

  deleteIndex(name) {
    if (this.transaction.mode !== "versionchange") throw domError("InvalidStateError", "indexes are deleted in an upgrade");
    if (!this.meta.indexes.delete(name)) throw domError("NotFoundError", `no index named ${name}`);
    this.indexNames = list(this.meta.indexes.keys());
    this.transaction.request(this, () => this.engine.sql(`DELETE FROM idb_indexes WHERE ${this.where()} AND name = ${literal(name)}; DELETE FROM idb_index_entries WHERE ${this.where()} AND idx = ${literal(name)}`));
  }
}

export class IDBIndex extends Reader {
  constructor(objectStore, name, index) {
    super(objectStore.transaction, objectStore.name, name);
    this.objectStore = objectStore;
    this.name = name;
    this.keyPath = index.keyPath;
    this.unique = index.unique;
    this.multiEntry = index.multiEntry;
  }

  getKey(query) {
    return this.transaction.request(this, async () => {
      const [row] = await this.records(query, 1);
      return row ? deserialize(row[0]) : undefined;
    });
  }
}

export class IDBCursor {
  constructor(source, range, direction, withValue) {
    this.source = source;
    this.range = range;
    this.direction = direction;
    this.withValue = withValue;
    this.key = undefined;
    this.primaryKey = undefined;
    this.value = undefined;
    this.position = null;
    this.primaryPosition = null;
    this.target = null;
    this.skip = 0;
  }

  async step() {
    const descending = this.direction.startsWith("prev");
    const unique = this.direction.endsWith("unique");
    const indexed = this.source.indexName !== null;
    const keyColumn = indexed ? "e.key" : "key";
    let after = "";
    if (this.position !== null) {
      const at = literal(this.position);
      if (!indexed || unique) after = ` AND ${keyColumn} ${descending ? "<" : ">"} ${at}`;
      else {
        const primary = literal(this.primaryPosition);
        after = descending
          ? ` AND (e.key < ${at} OR (e.key = ${at} AND e.primary_key < ${primary}))`
          : ` AND (e.key > ${at} OR (e.key = ${at} AND e.primary_key > ${primary}))`;
      }
    }
    if (this.target !== null) after += ` AND ${keyColumn} ${descending ? "<=" : ">="} ${literal(this.target)}`;
    this.target = null;
    const reader = this.source;
    const where = rangeSql(keyColumn, this.range ?? undefined) + after;
    const order = descending ? "DESC" : "ASC";
    const offset = this.skip > 0 ? ` OFFSET ${this.skip}` : "";
    this.skip = 0;
    const rows = indexed
      ? await reader.engine.rows(`SELECT r.key_value, r.value, e.key_value, e.key, r.key FROM idb_index_entries e JOIN idb_records r ON r.db = e.db AND r.store = e.store AND r.key = e.primary_key WHERE e.db = ${literal(reader.dbName)} AND e.store = ${literal(reader.storeName)} AND e.idx = ${literal(reader.indexName)}${where} ORDER BY e.key ${order}, e.primary_key ${unique && descending ? "ASC" : order} LIMIT 1${offset}`)
      : await reader.engine.rows(`SELECT key_value, value, key_value, key, key FROM idb_records WHERE ${reader.where()}${where} ORDER BY key ${order} LIMIT 1${offset}`);
    if (!rows.length) return null;
    const [primaryValue, value, keyValue, keyCode, primaryCode] = rows[0];
    this.position = keyCode;
    this.primaryPosition = primaryCode;
    this.key = deserialize(keyValue);
    this.primaryKey = deserialize(primaryValue);
    this.value = this.withValue ? deserialize(value) : undefined;
    return this;
  }

  again() {
    const transaction = this.source.transaction;
    if (transaction.state !== "active") throw domError("TransactionInactiveError", "the transaction has finished");
    transaction.pending++;
    transaction.operations.push({ request: this.request, work: () => this.step() });
    this.request.readyState = "pending";
    transaction.pump();
  }

  continue(key) {
    if (key !== undefined) this.target = encodeKey(key);
    this.again();
  }

  advance(count) {
    if (!(count > 0)) throw new TypeError("advance needs a positive count");
    this.skip = count - 1;
    this.again();
  }

  update(value) {
    const store = this.source.objectStore ?? this.source;
    if (store.keyPath !== null) return store.put(value);
    return store.put(value, this.primaryKey);
  }

  delete() {
    const store = this.source.objectStore ?? this.source;
    return store.delete(this.primaryKey);
  }
}

export class IDBDatabase extends EventSource {
  constructor(factory, name, version, stores) {
    super(["close", "versionchange", "abort", "error"]);
    this.factory = factory;
    this.engine = factory.engine;
    this.name = name;
    this.version = version;
    this.stores = stores;
    this.closed = false;
  }

  get objectStoreNames() {
    return list(this.stores.keys());
  }

  createObjectStore(name, { keyPath = null, autoIncrement = false } = {}) {
    const upgrade = this.upgrade;
    if (!upgrade || upgrade.state !== "active") throw domError("InvalidStateError", "object stores are created in an upgrade");
    if (this.stores.has(name)) throw domError("ConstraintError", `an object store named ${name} exists`);
    if (autoIncrement && (keyPath === "" || Array.isArray(keyPath))) throw domError("InvalidAccessError", "a generated key needs a simple key path");
    const meta = { keyPath, autoIncrement, indexes: new Map() };
    this.stores.set(name, meta);
    upgrade.objectStoreNames = list(this.stores.keys());
    upgrade.request(null, () => this.engine.sql(`INSERT INTO idb_stores (db, store, key_path, auto_increment, next_key) VALUES (${literal(this.name)}, ${literal(name)}, ${keyPath === null ? "NULL" : literal(JSON.stringify(keyPath))}, ${autoIncrement ? 1 : 0}, 1)`));
    return new IDBObjectStore(upgrade, name, meta);
  }

  deleteObjectStore(name) {
    const upgrade = this.upgrade;
    if (!upgrade || upgrade.state !== "active") throw domError("InvalidStateError", "object stores are deleted in an upgrade");
    if (!this.stores.delete(name)) throw domError("NotFoundError", `no object store named ${name}`);
    upgrade.objectStoreNames = list(this.stores.keys());
    const where = `db = ${literal(this.name)} AND store = ${literal(name)}`;
    upgrade.request(null, () => this.engine.sql(`DELETE FROM idb_stores WHERE ${where}; DELETE FROM idb_indexes WHERE ${where}; DELETE FROM idb_records WHERE ${where}; DELETE FROM idb_index_entries WHERE ${where}`));
  }

  transaction(names, mode = "readonly") {
    if (this.closed) throw domError("InvalidStateError", "the database connection is closed");
    const wanted = typeof names === "string" ? [names] : [...names];
    if (!wanted.length) throw domError("InvalidAccessError", "a transaction needs at least one object store");
    for (const name of wanted) if (!this.stores.has(name)) throw domError("NotFoundError", `no object store named ${name}`);
    if (mode !== "readonly" && mode !== "readwrite") throw new TypeError(`unknown transaction mode ${mode}`);
    const transaction = new IDBTransaction(this, wanted, mode);
    transaction.turn = new Promise((resolve) => {
      this.engine.exclusive(() => {
        resolve();
        return transaction.done;
      });
    });
    return transaction;
  }

  close() {
    this.closed = true;
  }
}

async function loadStores(engine, name) {
  const stores = new Map();
  for (const [store, keyPath, autoIncrement] of await engine.rows(`SELECT store, key_path, auto_increment FROM idb_stores WHERE db = ${literal(name)}`)) {
    stores.set(store, { keyPath: keyPath === null ? null : JSON.parse(keyPath), autoIncrement: Number(autoIncrement) === 1, indexes: new Map() });
  }
  for (const [store, index, keyPath, unique, multiEntry] of await engine.rows(`SELECT store, name, key_path, is_unique, multi_entry FROM idb_indexes WHERE db = ${literal(name)}`)) {
    stores.get(store)?.indexes.set(index, { keyPath: JSON.parse(keyPath), unique: Number(unique) === 1, multiEntry: Number(multiEntry) === 1 });
  }
  return stores;
}

export class IDBFactory {
  constructor(db) {
    this.engine = new Engine(db);
  }

  open(name, version) {
    if (version !== undefined && !(Number.isInteger(version) && version >= 1)) throw new TypeError("the version must be a positive whole number");
    const request = new IDBOpenDBRequest();
    this.engine.exclusive(async () => {
      try {
        const [row] = await this.engine.rows(`SELECT version FROM idb_databases WHERE name = ${literal(name)}`);
        const oldVersion = row ? Number(row[0]) : 0;
        const newVersion = version ?? (oldVersion || 1);
        if (newVersion < oldVersion) throw domError("VersionError", `the database is at version ${oldVersion}, above ${newVersion}`);
        const stores = await loadStores(this.engine, name);
        const database = new IDBDatabase(this, name, oldVersion, stores);
        if (newVersion === oldVersion) {
          request.succeed(database);
          return;
        }
        await this.engine.sql("BEGIN");
        const upgrade = new IDBTransaction(database, stores.keys(), "versionchange");
        upgrade.external = true;
        upgrade.started = true;
        database.upgrade = upgrade;
        database.version = newVersion;
        request.transaction = upgrade;
        upgrade.afterCommit = async () => {
          await this.engine.sql(`DELETE FROM idb_databases WHERE name = ${literal(name)}; INSERT INTO idb_databases (name, version) VALUES (${literal(name)}, ${newVersion}); COMMIT`);
        };
        upgrade.afterAbort = async () => {
          await this.engine.sql("ROLLBACK").catch(() => {});
        };
        request.readyState = "done";
        request.result = database;
        request.fire("upgradeneeded", { oldVersion, newVersion });
        await upgrade.done;
        database.upgrade = null;
        request.transaction = null;
        if (upgrade.error) {
          database.version = oldVersion;
          request.failWith(domError("AbortError", "the upgrade was aborted"));
          return;
        }
        request.succeed(database);
      } catch (error) {
        request.failWith(error);
      }
    });
    return request;
  }

  deleteDatabase(name) {
    const request = new IDBOpenDBRequest();
    this.engine.exclusive(async () => {
      try {
        const [row] = await this.engine.rows(`SELECT version FROM idb_databases WHERE name = ${literal(name)}`);
        const where = `db = ${literal(name)}`;
        await this.engine.sql(`BEGIN; DELETE FROM idb_databases WHERE name = ${literal(name)}; DELETE FROM idb_stores WHERE ${where}; DELETE FROM idb_indexes WHERE ${where}; DELETE FROM idb_records WHERE ${where}; DELETE FROM idb_index_entries WHERE ${where}; COMMIT`);
        request.oldVersion = row ? Number(row[0]) : 0;
        request.succeed(undefined);
      } catch (error) {
        request.failWith(error);
      }
    });
    return request;
  }

  async databases() {
    return this.engine.exclusive(async () => (await this.engine.rows("SELECT name, version FROM idb_databases ORDER BY name")).map(([name, version]) => ({ name, version: Number(version) })));
  }

  cmp(first, second) {
    return cmp(first, second);
  }
}

export function indexedDBOver(db) {
  return new IDBFactory(db);
}
