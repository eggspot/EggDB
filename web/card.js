import { Wasi, compile } from "./eggdb.js";

const ACTIONS = ["none", "open", "browse", "cancel", "show-layout-picker", "load-sample-dataset", "quit"];
const KEY_IGNORED = 0xffffffff;
const THEMES = { light: 0, dark: 1, "high-contrast": 2 };
const SHORTCUT_LETTERS = new Set(["a", "o", "q"]);
const CONTAINERS = new Set(["document", "group", "tablist", "tabpanel"]);

function modifierBits(event) {
  return (event.shiftKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) | (event.altKey ? 4 : 0) | (event.metaKey ? 8 : 0);
}

function sameStructure(old, next) {
  return old.length === next.length && old.every((node, at) => node.id === next[at].id && node.role === next[at].role && node.parent === next[at].parent);
}

function mirrorElement(node) {
  switch (node.role) {
    case "textbox": {
      const input = document.createElement("input");
      input.type = "text";
      input.autocomplete = "off";
      input.spellcheck = false;
      return input;
    }
    case "checkbox": {
      const input = document.createElement("input");
      input.type = "checkbox";
      return input;
    }
    case "button":
      return document.createElement("button");
    case "tab": {
      const tab = document.createElement("button");
      tab.setAttribute("role", "tab");
      return tab;
    }
    case "link": {
      const link = document.createElement("a");
      link.href = "#";
      return link;
    }
    case "heading":
      return document.createElement("h1");
    default: {
      const element = document.createElement("div");
      element.setAttribute("role", node.role);
      return element;
    }
  }
}

function place(element, node, parent) {
  const left = node.x - (parent ? parent.x : 0);
  const top = node.y - (parent ? parent.y : 0);
  Object.assign(element.style, {
    position: "absolute",
    left: `${left}px`,
    top: `${top}px`,
    width: `${node.width}px`,
    height: `${node.height}px`,
    margin: "0",
    padding: "0",
    border: "0",
    opacity: "0",
    font: "inherit",
    overflow: "hidden",
  });
}

function describe(element, node) {
  element.dataset.node = String(node.id);
  if (CONTAINERS.has(node.role) || node.role === "textbox" || node.role === "checkbox") element.setAttribute("aria-label", node.name);
  else if (node.role !== "status" && element.textContent !== node.name) element.textContent = node.name;
  if (node.role === "textbox") {
    if (element.value !== node.value) element.value = node.value ?? "";
    element.placeholder = node.placeholder ?? "";
  }
  if (node.role === "checkbox") element.checked = node.checked === true;
  if (node.role === "tab") {
    element.setAttribute("aria-selected", String(node.selected));
    element.tabIndex = node.selected ? 0 : -1;
  }
  if (node.role === "button" || node.role === "tab") element.disabled = node.disabled;
  if (node.role === "button" && node.default) element.type = "submit";
  setOptional(element, "aria-expanded", node.expanded);
  setOptional(element, "aria-invalid", node.invalid ? true : null);
  setOptional(element, "aria-readonly", node.readonly ? true : null);
  setOptional(element, "aria-description", node.description);
  setOptional(element, "aria-keyshortcuts", node.shortcut);
}

function setOptional(element, name, value) {
  if (value === null || value === undefined) element.removeAttribute(name);
  else element.setAttribute(name, String(value));
}

export class ConnectCard {
  static async mount(container, { wasm = "eggdb-gui.wasm", theme = "light", fonts = {}, onAction = () => {}, autofocus = false } = {}) {
    const module = await compile(wasm);
    const wasi = new Wasi();
    const instance = await WebAssembly.instantiate(module, wasi.imports);
    wasi.memory = instance.exports.memory;
    if (instance.exports._initialize) instance.exports._initialize();
    const card = new ConnectCard(container, instance.exports, theme, onAction);
    for (const [role, url] of [[0, fonts.ui], [1, fonts.bold]]) {
      if (url) await card.loadFont(role, url);
    }
    card.update();
    if (autofocus) card.syncFocus(true);
    return card;
  }

  constructor(container, exports, theme, onAction) {
    if (!(theme in THEMES)) throw new Error(`unknown theme ${theme}`);
    this.exports = exports;
    this.theme = theme;
    this.onAction = onAction;
    this.container = container;
    this.nodes = [];
    this.elements = new Map();
    container.style.position = "relative";
    this.canvas = document.createElement("canvas");
    this.canvas.setAttribute("aria-hidden", "true");
    Object.assign(this.canvas.style, { position: "absolute", left: "0", top: "0", width: "100%", height: "100%" });
    this.mirror = document.createElement("div");
    Object.assign(this.mirror.style, { position: "absolute", left: "0", top: "0", width: "100%", height: "100%" });
    this.live = document.createElement("div");
    this.live.setAttribute("role", "status");
    this.live.setAttribute("aria-live", "polite");
    Object.assign(this.live.style, { position: "absolute", width: "1px", height: "1px", overflow: "hidden", clipPath: "inset(50%)" });
    container.append(this.canvas, this.mirror, this.live);
    this.mirror.addEventListener("keydown", (event) => this.onKey(event));
    this.mirror.addEventListener("focusin", (event) => this.onFocus(event));
    this.mirror.addEventListener("click", (event) => this.onClick(event));
    this.mirror.addEventListener("input", () => this.update());
    if (typeof ResizeObserver === "function") new ResizeObserver(() => this.update()).observe(container);
  }

