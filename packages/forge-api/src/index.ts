// Public API for Forge extensions.
//
//   import { forge, View, Text, Button } from '@forge/api';
//   export function activate(ctx: ExtensionContext) {
//     forge.panels.register({ id: 'hello', title: 'Hello', render: () => <Text>Hi</Text> });
//   }
//
// UI is plain React, but host components render as native GPUI elements (no DOM, no CSS).

import { createElement } from 'react';
import type { ReactNode } from 'react';
import { native } from './native';
import { renderPanel, unmountPanel } from './reconciler';

// ---------------------------------------------------------------- styling

/** Semantic colours resolved against the active Zed theme. */
export type Color = 'default' | 'muted' | 'accent' | 'error' | 'warning' | 'success' | 'surface' | 'elevated' | 'panel' | 'editor' | 'transparent';

export type Style = {
  direction?: 'row' | 'column';
  gap?: number;
  padding?: number;
  paddingX?: number;
  paddingY?: number;
  align?: 'start' | 'center' | 'end' | 'stretch';
  justify?: 'start' | 'center' | 'end' | 'between';
  /** Takes the space left in its parent (and may shrink below its content). */
  grow?: boolean;
  /** `false` keeps its size when the parent is short of space. */
  shrink?: boolean;
  wrap?: boolean;
  width?: number | 'full';
  height?: number | 'full';
  minWidth?: number;
  maxWidth?: number;
  background?: Color;
  color?: Color;
  border?: boolean;
  /** A border on one side only. */
  borderSide?: 'top' | 'bottom' | 'left' | 'right';
  rounded?: boolean;
  size?: 'xs' | 'sm' | 'md' | 'lg';
  weight?: 'normal' | 'medium' | 'bold';
  mono?: boolean;
  italic?: boolean;
  /** Cuts long text with an ellipsis. */
  truncate?: boolean;
};

/** An entry of a context menu; choosing it calls `onContextMenu` with its `id`. */
export type MenuItem =
  | { id: string; label: string; icon?: string; disabled?: boolean; danger?: boolean }
  | { separator: true }
  | { header: string };

type Base = {
  style?: Style;
  children?: ReactNode;
  key?: string | number;
  /** Shown on right click. */
  contextMenu?: MenuItem[];
  onContextMenu?: (event: { id: string }) => void;
};

// ---------------------------------------------------------------- host components

export type ViewProps = Base & { onClick?: () => void; tooltip?: string; hidden?: boolean };
export type TextProps = Base & { selectable?: boolean };
export type ButtonProps = Base & {
  label?: string;
  icon?: string;
  variant?: 'filled' | 'subtle' | 'ghost';
  disabled?: boolean;
  /** Shown pressed (e.g. the active tab or a toggled option). */
  selected?: boolean;
  tooltip?: string;
  onClick?: () => void;
};
export type InputProps = Base & {
  value: string;
  placeholder?: string;
  /** Several lines (Enter adds a line; ⌘Enter submits). */
  multiline?: boolean;
  /** Hides what is typed. */
  password?: boolean;
  /** Takes the keyboard focus when it appears. */
  autoFocus?: boolean;
  onChange?: (value: string) => void;
  /** Enter in a single-line input, ⌘Enter in a multi-line one. */
  onSubmit?: (value: string) => void;
};
export type CheckboxProps = Base & { checked: boolean; label?: string; onChange?: (checked: boolean) => void };
export type IconProps = Base & { name: string };
export type DividerProps = { direction?: 'horizontal' | 'vertical' };

export type SelectOption<T extends string | number = string> = { value: T; label: string };
export type SelectProps<T extends string | number = string> = Base & {
  value: T | null;
  options: SelectOption<T>[];
  placeholder?: string;
  disabled?: boolean;
  onChange?: (value: T) => void;
};

/**
 * One row of a tree. The tree itself is yours: render the visible rows in order, with
 * their `depth`, and expand or collapse them in `onToggle`.
 */
