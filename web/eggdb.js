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
const ACCESS_HANDLE_TRIES = 100;
const ACCESS_HANDLE_RETRY_MS = 20;

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

function opfsName(path) {
  return OPFS_PREFIX + encodeURIComponent(path);
}

function addParents(directories, path) {
  const folders = path.split("/").slice(1, -1);
  for (let at = 1; at <= folders.length; at++) directories.add(`/${folders.slice(0, at).join("/")}`);
}

export class MemoryFiles {
  constructor() {
    this.files = new Map();
    this.directories = new Set(["/"]);
    this.dirty = new Set();
    this.removed = new Set();
  }

  touch(path) {
    this.dirty.add(path);
    this.removed.delete(path);
  }

  isDirectory(path) {
    return this.directories.has(path);
  }

  makeDirectory(path) {
    this.directories.add(path);
  }

  removeDirectory(path) {
    this.directories.delete(path);
  }

  hasChildren(path) {
    for (const name of [...this.files.keys(), ...this.directories]) if (name.startsWith(`${path}/`)) return true;
    return false;
  }

  has(path) {
    return this.files.has(path);
  }

  size(path) {
    return this.files.get(path).size;
  }

  grow(file, size) {
    if (size <= file.data.length) return;
    const bigger = new Uint8Array(Math.max(size, file.data.length * 2, 4096));
    bigger.set(file.data.subarray(0, file.size));
    file.data = bigger;
  }

  read(path, buffer, offset) {
    const file = this.files.get(path);
    const available = Math.max(0, Math.min(buffer.length, file.size - offset));
    buffer.set(file.data.subarray(offset, offset + available));
    return available;
  }

  write(path, buffer, offset) {
    const file = this.files.get(path);
    this.grow(file, offset + buffer.length);
    file.data.set(buffer, offset);
    file.size = Math.max(file.size, offset + buffer.length);
    this.touch(path);
    return buffer.length;
  }

  setSize(path, length) {
    const file = this.files.get(path);
    this.grow(file, length);
    if (length < file.size) file.data.fill(0, length, file.size);
    file.size = length;
    this.touch(path);
  }

  create(path) {
    this.files.set(path, { data: new Uint8Array(4096), size: 0 });
    this.touch(path);
  }

  unlink(path) {
    this.files.delete(path);
    this.dirty.delete(path);
    this.removed.add(path);
  }

  rename(from, to) {
    this.files.set(to, this.files.get(from));
    this.files.delete(from);
    this.removed.add(from);
    this.dirty.delete(from);
    this.touch(to);
  }

  sync() {}

  contents(path) {
    const file = this.files.get(path);
    return file.data.slice(0, file.size);
  }

  adopt(path, data) {
    addParents(this.directories, path);
    this.files.set(path, { data, size: data.length });
  }

  async load(root) {
    for await (const [name, handle] of root.entries()) {
      if (handle.kind !== "file" || !name.startsWith(OPFS_PREFIX)) continue;
      const path = decodeURIComponent(name.slice(OPFS_PREFIX.length));
      if (this.files.has(path) || this.dirty.has(path)) continue;
      this.adopt(path, new Uint8Array(await (await handle.getFile()).arrayBuffer()));
    }
  }

  async save(root) {
    for (const path of this.removed) {
      await root.removeEntry(opfsName(path)).catch(() => {});
    }
    this.removed.clear();
    for (const path of [...this.dirty]) {
      const file = this.files.get(path);
      this.dirty.delete(path);
      if (!file) continue;
      const handle = await root.getFileHandle(opfsName(path), { create: true });
      const writable = await handle.createWritable();
      await writable.write(file.data.subarray(0, file.size));
      await writable.close();
    }
  }

  forget(prefix) {
    for (const name of [...this.files.keys()]) {
      if (name === prefix || name.startsWith(`${prefix}-`) || name.startsWith(`${prefix}.`)) this.unlink(name);
    }
  }
}

export class OpfsSyncFiles {
  constructor(root) {
    this.root = root;
    this.handles = new Map();
    this.memory = new MemoryFiles();
  }

