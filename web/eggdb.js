const SUCCESS = 0;
const EBADF = 8;
const EEXIST = 20;
const EINVAL = 28;
const EISDIR = 31;
const ENOENT = 44;
const ENOSYS = 52;
const ENOTDIR = 54;
const ENOTEMPTY = 55;
const FILETYPE_CHARACTER_DEVICE = 2;
const FILETYPE_DIRECTORY = 3;
const FILETYPE_REGULAR_FILE = 4;
const OFLAG_CREAT = 1;
const OFLAG_DIRECTORY = 2;
const OFLAG_EXCL = 4;
const OFLAG_TRUNC = 8;
const FDFLAG_APPEND = 1;
const ALL_RIGHTS = 0xffffffffffffffffn;
const DIALECTS = ["postgresql", "sqlite", "mysql", "sqlserver", "oracle"];
const OPFS_PREFIX = "eggdb:";

const files = new Map();
const directories = new Set(["/"]);
const dirty = new Set();
const removed = new Set();

function normalize(base, relative) {
  const parts = (relative.startsWith("/") ? relative : `${base}/${relative}`).split("/");
  const out = [];
  for (const part of parts) {
    if (part === "" || part === ".") continue;
    if (part === "..") out.pop();
    else out.push(part);
  }
  return `/${out.join("/")}`;
}

function parent(path) {
  const at = path.lastIndexOf("/");
  return at <= 0 ? "/" : path.slice(0, at);
}

function grow(file, size) {
  if (size <= file.data.length) return;
  const bigger = new Uint8Array(Math.max(size, file.data.length * 2, 4096));
  bigger.set(file.data.subarray(0, file.size));
  file.data = bigger;
}

function touch(path) {
  dirty.add(path);
  removed.delete(path);
}

class Wasi {
  constructor() {
    this.memory = null;
    this.fds = new Map([
      [0, { kind: "stdio" }],
      [1, { kind: "stdio" }],
      [2, { kind: "stdio" }],
      [3, { kind: "dir", path: "/", preopen: "/" }],
    ]);
    this.nextFd = 4;
    this.stderr = "";
    this.imports = { wasi_snapshot_preview1: this.functions() };
  }

  view() {
    return new DataView(this.memory.buffer);
  }

  bytes(pointer, length) {
    return new Uint8Array(this.memory.buffer, pointer, length);
  }

  text(pointer, length) {
    return new TextDecoder().decode(this.bytes(pointer, length));
  }

  vectors(pointer, count) {
    const view = this.view();
    const out = [];
    for (let at = 0; at < count; at++) {
      out.push(this.bytes(view.getUint32(pointer + at * 8, true), view.getUint32(pointer + at * 8 + 4, true)));
    }
    return out;
  }

  stat(pointer, type, size) {
    const view = this.view();
    for (let at = 0; at < 64; at += 8) view.setBigUint64(pointer + at, 0n, true);
    view.setUint8(pointer + 16, type);
    view.setBigUint64(pointer + 24, 1n, true);
    view.setBigUint64(pointer + 32, BigInt(size), true);
  }

  resolve(fd, pointer, length) {
    const entry = this.fds.get(fd);
    if (!entry || entry.kind !== "dir") return null;
    return normalize(entry.path, this.text(pointer, length));
  }

  readAt(entry, buffers, offset) {
    const file = files.get(entry.path);
    if (!file) return [EBADF, 0];
    let total = 0;
    for (const buffer of buffers) {
      const available = Math.max(0, Math.min(buffer.length, file.size - offset));
      buffer.set(file.data.subarray(offset, offset + available));
      total += available;
      offset += available;
      if (available < buffer.length) break;
    }
    return [SUCCESS, total];
  }

  writeAt(entry, buffers, offset) {
    const file = files.get(entry.path);
    if (!file) return [EBADF, 0];
    let total = 0;
    for (const buffer of buffers) {
      grow(file, offset + buffer.length);
      file.data.set(buffer, offset);
      offset += buffer.length;
      total += buffer.length;
      file.size = Math.max(file.size, offset);
    }
    touch(entry.path);
    return [SUCCESS, total];
  }

