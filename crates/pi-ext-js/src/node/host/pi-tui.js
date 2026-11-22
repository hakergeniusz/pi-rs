// `@earendil-works/pi-tui` virtual module — the component/builder surface the
// bundled MCP (and codemode/llama) extensions import. Upstream bundles the
// whole TUI package (`core/extensions/virtual-modules.ts:5,23,35`).
//
// QuickJS cannot own a real terminal: there is no process.stdout, no terminal
// size, no key event loop. This shim therefore provides *faithful data-shape
// constructors*: the classes keep the same constructor arguments, public
// fields, methods and `render(width): string[]` contract as upstream, and
// `render` produces plain (ANSI-preserving) string rows so a host-side
// renderer can paint them. It is not a drop-in terminal runtime — see
// WIRING.md ("pi-tui limitation") for the exact bounds.
//
// Ground truth:
//   `packages/tui/src/tui.ts:364-421`        Container
//   `packages/tui/src/components/text.ts`    Text
//   `packages/tui/src/components/spacer.ts`  Spacer
//   `packages/tui/src/components/select-list.ts:14-180` SelectList / SelectItem
//   `packages/tui/src/components/input.ts:56-...` Input
//   `packages/tui/src/utils.ts:250-...,1186-...` visibleWidth / truncateToWidth
//   `packages/tui/src/keys.ts:163-...`        Key