  static async open(root, paths) {
    const store = new OpfsSyncFiles(root);
    for await (const [name, handle] of root.entries()) {
      if (handle.kind !== "file" || !name.startsWith(OPFS_PREFIX)) continue;
      await store.attach(decodeURIComponent(name.slice(OPFS_PREFIX.length)));
    }
    for (const path of paths) if (!store.handles.has(path)) await store.attach(path);
    return store;
  }

  async attach(path) {
    const file = await this.root.getFileHandle(opfsName(path), { create: true });
    for (let tries = 0; ; tries++) {
      try {
        this.handles.set(path, await file.createSyncAccessHandle());
        break;
      } catch (error) {
        if (error?.name !== "NoModificationAllowedError" || tries >= ACCESS_HANDLE_TRIES) throw error;
        await new Promise((done) => setTimeout(done, ACCESS_HANDLE_RETRY_MS));
      }
    }
    addParents(this.memory.directories, path);
  }

  isDirectory(path) {
    return this.memory.isDirectory(path);
  }

  makeDirectory(path) {
    this.memory.makeDirectory(path);
  }

  removeDirectory(path) {
    this.memory.removeDirectory(path);
  }

  hasChildren(path) {
    for (const name of this.handles.keys()) if (name.startsWith(`${path}/`)) return true;
    return this.memory.hasChildren(path);
  }

  has(path) {
    return this.handles.has(path) || this.memory.has(path);
  }

  size(path) {
    const handle = this.handles.get(path);
    return handle ? handle.getSize() : this.memory.size(path);
  }

  read(path, buffer, offset) {
    const handle = this.handles.get(path);
    return handle ? handle.read(buffer, { at: offset }) : this.memory.read(path, buffer, offset);
  }

  write(path, buffer, offset) {
    const handle = this.handles.get(path);
    return handle ? handle.write(buffer, { at: offset }) : this.memory.write(path, buffer, offset);
  }

  setSize(path, length) {
    const handle = this.handles.get(path);
    if (handle) handle.truncate(length);
    else this.memory.setSize(path, length);
  }

  create(path) {
    this.memory.create(path);
  }

  unlink(path) {
    const handle = this.handles.get(path);
    if (!handle) {
      this.memory.unlink(path);
      return;
    }
    handle.close();
    this.handles.delete(path);
    this.memory.removed.add(path);
  }

  rename(from, to) {
    const data = this.contents(from);
    const target = this.handles.get(to);
    if (target) {
      target.truncate(0);
      target.write(data, { at: 0 });
      target.flush();
      this.unlink(from);
      return;
    }
    if (this.handles.has(from)) {
      this.unlink(from);
      this.memory.adopt(to, data);
      this.memory.touch(to);
      return;
    }
    this.memory.rename(from, to);
  }

  sync(path) {
    this.handles.get(path)?.flush();
  }

  contents(path) {
    const handle = this.handles.get(path);
    if (!handle) return this.memory.contents(path);
    const data = new Uint8Array(handle.getSize());
    handle.read(data, { at: 0 });
    return data;
  }

  async save() {
    await this.memory.save(this.root);
    for (const path of [...this.memory.files.keys()]) {
      if (!this.handles.has(path)) {
        const data = this.memory.contents(path);
        await this.attach(path);
        this.handles.get(path).write(data, { at: 0 });
        this.handles.get(path).flush();
        this.memory.files.delete(path);
      }
    }
  }

  forget(prefix) {
    for (const name of [...this.handles.keys()]) {
      if (name === prefix || name.startsWith(`${prefix}-`) || name.startsWith(`${prefix}.`)) this.unlink(name);
    }
    this.memory.forget(prefix);
  }

  close() {
    for (const handle of this.handles.values()) handle.close();
    this.handles.clear();
  }
}

const SLEEP_CELL = typeof SharedArrayBuffer === "function" ? new Int32Array(new SharedArrayBuffer(4)) : null;