  functions() {
    const wasi = this;
    const handled = {
      args_sizes_get(count, size) {
        wasi.view().setUint32(count, 0, true);
        wasi.view().setUint32(size, 0, true);
        return SUCCESS;
      },
      args_get: () => SUCCESS,
      environ_sizes_get(count, size) {
        wasi.view().setUint32(count, 0, true);
        wasi.view().setUint32(size, 0, true);
        return SUCCESS;
      },
      environ_get: () => SUCCESS,
      clock_res_get(id, pointer) {
        wasi.view().setBigUint64(pointer, 1000n, true);
        return SUCCESS;
      },
      clock_time_get(id, precision, pointer) {
        const nanoseconds = id === 0 ? BigInt(Date.now()) * 1000000n : BigInt(Math.round(performance.now() * 1e6));
        wasi.view().setBigUint64(pointer, nanoseconds, true);
        return SUCCESS;
      },
      random_get(pointer, length) {
        const target = wasi.bytes(pointer, length);
        for (let at = 0; at < length; at += 65536) crypto.getRandomValues(target.subarray(at, Math.min(length, at + 65536)));
        return SUCCESS;
      },
      fd_write(fd, vectors, count, written) {
        const entry = wasi.fds.get(fd);
        if (!entry) return EBADF;
        const buffers = wasi.vectors(vectors, count);
        if (entry.kind === "stdio") {
          const total = buffers.reduce((sum, buffer) => sum + buffer.length, 0);
          wasi.stderr += buffers.map((buffer) => new TextDecoder().decode(buffer)).join("");
          wasi.view().setUint32(written, total, true);
          return SUCCESS;
        }
        if (entry.kind !== "file") return EISDIR;
        const offset = entry.append ? files.get(entry.path).size : entry.position;
        const [error, total] = wasi.writeAt(entry, buffers, offset);
        entry.position = offset + total;
        wasi.view().setUint32(written, total, true);
        return error;
      },
      fd_pwrite(fd, vectors, count, offset, written) {
        const entry = wasi.fds.get(fd);
        if (!entry || entry.kind !== "file") return EBADF;
        const [error, total] = wasi.writeAt(entry, wasi.vectors(vectors, count), Number(offset));
        wasi.view().setUint32(written, total, true);
        return error;
      },
      fd_read(fd, vectors, count, read) {
        const entry = wasi.fds.get(fd);
        if (!entry) return EBADF;
        if (entry.kind === "stdio") {
          wasi.view().setUint32(read, 0, true);
          return SUCCESS;
        }
        if (entry.kind !== "file") return EISDIR;
        const [error, total] = wasi.readAt(entry, wasi.vectors(vectors, count), entry.position);
        entry.position += total;
        wasi.view().setUint32(read, total, true);
        return error;
      },
      fd_pread(fd, vectors, count, offset, read) {
        const entry = wasi.fds.get(fd);
        if (!entry || entry.kind !== "file") return EBADF;
        const [error, total] = wasi.readAt(entry, wasi.vectors(vectors, count), Number(offset));
        wasi.view().setUint32(read, total, true);
        return error;
      },
      fd_seek(fd, offset, whence, result) {
        const entry = wasi.fds.get(fd);
        if (!entry || entry.kind !== "file") return EBADF;
        const base = whence === 0 ? 0 : whence === 1 ? entry.position : files.get(entry.path).size;
        const position = base + Number(offset);
        if (position < 0) return EINVAL;
        entry.position = position;
        wasi.view().setBigUint64(result, BigInt(position), true);
        return SUCCESS;
      },
      fd_tell(fd, result) {
        const entry = wasi.fds.get(fd);
        if (!entry || entry.kind !== "file") return EBADF;
        wasi.view().setBigUint64(result, BigInt(entry.position), true);
        return SUCCESS;
      },
      fd_close(fd) {
        return wasi.fds.delete(fd) ? SUCCESS : EBADF;
      },
      fd_sync: () => SUCCESS,
      fd_datasync: () => SUCCESS,
      fd_advise: () => SUCCESS,
      fd_allocate: () => SUCCESS,
      fd_fdstat_set_flags: () => SUCCESS,
      fd_fdstat_get(fd, pointer) {
        const entry = wasi.fds.get(fd);
        if (!entry) return EBADF;
        const view = wasi.view();
        view.setUint8(pointer, entry.kind === "dir" ? FILETYPE_DIRECTORY : entry.kind === "file" ? FILETYPE_REGULAR_FILE : FILETYPE_CHARACTER_DEVICE);
        view.setUint16(pointer + 2, entry.append ? FDFLAG_APPEND : 0, true);
        view.setBigUint64(pointer + 8, ALL_RIGHTS, true);
        view.setBigUint64(pointer + 16, ALL_RIGHTS, true);
        return SUCCESS;
      },
      fd_filestat_get(fd, pointer) {
        const entry = wasi.fds.get(fd);
        if (!entry) return EBADF;
        if (entry.kind === "file") wasi.stat(pointer, FILETYPE_REGULAR_FILE, files.get(entry.path).size);
        else wasi.stat(pointer, entry.kind === "dir" ? FILETYPE_DIRECTORY : FILETYPE_CHARACTER_DEVICE, 0);
        return SUCCESS;
      },
      fd_filestat_set_size(fd, size) {
        const entry = wasi.fds.get(fd);
        if (!entry || entry.kind !== "file") return EBADF;
        const file = files.get(entry.path);
        const length = Number(size);
        grow(file, length);
        if (length < file.size) file.data.fill(0, length, file.size);
        file.size = length;
        touch(entry.path);
        return SUCCESS;
      },
      fd_prestat_get(fd, pointer) {
        const entry = wasi.fds.get(fd);
        if (!entry || !entry.preopen) return EBADF;
        wasi.view().setUint8(pointer, 0);
        wasi.view().setUint32(pointer + 4, new TextEncoder().encode(entry.preopen).length, true);
        return SUCCESS;
      },
      fd_prestat_dir_name(fd, pointer, length) {
        const entry = wasi.fds.get(fd);
        if (!entry || !entry.preopen) return EBADF;
        wasi.bytes(pointer, length).set(new TextEncoder().encode(entry.preopen).subarray(0, length));
        return SUCCESS;
      },
      path_open(dirFd, lookup, pointer, length, oflags, rightsBase, rightsInheriting, fdflags, result) {
        const path = wasi.resolve(dirFd, pointer, length);
        if (path === null) return EBADF;
        if (directories.has(path)) {
          if (oflags & (OFLAG_CREAT | OFLAG_TRUNC)) return EISDIR;
          const fd = wasi.nextFd++;
          wasi.fds.set(fd, { kind: "dir", path });
          wasi.view().setUint32(result, fd, true);
          return SUCCESS;
        }
        if (oflags & OFLAG_DIRECTORY) return files.has(path) ? ENOTDIR : ENOENT;
        if (files.has(path)) {
          if (oflags & OFLAG_EXCL) return EEXIST;
          if (oflags & OFLAG_TRUNC) {
            files.get(path).size = 0;
            touch(path);
          }
        } else {
          if (!(oflags & OFLAG_CREAT)) return ENOENT;
          if (!directories.has(parent(path))) return ENOENT;
          files.set(path, { data: new Uint8Array(4096), size: 0 });
          touch(path);
        }
        const fd = wasi.nextFd++;
        wasi.fds.set(fd, { kind: "file", path, position: 0, append: (fdflags & FDFLAG_APPEND) !== 0 });
        wasi.view().setUint32(result, fd, true);
        return SUCCESS;
      },
      path_filestat_get(fd, flags, pointer, length, result) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (directories.has(path)) wasi.stat(result, FILETYPE_DIRECTORY, 0);
        else if (files.has(path)) wasi.stat(result, FILETYPE_REGULAR_FILE, files.get(path).size);
        else return ENOENT;
        return SUCCESS;
      },
      path_create_directory(fd, pointer, length) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (directories.has(path) || files.has(path)) return EEXIST;
        if (!directories.has(parent(path))) return ENOENT;
        directories.add(path);
        return SUCCESS;
      },
      path_remove_directory(fd, pointer, length) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (!directories.has(path)) return ENOENT;
        for (const name of [...files.keys(), ...directories]) if (name.startsWith(`${path}/`)) return ENOTEMPTY;
        directories.delete(path);
        return SUCCESS;
      },
      path_unlink_file(fd, pointer, length) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (!files.delete(path)) return directories.has(path) ? EISDIR : ENOENT;
        dirty.delete(path);
        removed.add(path);
        return SUCCESS;
      },
      path_rename(fd, oldPointer, oldLength, newFd, newPointer, newLength) {
        const from = wasi.resolve(fd, oldPointer, oldLength);
        const to = wasi.resolve(newFd, newPointer, newLength);
        if (from === null || to === null) return EBADF;
        if (!files.has(from)) return ENOENT;
        files.set(to, files.get(from));
        files.delete(from);
        for (const entry of wasi.fds.values()) if (entry.path === from) entry.path = to;
        removed.add(from);
        dirty.delete(from);
        touch(to);
        return SUCCESS;
      },
      poll_oneoff(subscriptions, events, count, written) {
        const view = wasi.view();
        for (let at = 0; at < count; at++) {
          view.setBigUint64(events + at * 32, view.getBigUint64(subscriptions + at * 48, true), true);
          view.setUint16(events + at * 32 + 8, 0, true);
          view.setUint8(events + at * 32 + 10, view.getUint8(subscriptions + at * 48 + 8));
        }
        view.setUint32(written, count, true);
        return SUCCESS;
      },
      sched_yield: () => SUCCESS,
      proc_exit(code) {
        throw new Error(`EggDB exited with code ${code}`);
      },
    };
    return new Proxy(handled, { get: (target, name) => target[name] ?? (() => ENOSYS) });
  }
}

