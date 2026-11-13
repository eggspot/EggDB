const DIALECTS = ["postgresql", "sqlite", "mysql", "sqlserver", "oracle"];
const NO_IMAGE = 0xffffffff;
const started = performance.now();

function hostImports() {
  return {
    eggdb_host: {
      monotonic_nanos: () => BigInt(Math.round((performance.now() - started) * 1e6)),
      wall_clock_nanos: () => BigInt(Date.now()) * 1000000n,
    },
  };
}

export class BareEggDB {
  static async open({ wasm = "eggdb-bare.wasm", dialect = "postgresql", image = null } = {}) {
    const module = wasm instanceof WebAssembly.Module
      ? wasm
      : wasm instanceof ArrayBuffer || ArrayBuffer.isView(wasm)
        ? await WebAssembly.compile(wasm)
        : await WebAssembly.compile(await (await fetch(wasm)).arrayBuffer());
    const instance = await WebAssembly.instantiate(module, hostImports());
    const db = new BareEggDB(instance.exports);
    const code = DIALECTS.indexOf(dialect);
    if (code < 0) throw new Error(`unknown dialect ${dialect}`);
    db.handle = image
      ? db.send(image, () => db.exports.eggdb_wasm_open_image(code))
      : db.send(new TextEncoder().encode(":memory:"), () => db.exports.eggdb_wasm_open(code));
    if (db.handle === 0) throw new Error(db.answer().error ?? "EggDB could not open the database");
    db.version = db.call("eggdb_wasm_version").version ?? "";
    return db;
  }

  constructor(exports) {
    this.exports = exports;
    this.handle = 0;
    this.version = "";
  }

  send(bytes, run) {
    const start = this.exports.eggdb_wasm_input(bytes.length);
    new Uint8Array(this.exports.memory.buffer, start, bytes.length).set(bytes);
    return run();
  }

  output(length) {
    return new Uint8Array(this.exports.memory.buffer, this.exports.eggdb_wasm_output(), length);
  }

  answer() {
    return JSON.parse(new TextDecoder().decode(this.output(this.exports.eggdb_wasm_output_len())));
  }

  call(name) {
    this.exports[name]();
    return this.answer();
  }

  execute(sql) {
    this.send(new TextEncoder().encode(sql), () => this.exports.eggdb_wasm_execute(this.handle));
    return this.answer();
  }

  image() {
    const length = this.exports.eggdb_wasm_image(this.handle);
    if (length === NO_IMAGE) throw new Error(this.answer().error ?? "EggDB could not take an image");
    return this.output(length).slice();
  }

  close() {
    this.exports.eggdb_wasm_close(this.handle);
  }
}