function sleepMs(milliseconds) {
  if (milliseconds <= 0) return;
  if (SLEEP_CELL) {
    try {
      Atomics.wait(SLEEP_CELL, 0, 0, milliseconds);
      return;
    } catch {}
  }
  const until = performance.now() + milliseconds;
  while (performance.now() < until) {}
}

function clockNow(id) {
  return id === 0 ? BigInt(Date.now()) * 1000000n : BigInt(Math.round(performance.now() * 1e6));
}

export class Wasi {
  constructor(store) {
    this.store = store;
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
    if (!this.store.has(entry.path)) return [EBADF, 0];
    let total = 0;
    for (const buffer of buffers) {
      const available = this.store.read(entry.path, buffer, offset);
      total += available;
      offset += available;
      if (available < buffer.length) break;
    }
    return [SUCCESS, total];
  }

  writeAt(entry, buffers, offset) {
    if (!this.store.has(entry.path)) return [EBADF, 0];
    let total = 0;
    for (const buffer of buffers) {
      const wrote = this.store.write(entry.path, buffer, offset);
      offset += wrote;
      total += wrote;
    }
    return [SUCCESS, total];
  }

  sleepFor(subscriptions, events, count, written) {
    const view = this.view();
    let earliest = null;
    let anyFd = false;
    for (let at = 0; at < count; at++) {
      const base = subscriptions + at * 48;
      if (view.getUint8(base + 8) !== 0) {
        anyFd = true;
        continue;
      }
      const id = view.getUint32(base + 16, true);
      const timeout = view.getBigUint64(base + 24, true);
      const absolute = (view.getUint16(base + 40, true) & 1) === 1;
      const now = clockNow(id);
      const wait = absolute ? (timeout > now ? timeout - now : 0n) : timeout;
      if (earliest === null || wait < earliest) earliest = wait;
    }
    if (!anyFd && earliest !== null) sleepMs(Number(earliest / 1000000n));
    let fired = 0;
    for (let at = 0; at < count; at++) {
      const base = subscriptions + at * 48;
      const kind = view.getUint8(base + 8);
      if (kind === 0 && anyFd) continue;
      if (kind === 0 && earliest !== null) {
        const timeout = view.getBigUint64(base + 24, true);
        const absolute = (view.getUint16(base + 40, true) & 1) === 1;
        if (!absolute && timeout > earliest) continue;
        if (absolute && timeout > clockNow(view.getUint32(base + 16, true))) continue;
      }
      const event = events + fired * 32;
      view.setBigUint64(event, view.getBigUint64(base, true), true);
      view.setUint16(event + 8, 0, true);
      view.setUint8(event + 10, kind);
      fired++;
    }
    view.setUint32(written, fired, true);
    return SUCCESS;
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
        wasi.view().setBigUint64(pointer, clockNow(id), true);
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
        const offset = entry.append ? wasi.store.size(entry.path) : entry.position;
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
        const base = whence === 0 ? 0 : whence === 1 ? entry.position : wasi.store.size(entry.path);
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
      fd_sync(fd) {
        const entry = wasi.fds.get(fd);
        if (entry?.kind === "file") wasi.store.sync(entry.path);
        return entry ? SUCCESS : EBADF;
      },
      fd_datasync(fd) {
        const entry = wasi.fds.get(fd);
        if (entry?.kind === "file") wasi.store.sync(entry.path);
        return entry ? SUCCESS : EBADF;
      },
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
        if (entry.kind === "file") wasi.stat(pointer, FILETYPE_REGULAR_FILE, wasi.store.size(entry.path));
        else wasi.stat(pointer, entry.kind === "dir" ? FILETYPE_DIRECTORY : FILETYPE_CHARACTER_DEVICE, 0);
        return SUCCESS;
      },
      fd_filestat_set_size(fd, size) {
        const entry = wasi.fds.get(fd);
        if (!entry || entry.kind !== "file") return EBADF;
        wasi.store.setSize(entry.path, Number(size));
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
        if (wasi.store.isDirectory(path)) {
          if (oflags & (OFLAG_CREAT | OFLAG_TRUNC)) return EISDIR;
          const fd = wasi.nextFd++;
          wasi.fds.set(fd, { kind: "dir", path });
          wasi.view().setUint32(result, fd, true);
          return SUCCESS;
        }
        if (oflags & OFLAG_DIRECTORY) return wasi.store.has(path) ? ENOTDIR : ENOENT;
        if (wasi.store.has(path)) {
          if (oflags & OFLAG_EXCL) return EEXIST;
          if (oflags & OFLAG_TRUNC) wasi.store.setSize(path, 0);
        } else {
          if (!(oflags & OFLAG_CREAT)) return ENOENT;
          if (!wasi.store.isDirectory(parent(path))) return ENOENT;
          wasi.store.create(path);
        }
        const fd = wasi.nextFd++;
        wasi.fds.set(fd, { kind: "file", path, position: 0, append: (fdflags & FDFLAG_APPEND) !== 0 });
        wasi.view().setUint32(result, fd, true);
        return SUCCESS;
      },
      path_filestat_get(fd, flags, pointer, length, result) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (wasi.store.isDirectory(path)) wasi.stat(result, FILETYPE_DIRECTORY, 0);
        else if (wasi.store.has(path)) wasi.stat(result, FILETYPE_REGULAR_FILE, wasi.store.size(path));
        else return ENOENT;
        return SUCCESS;
      },
      path_create_directory(fd, pointer, length) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (wasi.store.isDirectory(path) || wasi.store.has(path)) return EEXIST;
        if (!wasi.store.isDirectory(parent(path))) return ENOENT;
        wasi.store.makeDirectory(path);
        return SUCCESS;
      },
      path_remove_directory(fd, pointer, length) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (!wasi.store.isDirectory(path)) return ENOENT;
        if (wasi.store.hasChildren(path)) return ENOTEMPTY;
        wasi.store.removeDirectory(path);
        return SUCCESS;
      },
      path_unlink_file(fd, pointer, length) {
        const path = wasi.resolve(fd, pointer, length);
        if (path === null) return EBADF;
        if (!wasi.store.has(path)) return wasi.store.isDirectory(path) ? EISDIR : ENOENT;
        wasi.store.unlink(path);
        return SUCCESS;
      },
      path_rename(fd, oldPointer, oldLength, newFd, newPointer, newLength) {
        const from = wasi.resolve(fd, oldPointer, oldLength);
        const to = wasi.resolve(newFd, newPointer, newLength);
        if (from === null || to === null) return EBADF;
        if (!wasi.store.has(from)) return ENOENT;
        wasi.store.rename(from, to);
        for (const entry of wasi.fds.values()) if (entry.path === from) entry.path = to;
        return SUCCESS;
      },
      poll_oneoff(subscriptions, events, count, written) {
        return wasi.sleepFor(subscriptions, events, count, written);
      },
      sched_yield: () => SUCCESS,
      proc_exit(code) {
        throw new Error(`EggDB exited with code ${code}`);
      },
    };
    return new Proxy(handled, { get: (target, name) => target[name] ?? (() => ENOSYS) });
  }
}

