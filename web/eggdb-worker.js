import { EggDB } from "./eggdb.js";

const workerId = crypto.randomUUID();
const LOST_LEADER = "the tab that held the database closed before answering; the statement may or may not have run, so check before running it again";

let database = null;
let channel = null;
let opening = null;
let queue = Promise.resolve();
const forwarded = new Map();
const sessions = new Map();

function reply(id, answer) {
  postMessage({ id, answer });
}

function fail(id, failure) {
  postMessage({ id, failure });
}

function describe() {
  return { version: database.version, durable: database.durable };
}

function sessionFor(from, dialect) {
  if (from === workerId && dialect === database.dialect) return database.handle;
  const key = `${from}:${dialect}`;
  if (!sessions.has(key)) sessions.set(key, database.openSession(dialect));
  return sessions.get(key);
}

function run(type, sql, from, dialect) {
  const work = queue.then(async () => {
    if (type === "execute") return database.executeIn(sessionFor(from, dialect), sql);
    if (type === "reset") {
      sessions.clear();
      await database.reset();
      return {};
    }
    return describe();
  });
  queue = work.catch(() => {});
  return work;
}

function failForwarded() {
  for (const [id] of forwarded) fail(id, LOST_LEADER);
  forwarded.clear();
}

async function lead() {
  failForwarded();
  database = await EggDB.openInWorker(opening);
  postMessage({ type: "role", leader: true });
  channel.postMessage({ kind: "leader", from: workerId });
  if (opening.id !== undefined) {
    reply(opening.id, describe());
    opening.id = undefined;
  }
  await new Promise(() => {});
}

function follow() {
  postMessage({ type: "role", leader: false });
  forward({ id: opening.id, type: "describe" });
  opening.id = undefined;
}

function forward(message) {
  forwarded.set(message.id, message);
  channel.postMessage({ kind: "request", from: workerId, id: message.id, type: message.type, sql: message.sql, dialect: opening.dialect });
}

function listen() {
  channel.onmessage = async ({ data }) => {
    if (data.kind === "leader" && data.from !== workerId && !database) {
      const describing = [...forwarded.values()].filter((message) => message.type === "describe");
      for (const message of describing) forwarded.delete(message.id);
      failForwarded();
      for (const message of describing) forward(message);
      return;
    }
    if (data.kind === "request" && database) {
      try {
        const answer = await run(data.type, data.sql, data.from, data.dialect);
        channel.postMessage({ kind: "reply", to: data.from, id: data.id, answer });
      } catch (error) {
        channel.postMessage({ kind: "reply", to: data.from, id: data.id, failure: String(error?.message ?? error) });
      }
      return;
    }
    if (data.kind === "reply" && data.to === workerId && forwarded.has(data.id)) {
      forwarded.delete(data.id);
      if (data.failure) fail(data.id, data.failure);
      else reply(data.id, data.answer);
    }
  };
}

async function open(message) {
  opening = { wasm: message.wasm, path: message.path, dialect: message.dialect, id: message.id };
  channel = new BroadcastChannel(`eggdb:${message.path}`);
  listen();
  const name = `eggdb:${message.path}`;
  if (!navigator.locks) {
    await lead();
    return;
  }
  await navigator.locks.request(name, { ifAvailable: true }, async (lock) => {
    if (lock) {
      await lead();
      return;
    }
    follow();
    navigator.locks.request(name, lead);
  });
}

onmessage = async ({ data }) => {
  try {
    if (data.type === "open") {
      if (opening) return fail(data.id, "this worker already opened a database");
      await open(data);
      return;
    }
    if (!opening) return fail(data.id, "open a database first");
    if (database) reply(data.id, await run(data.type, data.sql, workerId, opening.dialect));
    else forward(data);
  } catch (error) {
    fail(data.id, String(error?.message ?? error));
  }
};
