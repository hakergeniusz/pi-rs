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

// ---- Fuzzy matching (packages/tui/src/fuzzy.ts) ----
function scorePositions(textLower, positions) {
  let score = 0;
  let lastMatchIndex = -1;
  let consecutiveMatches = 0;
  for (const i of positions) {
    if (lastMatchIndex === i - 1) {
      consecutiveMatches++;
      score -= consecutiveMatches * 5;
    } else {
      consecutiveMatches = 0;
      if (lastMatchIndex >= 0) score += (i - lastMatchIndex - 1) * 2;
    }
    if (i === 0 || /[\s\-_./:]/.test(textLower[i - 1])) score -= 10;
    score += i * 0.1;
    lastMatchIndex = i;
  }
  return score;
}

function matchQuery(normalizedQuery, textLower) {
  if (normalizedQuery.length === 0) return { matches: true, score: 0 };
  if (normalizedQuery.length > textLower.length) return { matches: false, score: 0 };
  const positions = [];
  let lastMatchIndex = -1;
  for (let queryIndex = 0; queryIndex < normalizedQuery.length; queryIndex++) {
    const i = textLower.indexOf(normalizedQuery[queryIndex], lastMatchIndex + 1);
    if (i === -1) return { matches: false, score: 0 };
    positions.push(i);
    lastMatchIndex = i;
  }
  let score = scorePositions(textLower, positions);
  for (let start = textLower.indexOf(normalizedQuery); start !== -1; start = textLower.indexOf(normalizedQuery, start + 1)) {
    for (let k = 0; k < positions.length; k++) positions[k] = start + k;
    score = Math.min(score, scorePositions(textLower, positions));
  }
  if (normalizedQuery === textLower) score -= 100;
  return { matches: true, score };
}

export function fuzzyMatch(query, text) {
  const queryLower = String(query ?? "").toLowerCase();
  const textLower = String(text ?? "").toLowerCase();
  const primary = matchQuery(queryLower, textLower);
  if (primary.matches) return primary;
  const alphaNumeric = queryLower.match(/^([a-z]+)([0-9]+)$/);
  const numericAlpha = queryLower.match(/^([0-9]+)([a-z]+)$/);
  const swappedQuery = alphaNumeric
    ? `${alphaNumeric[2]}${alphaNumeric[1]}`
    : numericAlpha
      ? `${numericAlpha[2]}${numericAlpha[1]}`
      : "";
  if (!swappedQuery) return primary;
  const swapped = matchQuery(swappedQuery, textLower);
  return swapped.matches ? { matches: true, score: swapped.score + 5 } : primary;
}

export function fuzzyFilter(items, query, getText) {
  if (!String(query ?? "").trim()) return items;
  const tokens = String(query).trim().split(/[\s/]+/).filter((token) => token.length > 0);
  if (tokens.length === 0) return items;
  const results = [];
  for (const item of items) {
    const text = getText(item);
    let totalScore = 0;
    let allMatch = true;
    for (const token of tokens) {
      const match = fuzzyMatch(token, text);
      if (match.matches) totalScore += match.score;
      else { allMatch = false; break; }
    }
    if (allMatch) results.push({ item, totalScore });
  }
  results.sort((a, b) => a.totalScore - b.totalScore);
  return results.map((entry) => entry.item);
}

// ---- Colors / styling (packages/tui/src/colors.ts) ----
function colorAnsi(color, mode, background) {
  if (!color) return undefined;
  if (typeof color === "string") color = { kind: "rgb", ...hexToRgb(color) };
  if (color.kind === "indexed") return `\u001b[${background ? 48 : 38};5;${color.index}m`;
  const r = Math.round(color.r ?? 0), g = Math.round(color.g ?? 0), b = Math.round(color.b ?? 0);
  if (mode === "truecolor") return `\u001b[${background ? 48 : 38};2;${r};${g};${b}m`;
  const index = 16 + 36 * Math.round(r / 51) + 6 * Math.round(g / 51) + Math.round(b / 51);
  return `\u001b[${background ? 48 : 38};5;${index}m`;
}