const pageFiles = new MemoryFiles();

export async function opfsRoot() {
  if (typeof navigator === "undefined" || !navigator.storage?.getDirectory) return null;
  try {
    return await navigator.storage.getDirectory();
  } catch {
    return null;
  }
}

export async function compile(wasm) {
  if (wasm instanceof WebAssembly.Module) return wasm;
  if (wasm instanceof ArrayBuffer || ArrayBuffer.isView(wasm)) return WebAssembly.compile(wasm);
  const response = await fetch(wasm);
  return WebAssembly.compileStreaming ? WebAssembly.compileStreaming(response) : WebAssembly.compile(await response.arrayBuffer());
}

export class EggDB {
  static async open({ wasm = "eggdb.wasm", path = "/playground.eggdb", dialect = "postgresql", persist = true } = {}) {
    const module = await compile(wasm);
    const root = persist ? await opfsRoot() : null;
    if (root) await pageFiles.load(root);
    const db = new EggDB(module, path, dialect, pageFiles, root ? () => pageFiles.save(root) : null);
    await db.start();
    return db;
  }

  static async openInWorker({ wasm = "eggdb.wasm", path = "/playground.eggdb", dialect = "postgresql" } = {}) {
    const module = await compile(wasm);
    const root = await opfsRoot();
    if (!root || typeof FileSystemFileHandle === "undefined" || !FileSystemFileHandle.prototype.createSyncAccessHandle) {
      return EggDB.open({ wasm: module, path, dialect, persist: true });
    }
    const normalized = normalize("/", path);
    const store = await OpfsSyncFiles.open(root, [normalized]);
    const db = new EggDB(module, normalized, dialect, store, () => store.save());
    db.durable = true;
    await db.start();
    return db;
  }