async function opfsRoot() {
  if (typeof navigator === "undefined" || !navigator.storage?.getDirectory) return null;
  try {
    return await navigator.storage.getDirectory();
  } catch {
    return null;
  }
}

async function loadPersisted(root) {
  for await (const [name, handle] of root.entries()) {
    if (handle.kind !== "file" || !name.startsWith(OPFS_PREFIX)) continue;
    const path = decodeURIComponent(name.slice(OPFS_PREFIX.length));
    if (files.has(path) || dirty.has(path)) continue;
    const data = new Uint8Array(await (await handle.getFile()).arrayBuffer());
    const folders = path.split("/").slice(1, -1);
    for (let at = 1; at <= folders.length; at++) directories.add(`/${folders.slice(0, at).join("/")}`);
    files.set(path, { data, size: data.length });
  }
}

async function saveToOpfs(root) {
  for (const path of removed) {
    await root.removeEntry(OPFS_PREFIX + encodeURIComponent(path)).catch(() => {});
  }
  removed.clear();
  for (const path of [...dirty]) {
    const file = files.get(path);
    dirty.delete(path);
    if (!file) continue;
    const handle = await root.getFileHandle(OPFS_PREFIX + encodeURIComponent(path), { create: true });
    const writable = await handle.createWritable();
    await writable.write(file.data.subarray(0, file.size));
    await writable.close();
  }
}