function hexToRgb(hex) {
  const value = String(hex).replace(/^#/, "");
  if (/^[0-9a-f]{6}$/i.test(value)) {
    return { r: parseInt(value.slice(0, 2), 16), g: parseInt(value.slice(2, 4), 16), b: parseInt(value.slice(4, 6), 16) };
  }
  return { r: 0, g: 0, b: 0 };
}

export function styleText(text, options = {}, mode = "truecolor") {
  const fg = colorAnsi(options.fg, mode, false);
  const bg = colorAnsi(options.bg, mode, true);
  let prefix = "", suffix = "";
  if (fg) { prefix += fg; suffix = "\u001b[39m"; }
  if (bg) { prefix += bg; suffix = `\u001b[49m${suffix}`; }
  if (options.bold) prefix += "\u001b[1m";
  if (options.dim) prefix += "\u001b[2m";
  if (options.bold || options.dim) suffix = `\u001b[22m${suffix}`;
  if (options.italic) { prefix += "\u001b[3m"; suffix = `\u001b[23m${suffix}`; }
  if (options.underline) { prefix += "\u001b[4m"; suffix = `\u001b[24m${suffix}`; }
  if (options.inverse) { prefix += "\u001b[7m"; suffix = `\u001b[27m${suffix}`; }
  if (options.strikethrough) { prefix += "\u001b[9m"; suffix = `\u001b[29m${suffix}`; }
  return `${prefix}${text}${suffix}`;
}

export function styleTextWithAnsi(text, fgAnsi, bgAnsi, options = {}) {
  let prefix = "", suffix = "";
  if (fgAnsi) { prefix += fgAnsi; suffix = "\u001b[39m"; }
  if (bgAnsi) { prefix += bgAnsi; suffix = `\u001b[49m${suffix}`; }
  if (options.bold) prefix += "\u001b[1m";
  if (options.dim) prefix += "\u001b[2m";
  if (options.bold || options.dim) suffix = `\u001b[22m${suffix}`;
  if (options.italic) { prefix += "\u001b[3m"; suffix = `\u001b[23m${suffix}`; }
  if (options.underline) { prefix += "\u001b[4m"; suffix = `\u001b[24m${suffix}`; }
  if (options.inverse) { prefix += "\u001b[7m"; suffix = `\u001b[27m${suffix}`; }
  if (options.strikethrough) { prefix += "\u001b[9m"; suffix = `\u001b[29m${suffix}`; }
  return `${prefix}${text}${suffix}`;
}

// ---- TruncatedText (packages/tui/src/components/truncated-text.ts) ----
export class TruncatedText extends Component {
  constructor(text = "", paddingX = 0, paddingY = 0) {
    super();
    this.text = text;
    this.paddingX = paddingX;
    this.paddingY = paddingY;
  }
  render(width) {
    const result = [];
    const emptyLine = " ".repeat(width);
    for (let i = 0; i < this.paddingY; i++) result.push(emptyLine);
    const availableWidth = Math.max(1, width - this.paddingX * 2);
    const newlineIndex = String(this.text).indexOf("\n");
    const singleLine = newlineIndex === -1 ? String(this.text) : String(this.text).slice(0, newlineIndex);
    const displayText = truncateToWidth(singleLine, availableWidth);
    const lineWithPadding = " ".repeat(this.paddingX) + displayText + " ".repeat(this.paddingX);
    result.push(lineWithPadding + " ".repeat(Math.max(0, width - visibleWidth(lineWithPadding))));
    for (let i = 0; i < this.paddingY; i++) result.push(emptyLine);
    return result;
  }
}

// ---- SettingsList (packages/tui/src/components/settings-list.ts) ----
export class SettingsList extends Component {
  constructor(items = [], maxVisible = 5, theme = {}, onChange, onCancel, options = {}) {
    super();
    this.capturesViewportInput = true;
    this.items = items;
    this.filteredItems = items;
    this.maxVisible = maxVisible;
    this.theme = theme;
    this.onChange = onChange;
    this.onCancel = onCancel;
    this.selectedIndex = 0;
    this.searchEnabled = options.enableSearch ?? false;
    this.searchInput = this.searchEnabled ? new Input() : undefined;
  }
  updateValue(id, newValue) {
    const item = this.items.find((entry) => entry.id === id);
    if (item) item.currentValue = newValue;
  }
  selectItem(id) {
    const items = this.searchEnabled ? this.filteredItems : this.items;
    const index = items.findIndex((entry) => entry.id === id);
    if (index !== -1) this.selectedIndex = index;
  }
  invalidate() {}
  render(width) {
    const lines = [];
    if (this.searchEnabled && this.searchInput) {
      lines.push(...this.searchInput.render(width), "");
      const filter = this.searchInput.getValue();
      this.filteredItems = filter
        ? fuzzyFilter(this.items, filter, (item) => `${item.label} ${item.currentValue} ${item.description ?? ""}`)
        : this.items;
    }
    if (this.filteredItems.length === 0) {
      lines.push(this.theme.hint ? this.theme.hint("  No settings available") : "  No settings available");
      return lines;
    }
    const count = Math.min(this.maxVisible, this.filteredItems.length);
    for (let index = 0; index < count; index++) {
      const item = this.filteredItems[index];
      const selected = index === this.selectedIndex;
      const cursor = selected ? this.theme.cursor ?? "> " : "  ";
      const label = this.theme.label ? this.theme.label(item.label, selected) : item.label;
      const value = this.theme.value ? this.theme.value(item.currentValue, selected) : item.currentValue;
      const row = `${cursor}${label}  ${value}`;
      lines.push(truncateToWidth(row, width, ""));
    }
    if (this.filteredItems.length > count) {
      lines.push(this.theme.scrollInfo?.(`  (${this.selectedIndex + 1}/${this.filteredItems.length})`) ?? `  (${this.selectedIndex + 1}/${this.filteredItems.length})`);
    }
    const selected = this.filteredItems[this.selectedIndex];
    if (selected?.description && this.theme.description) {
      lines.push(this.theme.description(selected.description));
    }
    return lines;
  }
  handleInput(data) {
    const key = String(data ?? "");
    if (this.searchEnabled && this.searchInput && key !== "\r" && key !== "\n" && !key.startsWith("\u001b[")) {
      this.searchInput.handleInput(data);
      this.selectedIndex = 0;
      return;
    }
    if (key === "\u001b[A" || key === "k") {
      this.selectedIndex = this.selectedIndex === 0 ? this.filteredItems.length - 1 : this.selectedIndex - 1;
    } else if (key === "\u001b[B" || key === "j") {
      this.selectedIndex = this.selectedIndex === this.filteredItems.length - 1 ? 0 : this.selectedIndex + 1;
    } else if (key === "\r" || key === "\n" || key === " ") {
      const item = this.filteredItems[this.selectedIndex];
      if (item?.values?.length) {
        const next = item.values[(item.values.indexOf(item.currentValue) + 1) % item.values.length];
        this.onChange?.(item.id, next);
      }
    } else if (key === "\u001b" || key === "q") {
      this.onCancel?.();
    }
  }
}

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
  fuzzyMatch,
  fuzzyFilter,
  styleText,
  styleTextWithAnsi,
  TruncatedText,
  SettingsList,
};