  constructor(module, path, dialect, store, persist) {
    this.module = module;
    this.path = normalize("/", path);
    this.dialect = dialect;
    this.store = store;
    this.persist = persist;
    this.persistent = persist !== null;
    this.durable = false;
    this.version = "";
  }

  async start() {
    this.wasi = new Wasi(this.store);
    this.instance = await WebAssembly.instantiate(this.module, this.wasi.imports);
    this.exports = this.instance.exports;
    this.wasi.memory = this.exports.memory;
    if (this.exports._initialize) this.exports._initialize();
    this.version = this.call("eggdb_wasm_version").version ?? "";
    this.handle = this.openSession(this.dialect);
  }

  openSession(dialect) {
    const code = DIALECTS.indexOf(dialect);
    if (code < 0) throw new Error(`unknown dialect ${dialect}`);
    const handle = this.send(this.path, () => this.exports.eggdb_wasm_open(code));
    if (handle === 0) throw new Error(this.answer().error ?? "EggDB could not open the database");
    return handle;
  }

  closeSession(handle) {
    this.exports.eggdb_wasm_close(handle);
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

  execute(sql) {
    return this.executeIn(this.handle, sql);
  }

  async executeIn(handle, sql) {
    const started = performance.now();
    this.send(sql, () => this.exports.eggdb_wasm_execute(handle));
    const out = this.answer();
    out.elapsedMs = performance.now() - started;
    if (this.persist) await this.persist();
    return out;
  }

  async reset() {
    this.exports.eggdb_wasm_close(this.handle);
    this.store.forget(this.path);
    if (this.persist) await this.persist();
    await this.start();
  }

  close() {
    this.exports.eggdb_wasm_close(this.handle);
    this.store.close?.();
  }
}

export class SharedEggDB {
  static async open({ wasm = "eggdb.wasm", path = "/playground.eggdb", dialect = "postgresql", worker = new URL("./eggdb-worker.js", import.meta.url) } = {}) {
    const shared = new SharedEggDB(new Worker(worker, { type: "module" }));
    const opened = await shared.request({ type: "open", wasm: wasm instanceof URL ? wasm.href : new URL(wasm, location.href).href, path, dialect });
    shared.version = opened.version;
    shared.durable = opened.durable;
    shared.persistent = true;
    return shared;
  }

  constructor(worker) {
    this.worker = worker;
    this.next = 1;
    this.waiting = new Map();
    this.leader = false;
    this.worker.onmessage = ({ data }) => {
      if (data.type === "role") {
        this.leader = data.leader;
        return;
      }
      const waiter = this.waiting.get(data.id);
      if (!waiter) return;
      this.waiting.delete(data.id);
      if (data.failure) waiter.reject(new Error(data.failure));
      else waiter.resolve(data.answer);
    };
    this.worker.onerror = (event) => {
      for (const waiter of this.waiting.values()) waiter.reject(new Error(event.message || "the EggDB worker failed to start"));
      this.waiting.clear();
    };
  }

  request(message) {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.waiting.set(id, { resolve, reject });
      this.worker.postMessage({ ...message, id });
    });
  }

  execute(sql) {
    return this.request({ type: "execute", sql });
  }

  reset() {
    return this.request({ type: "reset" });
  }

  close() {
    this.worker.terminate();
    for (const waiter of this.waiting.values()) waiter.reject(new Error("the database was closed"));
    this.waiting.clear();
  }
}