async function compile(wasm) {
  if (wasm instanceof WebAssembly.Module) return wasm;
  if (wasm instanceof ArrayBuffer || ArrayBuffer.isView(wasm)) return WebAssembly.compile(wasm);
  const response = await fetch(wasm);
  return WebAssembly.compileStreaming ? WebAssembly.compileStreaming(response) : WebAssembly.compile(await response.arrayBuffer());
}

export class EggDB {
  static async open({ wasm = "eggdb.wasm", path = "/playground.eggdb", dialect = "postgresql", persist = true } = {}) {
    const module = await compile(wasm);
    const root = persist ? await opfsRoot() : null;
    if (root) await loadPersisted(root);
    const db = new EggDB(module, path, dialect, root);
    await db.start();
    return db;
  }

  constructor(module, path, dialect, root) {
    this.module = module;
    this.path = normalize("/", path);
    this.dialect = dialect;
    this.root = root;
    this.persistent = root !== null;
    this.version = "";
  }

  async start() {
    const code = DIALECTS.indexOf(this.dialect);
    if (code < 0) throw new Error(`unknown dialect ${this.dialect}`);
    this.wasi = new Wasi();
    this.instance = await WebAssembly.instantiate(this.module, this.wasi.imports);
    this.exports = this.instance.exports;
    this.wasi.memory = this.exports.memory;
    if (this.exports._initialize) this.exports._initialize();
    this.version = this.call("eggdb_wasm_version").version ?? "";
    this.handle = this.send(this.path, () => this.exports.eggdb_wasm_open(code));
    if (this.handle === 0) throw new Error(this.answer().error ?? "EggDB could not open the database");
  }

  send(text, run) {
    const bytes = new TextEncoder().encode(text);
    const start = this.exports.eggdb_wasm_input(bytes.length);
    new Uint8Array(this.exports.memory.buffer, start, bytes.length).set(bytes);
    return run();
  }

  answer() {
    const start = this.exports.eggdb_wasm_output();
    const length = this.exports.eggdb_wasm_output_len();
    return JSON.parse(new TextDecoder().decode(new Uint8Array(this.exports.memory.buffer, start, length)));
  }

  call(name) {
    this.exports[name]();
    return this.answer();
  }

  async execute(sql) {
    const started = performance.now();
    this.send(sql, () => this.exports.eggdb_wasm_execute(this.handle));
    const out = this.answer();
    out.elapsedMs = performance.now() - started;
    if (this.root) await saveToOpfs(this.root);
    return out;
  }

  async reset() {
    this.exports.eggdb_wasm_close(this.handle);
    for (const name of [...files.keys()]) {
      if (name === this.path || name.startsWith(`${this.path}-`) || name.startsWith(`${this.path}.`)) {
        files.delete(name);
        dirty.delete(name);
        removed.add(name);
      }
    }
    if (this.root) await saveToOpfs(this.root);
    await this.start();
  }

  close() {
    this.exports.eggdb_wasm_close(this.handle);
  }
}