export type TreeItemProps = Base & {
  label: string;
  /** Muted text after the label. */
  description?: string;
  icon?: string;
  iconColor?: Color;
  depth?: number;
  /** `true`/`false` shows a disclosure arrow; leave it out for leaves. */
  expanded?: boolean;
  selected?: boolean;
  /** Shows a spinner instead of the icon. */
  loading?: boolean;
  onToggle?: () => void;
  onClick?: () => void;
  onDoubleClick?: () => void;
};

/** A cell value; `null` shows as NULL. */
export type Cell = string | number | boolean | null;
export type GridColumn = { name: string; type?: string; width?: number; primaryKey?: boolean };
export type GridRowState = 'new' | 'modified' | 'deleted';

export type DataGridProps = Omit<Base, 'onContextMenu'> & {
  columns: GridColumn[];
  rows: Cell[][];
  /** Number of the first row (rows are numbered from `rowOffset + 1`). */
  rowOffset?: number;
  selectedRows?: number[];
  rowStates?: Record<number, GridRowState>;
  /** Cells changed and not saved yet, as `"row:column"`. */
  editedCells?: string[];
  /** Double-clicking a cell edits it (else it calls `onRowActivate`). */
  editable?: boolean;
  sort?: { column: string; desc?: boolean } | null;
  emptyText?: string;
  onSelect?: (rows: number[]) => void;
  onCellEdit?: (edit: { row: number; column: number; value: string }) => void;
  onSort?: (column: string, index: number) => void;
  /** Delete or Backspace with rows selected. */
  onDeleteRows?: (rows: number[]) => void;
  onRowActivate?: (row: number, column: number) => void;
  onContextMenu?: (event: { id: string; row: number; column: number; rows: number[] }) => void;
};

// Intrinsic element names understood by crates/forge-extension-host/src/surface.rs.
export const View = (p: ViewProps) => createElement('view', p);
export const Scroll = (p: ViewProps) => createElement('scroll', p);
export const Text = (p: TextProps) => createElement('text', p);
export const Button = (p: ButtonProps) => createElement('button', p);
export const Input = (p: InputProps) => createElement('input', p);
export const Checkbox = (p: CheckboxProps) => createElement('checkbox', p);
export const Icon = (p: IconProps) => createElement('icon', p);
export const Divider = (p: DividerProps = {}) => createElement('divider', p);
/** A spinning progress indicator. */
export const Spinner = (p: { style?: Style } = {}) => createElement('spinner', p);
/** A drop-down list of options. */
export function Select<T extends string | number = string>(p: SelectProps<T>) {
  return createElement('select', p);
}
export const TreeItem = (p: TreeItemProps) => createElement('treeItem', p);

/**
 * A virtualized table of rows (thousands are fine): resizable columns, a row-number column,
 * selection (click, ⌘-click, shift-click), ⌘C to copy the selected rows, and in-place
 * editing with `editable`. Give it room with `style: { grow: true }` or a height.
 */
export function DataGrid(p: DataGridProps) {
  const { onSelect, onSort, onDeleteRows, onRowActivate, ...rest } = p;
  return createElement('grid', {
    ...rest,
    onSelect: onSelect && ((e: { rows: number[] }) => onSelect(e.rows)),
    onSort: onSort && ((e: { column: string; index: number }) => onSort(e.column, e.index)),
    onDeleteRows: onDeleteRows && ((e: { rows: number[] }) => onDeleteRows(e.rows)),
    onRowActivate: onRowActivate && ((e: { row: number; column: number }) => onRowActivate(e.row, e.column)),
  });
}

export type TabItem = { id: string; label: string; icon?: string; closable?: boolean };

/** A row of tabs (the content below is yours to switch). */
export function Tabs(p: { tabs: TabItem[]; active: string | null; onSelect: (id: string) => void; onClose?: (id: string) => void; style?: Style }) {
  return createElement(
    'view',
    { style: { direction: 'row', gap: 2, paddingX: 4, paddingY: 2, borderSide: 'bottom', ...p.style } },
    ...p.tabs.map((t) =>
      createElement(
        'view',
        { key: t.id, style: { direction: 'row', gap: 0 } },
        createElement('button', { label: t.label, icon: t.icon, selected: t.id === p.active, variant: 'ghost', onClick: () => p.onSelect(t.id) }),
        t.closable && p.onClose ? createElement('button', { icon: 'close', variant: 'ghost', tooltip: 'Close', onClick: () => p.onClose!(t.id) }) : null,
      ),
    ),
  );
}

