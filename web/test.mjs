import { readFile } from "node:fs/promises";
import { EggDB } from "./eggdb.js";

const wasm = await readFile(process.argv[2]);
const db = await EggDB.open({ wasm, path: "/data.eggdb", dialect: "sqlite", persist: false });
let out = await db.execute("CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT); INSERT INTO eggs VALUES (1, 'brown'), (2, NULL); SELECT colour FROM eggs ORDER BY id");
if (out.error || JSON.stringify(out.results[2].rows) !== '[["brown"],[null]]') throw new Error(JSON.stringify(out));
db.close();
const again = await EggDB.open({ wasm, path: "/data.eggdb", dialect: "postgresql", persist: false });
out = await again.execute("SELECT count(*) AS n FROM eggs");
if (out.error || out.results[0].rows[0][0] !== "2") throw new Error(JSON.stringify(out));
out = await again.execute("SELECT * FROM nothing_here");
if (!out.error) throw new Error("an error was expected");
await again.reset();
out = await again.execute("SELECT count(*) FROM eggs");
if (!out.error) throw new Error("reset should have removed the table");
console.log(`eggdb ${again.version} in WebAssembly under Node: passed`);