const ANSI = /\u001b\[[0-?]*[ -/]*[@-~]/g;

/** Width of a string ignoring ANSI escape sequences and tabs (3 columns). */
export function visibleWidth(str) {
  if (!str) return 0;
  let width = 0;
  for (const character of String(str).replace(ANSI, "").replace(/\t/g, "   ")) {
    // Combining marks and zero-width joiners do not occupy a column. QuickJS
    // has no Intl.Segmenter; a code-point walk is the faithful approximation.
    const code = character.codePointAt(0);
    if (
      (code >= 0x0300 && code <= 0x036f) ||
      code === 0x200d ||
      (code >= 0xfe00 && code <= 0xfe0f)
    ) {
      continue;
    }
    width += 1;
  }
  return width;
}

/** Cuts a styled line to `width` columns, keeping ANSI codes coherent. */
export function truncateToWidth(str, width, ellipsis = "…") {
  const text = String(str ?? "");
  const limit = Math.max(0, width);
  if (text.length === 0 || visibleWidth(text) <= limit) return text;
  const marker = visibleWidth(ellipsis) > 0 ? ellipsis : "";
  const markerWidth = visibleWidth(marker);
  const budget = Math.max(0, limit - markerWidth);
  let columns = 0;
  let cut = 0;
  let inEscape = false;
  for (let index = 0; index < text.length; index++) {
    const character = text[index];
    if (character === "\u001b") inEscape = true;
    if (inEscape) {
      cut = index + 1;
      if ((character >= "@" && character <= "~") || (character >= "a" && character <= "z")) {
        inEscape = false;
      }
      continue;
    }
    const code = character.codePointAt(0);
    const zeroWidth =
      (code >= 0x0300 && code <= 0x036f) || code === 0x200d || (code >= 0xfe00 && code <= 0xfe0f);
    if (!zeroWidth && columns >= budget) break;
    if (!zeroWidth) columns += 1;
    cut = index + 1;
  }
  return text.slice(0, cut).replace(ANSI, "") + marker;
}

/** Base class: the small slice of `Component` the constructors implement. */
export class Component {
  render(_width) {
    return [];
  }
  invalidate() {}
  handleInput(_data) {}
}

/** A vertical stack of components (`tui.ts:364`). */
export class Container extends Component {
  constructor() {
    super();
    this.children = [];
    this.mouseLayout = undefined;
  }
  addChild(component) {
    this.children.push(component);
  }
  removeChild(component) {
    const index = this.children.indexOf(component);
    if (index !== -1) this.children.splice(index, 1);
  }
  clear() {
    this.children = [];
  }
  invalidate() {
    for (const child of this.children) child.invalidate?.();
  }
  render(width) {
    const lines = [];
    const mouseChildren = [];
    for (const child of this.children) {
      const childLines = child.render?.(width) ?? [];
      mouseChildren.push({ component: child, height: childLines.length });
      for (const line of childLines) lines.push(line);
    }
    this.mouseLayout = { width, children: mouseChildren };
    return lines;
  }
}

/** Empty rendered lines (`components/spacer.ts`). */
export class Spacer extends Component {
  constructor(lines = 1) {
    super();
    this.lines = lines;
  }
  setLines(lines) {
    this.lines = lines;
  }
  render(_width) {
    return Array.from({ length: Math.max(0, this.lines) }, () => "");
  }
}

/** Word-wrapped, padded text (`components/text.ts`). */
export class Text extends Component {
  constructor(text = "", paddingX = 1, paddingY = 1, customBgFn) {
    super();
    this.text = text;
    this.paddingX = paddingX;
    this.paddingY = paddingY;
    this.customBgFn = customBgFn;
    this.cachedText = undefined;
    this.cachedWidth = undefined;
    this.cachedLines = undefined;
  }
  setText(text) {
    if (text === this.text) return;
    this.text = text;
    this.invalidate();
  }
  setCustomBgFn(fn) {
    this.customBgFn = fn;
    this.invalidate();
  }
  setPaddingX(paddingX) {
    this.paddingX = paddingX;
    this.invalidate();
  }
  invalidate() {
    this.cachedText = undefined;
    this.cachedWidth = undefined;
    this.cachedLines = undefined;
  }
  render(width) {
    if (this.cachedLines && this.cachedText === this.text && this.cachedWidth === width) {
      return this.cachedLines;
    }
    if (!this.text || !String(this.text).trim()) {
      this.cachedText = this.text;
      this.cachedWidth = width;
      this.cachedLines = [""];
      return this.cachedLines;
    }
    const normalized = String(this.text).replace(/\t/g, "   ");
    const padX = Math.min(this.paddingX, Math.max(0, Math.floor((width - 1) / 2)));
    const contentWidth = Math.max(1, width - padX * 2);
    const words = normalized.split(/\s+/);
    const wrapped = [];
    let line = "";
    for (const word of words) {
      if (!line) line = word;
      else if (visibleWidth(`${line} ${word}`) <= contentWidth) line += ` ${word}`;
      else {
        wrapped.push(line);
        line = word;
      }
    }
    if (line) wrapped.push(line);
    const margin = " ".repeat(padX);
    const contentLines = wrapped.map((row) => {
      const padded = margin + row + margin;
      const output = padded + " ".repeat(Math.max(0, width - visibleWidth(padded)));
      return this.customBgFn ? this.customBgFn(output) : output;
    });
    const empty = " ".repeat(width);
    const emptyLines = Array.from({ length: Math.max(0, this.paddingY) }, () =>
      this.customBgFn ? this.customBgFn(empty) : empty,
    );
    const result = [...emptyLines, ...contentLines, ...emptyLines];
    this.cachedText = this.text;
    this.cachedWidth = width;
    this.cachedLines = result;
    return result;
  }
}

/** A one-line text input (`components/input.ts`). */
export class Input extends Component {
  constructor(options = {}) {
    super();
    this.value = options.initialValue ?? "";
    this.cursorPosition = this.value.length;
    this.focused = false;
    this.capturesViewportInput = true;
    this.options = options;
  }
  getValue() {
    return this.value;
  }
  setValue(value) {
    this.value = String(value ?? "");
    this.cursorPosition = this.value.length;
  }
  handleInput(data) {
    const text = String(data ?? "");
    if (text === "\x7f" || text === "\b") {
      this.value = this.value.slice(0, -1);
    } else if (!text.startsWith("\u001b")) {
      this.value += text;
    }
    this.cursorPosition = this.value.length;
  }
  render(width) {
    const shown = truncateToWidth(this.value, Math.max(1, width - 1), "");
    return [shown + " ".repeat(Math.max(0, width - visibleWidth(shown)))];
  }
}

/** A keyboard-selectable list (`components/select-list.ts`). */
export class SelectList extends Component {
  constructor(items = [], maxVisible = 5, theme = {}, layout = {}) {
    super();
    this.items = items;
    this.filteredItems = items;
    this.selectedIndex = 0;
    this.maxVisible = maxVisible;
    this.theme = theme;
    this.layout = layout;
    this.capturesViewportInput = true;
    this.focused = true;
    this.onSelect = undefined;
    this.onCancel = undefined;
    this.onSelectionChange = undefined;
  }
  setFilter(filter) {
    const needle = String(filter ?? "").toLowerCase();
    this.filteredItems = this.items.filter((item) => String(item.value).toLowerCase().startsWith(needle));
    this.selectedIndex = 0;
  }
  setSelectedIndex(index) {
    this.selectedIndex = Math.max(0, Math.min(index, this.filteredItems.length - 1));
  }
  getSelectedItem() {
    return this.filteredItems[this.selectedIndex];
  }
  notifySelectionChange() {
    const item = this.getSelectedItem();
    if (item) this.onSelectionChange?.(item);
  }
  handleInput(data) {
    const key = String(data ?? "");
    if (key === "\u001b[A" || key === "k") {
      const previous = this.selectedIndex;
      this.selectedIndex = this.selectedIndex === 0 ? this.filteredItems.length - 1 : this.selectedIndex - 1;
      if (this.selectedIndex !== previous) this.notifySelectionChange();
    } else if (key === "\u001b[B" || key === "j") {
      const previous = this.selectedIndex;
      this.selectedIndex = this.selectedIndex === this.filteredItems.length - 1 ? 0 : this.selectedIndex + 1;
      if (this.selectedIndex !== previous) this.notifySelectionChange();
    } else if (key === "\r" || key === "\n") {
      const item = this.getSelectedItem();
      if (item) this.onSelect?.(item);
    } else if (key === "\u001b" || key === "q") {
      this.onCancel?.();
    }
  }
  render(width) {
    if (this.filteredItems.length === 0) return [this.theme.noMatch?.("No matches") ?? "No matches"];
    const count = Math.min(this.maxVisible, this.filteredItems.length);
    const rows = [];
    for (let index = 0; index < count; index++) {
      const item = this.filteredItems[index];
      const selected = index === this.selectedIndex;
      const label = String(item.label ?? item.value);
      const description = item.description ? ` ${item.description}` : "";
      const raw = `${selected ? "> " : "  "}${label}${description}`;
      rows.push(selected ? this.theme.selectedText?.(raw) ?? raw : raw);
    }
    if (this.filteredItems.length > count) {
      rows.push(this.theme.scrollInfo?.(`  (${this.selectedIndex + 1}/${this.filteredItems.length})`) ?? `  (${this.selectedIndex + 1}/${this.filteredItems.length})`);
    }
    return rows.map((row) => truncateToWidth(row, width, ""));
  }
}

/** A bordered box (`components/box.ts`). */
export class Box extends Component {
  constructor(title = "", options = {}) {
    super();
    this.title = title;
    this.options = options;
    this.child = undefined;
  }
  setChild(child) {
    this.child = child;
  }
  addChild(child) {
    this.setChild(child);
  }
  render(width) {
    const top = `┌─${this.title}`.padEnd(Math.max(0, width), "─");
    const body = (this.child?.render?.(Math.max(1, width - 2)) ?? []).map((line) =>
      `│ ${truncateToWidth(line, Math.max(0, width - 2), "")}`,
    );
    return [top, ...body, "└".padEnd(Math.max(0, width), "─")];
  }
}

/** Horizontal stack (`components/h-stack.ts`). */
export class HStack extends Component {
  constructor(children = []) {
    super();
    this.children = children;
  }
  addChild(child) {
    this.children.push(child);
  }
  render(width) {
    const columns = Math.max(1, Math.floor(width / Math.max(1, this.children.length)));
    const rendered = this.children.map((child) => child.render?.(columns) ?? []);
    const height = Math.max(0, ...rendered.map((lines) => lines.length));
    const rows = [];
    for (let index = 0; index < height; index++) {
      rows.push(rendered.map((lines) => truncateToWidth(lines[index] ?? "", columns, "")).join(""));
    }
    return rows;
  }
}

/** Minimal markdown renderer: returns its source rows. */
export class Markdown extends Component {
  constructor(markdown = "", _options = {}) {
    super();
    this.markdown = markdown;
  }
  render(width) {
    return String(this.markdown)
      .split("\n")
      .map((line) => truncateToWidth(line, width, ""));
  }
}

/** A scrollable wrapper. */
export class ScrollView extends Component {
  constructor(child, options = {}) {
    super();
    this.child = child;
    this.options = options;
    this.scroll = 0;
  }
  render(width) {
    return (this.child?.render?.(width) ?? []).slice(this.scroll, this.scroll + (this.options.maxHeight ?? 20));
  }
}

/** A spinner-style loader. */
export class Loader extends Component {
  constructor(options = {}) {
    super();
    this.options = options;
    this.active = false;
  }
  start() {
    this.active = true;
  }
  stop() {
    this.active = false;
  }
  render(_width) {
    return this.active ? [this.options.label ?? "Loading…"] : [];
  }
}

export class CancellableLoader extends Loader {}

/** Terminal capability helpers used by tool renderers. */
export function getCapabilities() {
  return { images: false, color: false, hyperlinks: false, keyboard: false, mouse: false };
}

export function getImageDimensions(_data) {
  return undefined;
}

export function hyperlink(text, _url) {
  return String(text);
}

export function imageFallback(description = "") {
  return `[image: ${description}]`;
}

/** Key identifiers (`keys.ts:163`); only the names extensions reference. */
export const Key = {
  escape: "escape",
  esc: "esc",
  enter: "enter",
  return: "return",
  tab: "tab",
  space: "space",
  backspace: "backspace",
  delete: "delete",
  insert: "insert",
  clear: "clear",
  home: "home",
  end: "end",
  pageUp: "pageUp",
  pageDown: "pageDown",
  up: "up",
  down: "down",
  left: "left",
  right: "right",
  f1: "f1",
  f2: "f2",
  f3: "f3",
  f4: "f4",
  f5: "f5",
  f6: "f6",
  f7: "f7",
  f8: "f8",
  f9: "f9",
  f10: "f10",
  f11: "f11",
  f12: "f12",
  backtick: "`",
  hyphen: "-",
  equals: "=",
  leftbracket: "[",
  rightbracket: "]",
  backslash: "\\",
  semicolon: ";",
  quote: "'",
  comma: ",",
  period: ".",
  slash: "/",
};

// Type-only upstream exports (`Component` is deliberately a runtime class
// here for convenience; `Focusable`, `SelectItem`, `TUI`, `Theme`,
// `SelectListTheme`, `EditorComponent`, …) are erased upstream and absent.

export default {
  Component,
  Container,
  Spacer,
  Text,
  Input,
  SelectList,
  Box,
  HStack,
  Markdown,
  ScrollView,
  Loader,
  CancellableLoader,
  visibleWidth,
  truncateToWidth,
  getCapabilities,
  getImageDimensions,
  hyperlink,
  imageFallback,
  Key,
};