// ---------------------------------------------------------------- host calls

let nextCall = 1;
const pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();

function call<T>(method: string, args: unknown = {}): Promise<T> {
  const id = nextCall++;
  return new Promise<T>((resolve, reject) => {
    pending.set(id, { resolve, reject });
    native().call(method, JSON.stringify(args), id);
  });
}

function notify(method: string, args: unknown = {}) {
  native().call(method, JSON.stringify(args), 0);
}

/** Called by the host to settle a `call`. */
export function resolveCall(id: number, ok: boolean, json: string) {
  const p = pending.get(id);
  if (!p) return;
  pending.delete(id);
  const value = json ? JSON.parse(json) : undefined;
  ok ? p.resolve(value) : p.reject(new Error(typeof value === 'string' ? value : JSON.stringify(value)));
}

// ---------------------------------------------------------------- API

export type Disposable = { dispose(): void };

/** JSON values an extension keeps between sessions (`ctx.storage`, `ctx.workspaceStorage`). */
export type Storage = {
  get<T = unknown>(key: string): Promise<T | null>;
  /** Stores `value` under `key`; `null` or `undefined` removes it. */
  set(key: string, value: unknown): Promise<void>;
  keys(): Promise<string[]>;
};

/** Strings kept in the system keychain (passwords, tokens), per extension. */
export type Secrets = {
  get(key: string): Promise<string | null>;
  /** Stores `value` under `key`; `null` removes it. */
  set(key: string, value: string | null): Promise<void>;
  delete(key: string): Promise<void>;
};

export type ExtensionContext = {
  id: string;
  path: string;
  subscriptions: Disposable[];
  /** Kept for this extension across projects. */
  storage: Storage;
  /** Kept for this extension in the current project. */
  workspaceStorage: Storage;
  /** Kept in the system keychain. */
  secrets: Secrets;
};

function storage(extension: string, scope: 'global' | 'workspace'): Storage {
  return {
    get: <T>(key: string) => call<T | null>('storage.get', { extension, scope, key }),
    set: (key: string, value: unknown) => call<void>('storage.set', { extension, scope, key, value: value ?? null }),
    keys: () => call<string[]>('storage.keys', { extension, scope }),
  };
}

function secrets(extension: string): Secrets {
  return {
    get: (key: string) => call<string | null>('secrets.get', { extension, key }),
    set: (key: string, value: string | null) => call<void>('secrets.set', { extension, key, value }),
    delete: (key: string) => call<void>('secrets.delete', { extension, key }),
  };
}

/** The context passed to an extension's `activate` (called by the runtime). */
export function createContext(id: string, path: string): ExtensionContext {
  return { id, path, subscriptions: [], storage: storage(id, 'global'), workspaceStorage: storage(id, 'workspace'), secrets: secrets(id) };
}

// ---------------------------------------------------------------- events

const eventListeners = new Map<string, Set<(value: any) => void>>();

/** Listens to a host event; the host only sends the ones someone listens to. */
function on<T>(name: string, listener: (value: T) => void): Disposable {
  let listeners = eventListeners.get(name);
  if (!listeners) {
    listeners = new Set();
    eventListeners.set(name, listeners);
    notify('events.listen', { name });
  }
  listeners.add(listener);
  return { dispose: () => listeners!.delete(listener) };
}

