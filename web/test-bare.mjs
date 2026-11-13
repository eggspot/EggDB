import { readFile } from "node:fs/promises";
import { BareEggDB } from "./eggdb-bare.js";

const wasm = await readFile(process.argv[2]);
const db = await BareEggDB.open({ wasm, dialect: "sqlite" });
let out = db.execute("CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT); INSERT INTO eggs VALUES (1, 'blue'); SELECT colour FROM eggs");
if (out.error || JSON.stringify(out.results[2].rows) !== '[["blue"]]') throw new Error(JSON.stringify(out));
out = db.execute("SELECT current_timestamp");
if (out.error) throw new Error(JSON.stringify(out));
const image = db.image();
db.close();
const again = await BareEggDB.open({ wasm, dialect: "postgresql", image });
out = again.execute("SELECT count(*) FROM eggs");
if (out.error || out.results[0].rows[0][0] !== "1") throw new Error(JSON.stringify(out));
console.log(`eggdb ${again.version} on wasm32-unknown-unknown under Node, ${image.length} bytes of image: passed`);
