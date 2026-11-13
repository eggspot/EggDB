import { readFile } from "node:fs/promises";
import { EggDB } from "./eggdb.js";
import { IDBKeyRange, indexedDBOver } from "./eggdb-indexeddb.js";

function check(condition, what) {
  if (!condition) throw new Error(what);
}

function done(request) {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = (event) => {
      event.preventDefault();
      reject(request.error);
    };
  });
}

function finished(transaction) {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onabort = () => reject(transaction.error);
  });
}

const wasm = await readFile(process.argv[2]);
const engine = await EggDB.open({ wasm, path: "/indexeddb.eggdb", dialect: "sqlite", persist: false });
const indexedDB = indexedDBOver(engine);

check(indexedDB.cmp(1, "a") < 0 && indexedDB.cmp("a", [1]) < 0 && indexedDB.cmp(-5, 2) < 0 && indexedDB.cmp("ab", "a") > 0, "key order");
check(indexedDB.cmp(new Date(5), 9e9) > 0 && indexedDB.cmp(new Uint8Array([1]), "z") > 0, "dates after numbers, binary after strings");

const opening = indexedDB.open("shop", 1);
let upgraded = false;
opening.onupgradeneeded = (event) => {
  upgraded = event.oldVersion === 0 && event.newVersion === 1;
  const db = opening.result;
  const orders = db.createObjectStore("orders", { keyPath: "id", autoIncrement: true });
  orders.createIndex("by_customer", "customer");
  orders.createIndex("by_tag", "tags", { multiEntry: true });
  db.createObjectStore("settings");
};
const db = await done(opening);
check(upgraded && db.version === 1 && db.objectStoreNames.contains("orders"), "the upgrade ran");

let tx = db.transaction(["orders", "settings"], "readwrite");
let ending = finished(tx);
const orders = tx.objectStore("orders");
const first = await done(orders.add({ customer: "ada", total: 12.5, tags: ["new", "gift"], placed: new Date(0) }));
const second = await done(orders.add({ customer: "bob", total: 3, tags: ["gift"] }));
await done(orders.put({ id: 10, customer: "ada", total: 1, tags: [] }));
const third = await done(orders.add({ customer: "cy", total: 7, tags: [] }));
await done(tx.objectStore("settings").put({ theme: "dark" }, "ui"));
await ending;
check(first === 1 && second === 2 && third === 11, `generated keys ${first} ${second} ${third}`);

tx = db.transaction("orders");
ending = finished(tx);
const store = tx.objectStore("orders");
const one = await done(store.get(1));
check(one.customer === "ada" && one.placed instanceof Date && one.tags.length === 2, "values round-trip");
const adas = await done(store.index("by_customer").getAll("ada"));
check(adas.length === 2, `index lookup found ${adas.length}`);
const gifts = await done(store.index("by_tag").getAllKeys("gift"));
check(JSON.stringify(gifts) === "[1,2]", `multi-entry index ${JSON.stringify(gifts)}`);
const middle = await done(store.getAllKeys(IDBKeyRange.bound(2, 10)));
check(JSON.stringify(middle) === "[2,10]", `range ${JSON.stringify(middle)}`);
check((await done(store.count())) === 4, "count");
const seen = [];
await new Promise((resolve, reject) => {
  const cursor = store.openCursor(null, "prev");
  cursor.onsuccess = () => {
    if (!cursor.result) return resolve();
    seen.push(cursor.result.key);
    cursor.result.continue();
  };
  cursor.onerror = () => reject(cursor.error);
});
check(JSON.stringify(seen) === "[11,10,2,1]", `cursor ${JSON.stringify(seen)}`);
await ending;

tx = db.transaction("orders", "readwrite");
ending = finished(tx);
const duplicate = tx.objectStore("orders").add({ id: 1, customer: "x", tags: [] });
const failure = await new Promise((resolve) => {
  duplicate.onsuccess = () => resolve(null);
  duplicate.onerror = () => resolve(duplicate.error);
});
check(failure?.name === "ConstraintError", `a duplicate add fails: ${failure?.name}`);
await ending.then(() => check(false, "the transaction must abort"), () => {});

tx = db.transaction("orders", "readwrite");
ending = finished(tx);
tx.objectStore("orders").delete(2);
tx.abort();
await ending.catch(() => {});
tx = db.transaction("orders");
ending = finished(tx);
check((await done(tx.objectStore("orders").get(2))) !== undefined, "an aborted delete is rolled back");
await ending;
db.close();

const reopening = indexedDB.open("shop", 2);
reopening.onupgradeneeded = (event) => {
  check(event.oldVersion === 1, "the second upgrade starts at 1");
  reopening.result.deleteObjectStore("settings");
};
const again = await done(reopening);
check(again.version === 2 && !again.objectStoreNames.contains("settings"), "the second upgrade ran");
const listed = await indexedDB.databases();
check(listed.length === 1 && listed[0].version === 2, JSON.stringify(listed));
await done(indexedDB.deleteDatabase("shop"));
check((await indexedDB.databases()).length === 0, "the database was deleted");
console.log("eggdb IndexedDB-compatible API under Node: passed");