/** Called by the host when something an extension listens to happens. */
export function deliverEvent(name: string, json: string) {
  const value = json ? JSON.parse(json) : null;
  if (name === 'process.output') return deliverProcessOutput(value);
  if (name === 'process.exit') return deliverProcessExit(value);
  if (name === 'tabs.closed') {
    unmountPanel(value.id);
    const listener = tabCloseListeners.get(value.id);
    tabCloseListeners.delete(value.id);
    try {
      listener?.();
    } catch (e) {
      console.error(`tab ${value.id} onClose:`, e);
    }
    return;
  }
  eventListeners.get(name)?.forEach((l) => {
    try {
      l(value);
    } catch (e) {
      console.error(`${name} listener:`, e);
    }
  });
}

// ---------------------------------------------------------------- editor

/** A place in a file: zero-based line, and column in characters. */
export type Position = { line: number; column: number };
export type Range = { start: Position; end: Position };
export type Selection = Range & { reversed: boolean };

/** The editor of the active file. */
export type EditorState = {
  path: string | null;
  /** Zed's language name, e.g. "Rust", "TypeScript", "C#". */
  language: string | null;
  lineCount: number;
  selections: Selection[];
  /** Text of the newest selection ("" for a cursor). */
  selectedText: string;
};

export type TextEdit = { range: Range; text: string };

export type DecorationColor = 'accent' | 'error' | 'warning' | 'success' | 'muted';

export type ExecOptions = {
  args?: string[];
  /** Defaults to the first project folder; relative paths resolve against it. */
  cwd?: string;
  /** Added to the project's shell environment. */
  env?: Record<string, string>;
  /** Written to the process's standard input. */
  stdin?: string;
};

export type ExecResult = { code: number | null; stdout: string; stderr: string };

export type TerminalOptions = { args?: string[]; cwd?: string; title?: string };

export type SpawnOptions = {
  args?: string[];
  /** Defaults to the first project folder; relative paths resolve against it. */
  cwd?: string;
  /** Added to the environment (the project's shell environment when a project is open). */
  env?: Record<string, string>;
};

/** A running program started with `forge.process.spawn` or `forge.process.sidecar`. */
export type ChildProcess = {
  id: number;
  /** Writes text to its standard input. */
  write(text: string): void;
  /** Closes its standard input. */
  end(): void;
  kill(): void;
  /** Output as it arrives, in chunks. */
  onStdout(listener: (text: string) => void): Disposable;
  onStderr(listener: (text: string) => void): Disposable;
  /** Standard output, one line at a time (without the newline). */
  onLine(listener: (line: string) => void): Disposable;
  onExit(listener: (code: number | null) => void): Disposable;
  /** Resolves with the exit code when it ends. */
  exited: Promise<number | null>;
};

type ProcessState = {
  stdout: Set<(t: string) => void>;
  stderr: Set<(t: string) => void>;
  exit: Set<(c: number | null) => void>;
  /** Output that arrived before anyone listened. */
  early: { stream: string; data: string }[];
  resolveExit: (code: number | null) => void;
};
const processes = new Map<number, ProcessState>();

function deliverProcessOutput(e: { id: number; stream: string; data: string }) {
  const p = processes.get(e.id);
  if (!p) return;
  const listeners = e.stream === 'stderr' ? p.stderr : p.stdout;
  if (listeners.size === 0 && p.early.length < 10000) p.early.push(e);
  listeners.forEach((l) => l(e.data));
}

function deliverProcessExit(e: { id: number; code: number | null }) {
  const p = processes.get(e.id);
  if (!p) return;
  processes.delete(e.id);
  p.exit.forEach((l) => l(e.code));
  p.resolveExit(e.code);
}