  send(text) {
    const bytes = typeof text === "string" ? new TextEncoder().encode(text) : text;
    const start = this.exports.eggdb_wasm_input(bytes.length);
    new Uint8Array(this.exports.memory.buffer, start, bytes.length).set(bytes);
  }

  output() {
    const start = this.exports.eggdb_wasm_output();
    const length = this.exports.eggdb_wasm_output_len();
    return new Uint8Array(this.exports.memory.buffer, start, length);
  }

  async loadFont(role, url) {
    const response = await fetch(url);
    this.send(new Uint8Array(await response.arrayBuffer()));
    return this.exports.eggdb_wasm_card_font(role) === 1;
  }

  setTheme(theme) {
    if (!(theme in THEMES)) throw new Error(`unknown theme ${theme}`);
    this.theme = theme;
    this.update();
  }

  render() {
    const width = Math.max(1, Math.round(this.container.clientWidth || 1280));
    const height = Math.max(1, Math.round(this.container.clientHeight || 800));
    const scale = window.devicePixelRatio || 1;
    const frameWidth = this.exports.eggdb_wasm_card_render(width, height, scale, THEMES[this.theme]);
    const pixels = this.output();
    const frameHeight = pixels.length / 4 / frameWidth;
    this.canvas.width = frameWidth;
    this.canvas.height = frameHeight;
    const image = new ImageData(new Uint8ClampedArray(pixels), frameWidth, frameHeight);
    this.canvas.getContext("2d").putImageData(image, 0, 0);
  }

  tree() {
    this.exports.eggdb_wasm_card_accessibility();
    return JSON.parse(new TextDecoder().decode(this.output()));
  }

  update() {
    this.render();
    const nodes = this.tree();
    if (!sameStructure(this.nodes, nodes)) this.rebuild(nodes);
    const byId = new Map(nodes.map((node) => [node.id, node]));
    let announcement = "";
    for (const node of nodes) {
      const element = this.elements.get(node.id);
      if (node.role === "status") announcement = node.name;
      if (!element) continue;
      describe(element, node);
      place(element, node, byId.get(node.parent));
    }
    if (this.live.textContent !== announcement) this.live.textContent = announcement;
    this.nodes = nodes;
    this.syncFocus(false);
  }

  rebuild(nodes) {
    this.mirror.replaceChildren();
    this.elements.clear();
    for (const node of nodes) {
      if (node.role === "document") {
        this.mirror.setAttribute("role", "document");
        this.mirror.setAttribute("aria-label", node.name);
        this.mirror.dataset.node = String(node.id);
        this.elements.set(node.id, null);
        continue;
      }
      if (node.role === "status") continue;
      const element = mirrorElement(node);
      const parent = this.elements.get(node.parent);
      (parent ?? this.mirror).append(element);
      this.elements.set(node.id, element);
    }
  }

  focusedElement() {
    const node = this.nodes.find((candidate) => candidate.focused);
    return node ? this.elements.get(node.id) : null;
  }

  syncFocus(force) {
    const element = this.focusedElement();
    if (!element || document.activeElement === element) return;
    if (force || this.mirror.contains(document.activeElement)) element.focus({ preventScroll: true });
  }

  act(code) {
    if (code === 0 || code === KEY_IGNORED) return;
    const action = ACTIONS[code] ?? "none";
    this.onAction(action);
    this.container.dispatchEvent(new CustomEvent("eggdb-action", { detail: action }));
  }

  key(name, modifiers) {
    this.send(name);
    const code = this.exports.eggdb_wasm_card_key(modifiers);
    if (code !== KEY_IGNORED) this.update();
    this.act(code);
    return code;
  }

  onKey(event) {
    if (event.isComposing) return;
    const primary = event.ctrlKey || event.metaKey;
    if (primary && !SHORTCUT_LETTERS.has(event.key.toLowerCase())) return;
    if (this.key(event.key, modifierBits(event)) !== KEY_IGNORED) event.preventDefault();
  }

  onFocus(event) {
    const id = Number(event.target?.dataset?.node);
    if (!Number.isInteger(id)) return;
    const focused = this.nodes.find((node) => node.focused);
    if (focused && focused.id === id) return;
    if (this.exports.eggdb_wasm_card_focus(id) === 1) this.update();
  }

  onClick(event) {
    const id = Number(event.target?.dataset?.node);
    const node = this.nodes.find((candidate) => candidate.id === id);
    if (!node) return;
    event.preventDefault();
    if (node.role === "textbox" || node.disabled) return;
    if (this.exports.eggdb_wasm_card_focus(id) !== 1) return;
    this.key(node.role === "checkbox" ? " " : "Enter", 0);
  }
}