function childProcess(id: number): ChildProcess {
  let resolveExit!: (code: number | null) => void;
  const exited = new Promise<number | null>((r) => (resolveExit = r));
  const state: ProcessState = { stdout: new Set(), stderr: new Set(), exit: new Set(), early: [], resolveExit };
  processes.set(id, state);
  const listen = (set: Set<(t: string) => void>, stream: string, l: (t: string) => void): Disposable => {
    set.add(l);
    const early = state.early.filter((e) => e.stream === stream);
    state.early = state.early.filter((e) => e.stream !== stream);
    early.forEach((e) => l(e.data));
    return { dispose: () => set.delete(l) };
  };
  return {
    id,
    write: (text) => notify('process.write', { id, data: text }),
    end: () => notify('process.closeStdin', { id }),
    kill: () => notify('process.kill', { id }),
    onStdout: (l) => listen(state.stdout, 'stdout', l),
    onStderr: (l) => listen(state.stderr, 'stderr', l),
    onLine(l) {
      let buffered = '';
      return listen(state.stdout, 'stdout', (text) => {
        buffered += text;
        let nl: number;
        while ((nl = buffered.indexOf('\n')) >= 0) {
          const line = buffered.slice(0, nl).replace(/\r$/, '');
          buffered = buffered.slice(nl + 1);
          l(line);
        }
      });
    },
    onExit(l) {
      state.exit.add(l);
      return { dispose: () => state.exit.delete(l) };
    },
    exited,
  };
}

export type ConfirmOptions = { detail?: string; buttons?: string[]; level?: 'info' | 'warning' | 'danger' };
export type PickFilesOptions = { files?: boolean; directories?: boolean; multiple?: boolean; prompt?: string };
export type SaveFileOptions = { directory?: string; name?: string };

export type TabOptions = {
  id: string;
  title: string;
  /** Zed icon name. */
  icon?: string;
  render: () => ReactNode;
  /** When the user closes the tab. */
  onClose?: () => void;
};

/** An extension tab in the editor area. */
export type Tab = Disposable & {
  id: string;
  setTitle(title: string): void;
  update(changes: { title?: string; icon?: string; tooltip?: string }): void;
  close(): void;
};

const tabCloseListeners = new Map<string, () => void>();

/** Path of the extension currently being activated (set by the runtime). */
let activating: string | null = null;
export function setActivating(path: string | null) {
  activating = path;
}

export type WebviewOptions = {
  id: string;
  title: string;
  icon?: string;
  /** HTML entry, relative to the extension folder (e.g. "webview/index.html"). */
  html: string;
  /** Extension folder; defaults to the extension being activated. */
  root?: string;
};

export type WebviewPanel = Disposable & {
  /** Sends a JSON-serialisable message to the page (`window.forge.onMessage`). */
  postMessage(message: unknown): void;
  /** Messages the page sent with `window.forge.postMessage`. */
  onMessage(listener: (message: unknown) => void): Disposable;
};

const webviewListeners = new Map<string, Set<(message: unknown) => void>>();

/** Called by the host when a page posts a message. */
export function deliverWebviewMessage(id: string, json: string) {
  const message = json ? JSON.parse(json) : undefined;
  webviewListeners.get(id)?.forEach((l) => l(message));
}

export type PanelOptions = {
  id: string;
  title: string;
  /** Zed icon name, e.g. "sparkle", "code", "file_tree". */
  icon?: string;
  position?: 'left' | 'right' | 'bottom';
  /**
   * `scroll` (the default): the panel scrolls its content, with some padding. `fill`: the
   * content fills the panel, for layouts that scroll parts of it (a tree, a grid).
   */
  layout?: 'scroll' | 'fill';
  render: () => ReactNode;
};

const settingListeners = new Set<(key: string, value: unknown) => void>();

/** Called by the host when a setting changes in the Settings tab. */
export function deliverSettingChanged(key: string, json: string) {
  const value = json ? JSON.parse(json) : null;
  settingListeners.forEach((l) => l(key, value));
}

const commands = new Map<string, () => unknown>();

/** Called by the host when the user runs an extension command. */
export function runCommand(id: string) {
  const c = commands.get(id);
  if (!c) throw new Error(`unknown command ${id}`);
  return c();
}

/** What each extension registered (panels, commands, listeners), undone when it unloads. */
const owned = new Map<string, Disposable[]>();

/** Undoes everything `extension` registered through its `forge` (called by the runtime). */
export function disposeOwned(extension: string) {
  for (const d of owned.get(extension)?.splice(0) ?? []) {
    try {
      d.dispose();
    } catch (e) {
      console.error(`unloading ${extension}:`, e);
    }
  }
  owned.delete(extension);
}

/**
 * The API as one extension sees it: what it registers is remembered, so unloading the
 * extension (to reload or uninstall it) takes it all away. `extension` is null for code
 * outside any extension.
 */
export function forgeFor(extension: string | null, root: string | null = null) {
  const own = <D extends Disposable>(d: D): D => {
    if (extension) {
      let list = owned.get(extension);
      if (!list) owned.set(extension, (list = []));
      list.push(d);
    }
    return d;
  };
  return {
  panels: {
    register(opts: PanelOptions): Disposable {
      notify('panels.register', { id: opts.id, title: opts.title, icon: opts.icon ?? 'sparkle', position: opts.position ?? 'right', layout: opts.layout ?? 'scroll', extension });
      renderPanel(opts.id, createElement(opts.render));
      return own({
        dispose: () => {
          unmountPanel(opts.id);
          notify('panels.unregister', { id: opts.id });
        },
      });
    },
  },
  webviews: {
    /**
     * A panel rendered by the system web view (WebKit on macOS) instead of native GPUI
     * elements: use it for UIs that need the DOM (charts, rich editors, existing web apps).
     * Theme colours are available as CSS variables (--forge-bg, --forge-fg, --forge-muted,
     * --forge-accent, --forge-border, --forge-surface).
     */
    register(opts: WebviewOptions): WebviewPanel {
      const folder = opts.root ?? root ?? activating;
      if (!folder) throw new Error('forge.webviews.register: call it during activate() or pass `root`');
      notify('webviews.register', { id: opts.id, title: opts.title, icon: opts.icon ?? 'globe', html: opts.html, root: folder, extension });
      const listeners = new Set<(message: unknown) => void>();
      webviewListeners.set(opts.id, listeners);
      return own({
        postMessage: (message: unknown) => notify('webviews.postMessage', { id: opts.id, message }),
        onMessage: (listener) => {
          listeners.add(listener);
          return { dispose: () => listeners.delete(listener) };
        },
        dispose: () => {
          webviewListeners.delete(opts.id);
          notify('panels.unregister', { id: opts.id });
        },
      });
    },
  },
  tabs: {
    /**
     * Opens a tab in the editor area that renders `render()` (filling the tab), or shows it
     * if one with this id is open. Ids are shared by all extensions: prefix yours.
     */
    open(opts: TabOptions): Tab {
      renderPanel(opts.id, createElement(opts.render));
      if (opts.onClose) tabCloseListeners.set(opts.id, opts.onClose);
      notify('tabs.open', { id: opts.id, title: opts.title, icon: opts.icon ?? 'sparkle' });
      const tab = own({
        id: opts.id,
        setTitle: (title: string) => notify('tabs.update', { id: opts.id, title }),
        update: (changes: { title?: string; icon?: string; tooltip?: string }) => notify('tabs.update', { id: opts.id, ...changes }),
        close: () => notify('tabs.close', { id: opts.id }),
        dispose: () => {
          tabCloseListeners.delete(opts.id);
          notify('tabs.close', { id: opts.id });
        },
      });
      return tab;
    },
  },
  commands: {
    register(id: string, title: string, handler: () => unknown): Disposable {
      commands.set(id, handler);
      notify('commands.register', { id, title, extension });
      return own({ dispose: () => { commands.delete(id); notify('commands.unregister', { id }); } });
    },
  },
  workspace: {
    /** Absolute paths of the project's root folders. */
    roots: () => call<string[]>('workspace.roots'),
    /** Reads a file; returns the editor buffer contents if it is open with unsaved changes. */
    readFile: (path: string) => call<string>('workspace.readFile', { path }),
    openFile: (path: string, line?: number) => call<void>('workspace.openFile', { path, line }),
    /** Path of the file in the active editor, if any. */
    activeFile: () => call<string | null>('workspace.activeFile'),
    /** When another file (or none: `null`) becomes the active one. */
    onDidChangeActiveFile: (listener: (path: string | null) => void) => own(on('workspace.activeFileChanged', listener)),
    /** When a file is saved, from any editor. */
    onDidSaveFile: (listener: (path: string) => void) => own(on('workspace.fileSaved', listener)),
  },
  editor: {
    /** The editor of the active file, or `null` when the active tab isn't one. */
    active: () => call<EditorState | null>('editor.state'),
    /** The active file's text, with unsaved changes. */
    getText: () => call<string | null>('editor.getText'),
    /** Applies edits to the active file (one undo step). Ranges refer to the text before the edits. */
    edit: (edits: TextEdit[]) => call<boolean>('editor.edit', { edits }),
    /** Replaces every selection (or inserts at every cursor). */
    replaceSelections: (text: string) => call<void>('editor.replaceSelections', { text }),
    /** Selects `ranges` (a cursor when start = end) and scrolls to the first. */
    select: (ranges: Range | Range[]) => call<void>('editor.select', { ranges: Array.isArray(ranges) ? ranges : [ranges] }),
    /**
     * Highlights `ranges` in a file (the active one unless `path` is given), replacing what
     * this `key` highlighted there before; an empty list clears them. They show in every
     * editor of the file, including ones opened later.
     */
    setDecorations: (key: string, ranges: Range[], color: DecorationColor = 'accent', path?: string) => call<void>('editor.setDecorations', { key, ranges, color, path }),
    /** When the selections of the active editor change, or another editor becomes active. */
    onDidChangeSelection: (listener: (state: EditorState | null) => void) => own(on('editor.selectionChanged', listener)),
  },
  process: {
    /** Runs a program to completion with the project's shell environment (`PATH` included). */
    exec: (command: string, options: ExecOptions = {}) => call<ExecResult>('process.exec', { command, ...options }),
    /** Starts a program that keeps running; talk to it through its input and output. It is killed when the extension unloads. */
    spawn: (command: string, options: SpawnOptions = {}) => call<number>('process.spawn', { command, extension, ...options }).then(childProcess),
    /**
     * Starts one of the extension's own programs: `bin/<platform>/<name>` in its folder
     * (e.g. `bin/darwin-arm64/name`), declared in package.json as `forge.sidecars`.
     */
    sidecar: (name: string, options: SpawnOptions = {}) => call<number>('process.spawn', { sidecar: name, extension, ...options }).then(childProcess),
  },
  terminal: {
    /** Runs a command in a new terminal tab, like a task. */
    run: (command: string, options: TerminalOptions = {}) => call<void>('terminal.run', { command, ...options }),
  },
  settings: {
    /**
     * A setting declared in the extension's package.json (`forge.settings.properties`):
     * the user's value from the Settings tab, else the declared default (null if neither).
     */
    get: <T = unknown>(key: string) => call<T>('settings.get', { key }),
    /** Called when the user changes a setting in the Settings tab (`null` once reset). */
    onDidChange(listener: (key: string, value: unknown) => void): Disposable {
      settingListeners.add(listener);
      return own({ dispose: () => settingListeners.delete(listener) });
    },
  },
  window: {
    showMessage: (message: string, level: 'info' | 'warning' | 'error' = 'info') => notify('window.showMessage', { message, level }),
    /** A native alert; resolves with the index of the button chosen (`buttons` defaults to OK, Cancel). */
    confirm: (message: string, options: ConfirmOptions = {}) => call<number | null>('window.confirm', { message, ...options }),
    /** The native open dialog; null when cancelled. */
    pickFiles: (options: PickFilesOptions = {}) => call<string[] | null>('window.pickFiles', options),
    /** The native save dialog; null when cancelled. */
    saveFile: (options: SaveFileOptions = {}) => call<string | null>('window.saveFile', options),
  },
  clipboard: {
    writeText: (text: string) => call<void>('clipboard.writeText', { text }),
    readText: () => call<string | null>('clipboard.readText'),
  },
  log: (...args: unknown[]) => native().log('info', args.map(String).join(' ')),
  };
}

export type Forge = ReturnType<typeof forgeFor>;

/** The API outside any extension (each extension gets its own, see `forgeFor`). */
export const forge: Forge = forgeFor(null);
