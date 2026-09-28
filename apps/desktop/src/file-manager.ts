// The file manager window (design doc §9.2, §18; ADR 0075, ADR 0076,
// ADR 0124).
//
// Two panes, the way every remote-desktop tool that people already know lays
// it out: this computer on the left, the remote one on the right, a transfer
// list along the bottom. Files move by dragging from one pane to the other,
// by Ctrl+C in one and Ctrl+V in the other, by the Upload and Download
// buttons, or by dragging them in from the desktop; each pane can make a
// folder, rename and delete.
//
// Nothing here decides anything and nothing here touches a filesystem. Every
// listing, change and transfer crosses the IPC boundary as a typed command
// the Rust side authorizes, and the host re-reads `file_browse` and
// `file_transfer` on every request that reaches it (§2.3). The local pane's
// changes run through the same plan the host runs for the remote one, so the
// two sides refuse the same paths.

import { html, nothing, type TemplateResult } from 'lit-html';

import { baseName, TransferQueue, type Job, type QueueItem } from './file-queue';
import { formatSize, type FileCommands, type FileTransfers, type TransferRow } from './file-transfers';
import { t, type Locale, type TranslationKey } from './i18n';

/** One entry of either pane, exactly as `DirEntryDto` carries it. */
export interface FileEntry {
  name: string;
  size: number;
  is_dir: boolean;
  modified_unix: number;
}

/** Why a listing carries no entries (§18; ADR 0075). */
export type DirRefusal = 'not_granted' | 'bad_path' | 'unreadable';

/** Why the host will not send a file this window asked for (ADR 0076). */
export type FetchRefusal = DirRefusal | 'too_many';

/** Why a file operation did not happen, on either side (ADR 0124). */
export type FileOpRefusal = 'not_granted' | 'bad_path' | 'exists' | 'not_found' | 'busy' | 'failed';

/** One change to a file or folder, as `FileOpDto` takes it. */
export type FileOp =
  | { kind: 'make_dir'; path: string }
  | { kind: 'rename'; path: string; name: string }
  | { kind: 'delete'; path: string };

/** One directory of this machine, as `local_dir_list` returns it. */
export interface LocalDir {
  path: string;
  parent: string | null;
  entries: FileEntry[];
  truncated: boolean;
}

/** The remote pane's whole state, as `remote_dir_status` returns it. */
export interface RemoteDir {
  path: string;
  parent: string | null;
  entries: FileEntry[];
  truncated: boolean;
  refused: DirRefusal | null;
  fetch_refused: FetchRefusal | null;
  answered: boolean;
  fetches_refused: number;
  uploads_refused: number;
  op_results: { id: number; refused: FileOpRefusal | null }[];
  listings_answered: number;
}

/** How the window talks to Tauri; injectable so the logic is testable. */
export interface FileManagerCommands {
  localList(path?: string): Promise<LocalDir>;
  localRoots(): Promise<string[]>;
  /** Rejects with an `IpcError` whose code names the refusal. */
  localOp(op: FileOp): Promise<void>;
  remoteList(path: string): Promise<void>;
  remoteStatus(): Promise<RemoteDir>;
  /** The id the answer will carry in `RemoteDir.op_results`. */
  remoteOp(op: FileOp): Promise<number>;
  download(path: string, into: string): Promise<void>;
  upload(localPath: string, remoteDir: string): Promise<void>;
}

/** Default binding to the real IPC surface, for one peer. */
export function tauriFileManagerCommands(peer: string): FileManagerCommands {
  const call = async (cmd: string, args: Record<string, unknown>): Promise<unknown> => {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke(cmd, args);
  };
  return {
    async localList(path) {
      return (await call('local_dir_list', { args: { peer, path: path ?? null } })) as LocalDir;
    },
    async localRoots() {
      return (await call('local_roots', { peer })) as string[];
    },
    async localOp(op) {
      await call('local_file_op', { args: { peer, op } });
    },
    async remoteList(path) {
      await call('remote_dir_list', { args: { peer, path } });
    },
    async remoteStatus() {
      return (await call('remote_dir_status', { peer })) as RemoteDir;
    },
    async remoteOp(op) {
      return (await call('remote_file_op', { args: { peer, op } })) as number;
    },
    async download(path, into) {
      await call('remote_download', { args: { peer, path, into } });
    },
    async upload(localPath, remoteDir) {
      await call('remote_upload', { args: { peer, local_path: localPath, remote_dir: remoteDir } });
    },
  };
}

/**
 * The two paths a host's disk can start at, tried in this order.
 *
 * The protocol carries no "where does this host keep its files" message and
 * deliberately gains none for this: a listing names an absolute path, and
 * which one exists is exactly what a probe answers. The Windows root goes
 * first because a Windows host also answers `/` — with the root of its
 * current drive, under a path that then reads like a Unix one — while a Unix
 * host refuses `C:\` as a directory it does not have.
 */
export const REMOTE_ROOTS = ['C:\\', '/'] as const;

/** How long a remote listing may take before the pane stops waiting for it. */
export const REMOTE_LIST_TIMEOUT_MS = 15_000;

/** How long a message at the bottom of the window stays up. */
export const NOTICE_MS = 5_000;

/** Which pane. */
export type Side = 'local' | 'remote';

/** Which column a pane is sorted by. */
export type SortKey = 'name' | 'size' | 'modified';

/** Everything one pane shows. */
export interface Pane {
  side: Side;
  /** The directory on screen; empty before the first listing. */
  path: string;
  parent: string | null;
  entries: FileEntry[];
  truncated: boolean;
  loading: boolean;
  /** Why the pane shows no listing, when it does not. */
  problem: TranslationKey | null;
  /** Selected entries, by name. */
  selected: Set<string>;
  /** Where a Shift range starts. */
  anchor: string | null;
  /** The row the keyboard is on. */
  cursor: string | null;
  sort: SortKey;
  ascending: boolean;
  /** Directories visited before this one, for Back. */
  back: string[];
  /** A row being renamed, or a new folder being named. */
  editing: { kind: 'rename'; name: string } | { kind: 'create' } | null;
}

function emptyPane(side: Side): Pane {
  return {
    side,
    path: '',
    parent: null,
    entries: [],
    truncated: false,
    loading: true,
    problem: null,
    selected: new Set(),
    anchor: null,
    cursor: null,
    sort: 'name',
    ascending: true,
    back: [],
    editing: null,
  };
}

/** Joins a directory and a name the way the side that owns the path would. */
export function joinPath(directory: string, name: string): string {
  const separator = directory.includes('\\') && !directory.includes('/') ? '\\' : '/';
  return directory.endsWith(separator) ? `${directory}${name}` : `${directory}${separator}${name}`;
}

/**
 * Folders first, then by the chosen column; names compare the way a person
 * reads them (`file2` before `file10`), in the window's own language.
 */
export function sortEntries(
  entries: readonly FileEntry[],
  key: SortKey,
  ascending: boolean,
  locale: string,
): FileEntry[] {
  const collator = new Intl.Collator(locale, { numeric: true, sensitivity: 'base' });
  const direction = ascending ? 1 : -1;
  return [...entries].sort((a, b) => {
    if (a.is_dir !== b.is_dir) {
      return a.is_dir ? -1 : 1;
    }
    let order = 0;
    if (key === 'size') {
      order = a.size - b.size;
    } else if (key === 'modified') {
      order = a.modified_unix - b.modified_unix;
    }
    if (order === 0) {
      order = collator.compare(a.name, b.name);
    }
    return order * direction;
  });
}

/** The names between `from` and `to`, inclusive, in on-screen order. */
export function rangeBetween(order: readonly string[], from: string, to: string): string[] {
  const a = order.indexOf(from);
  const b = order.indexOf(to);
  if (a < 0 || b < 0) {
    return [to];
  }
  return order.slice(Math.min(a, b), Math.max(a, b) + 1);
}

/** A short date and time in the window's language, or nothing for "unknown". */
export function formatModified(unix: number, locale: string): string {
  if (unix <= 0) {
    return '';
  }
  return new Intl.DateTimeFormat(locale, { dateStyle: 'short', timeStyle: 'short' }).format(
    new Date(unix * 1000),
  );
}

const DIR_REFUSAL_KEY: Record<DirRefusal, TranslationKey> = {
  not_granted: 'fileManager.refused.notGranted',
  bad_path: 'fileManager.refused.badPath',
  unreadable: 'fileManager.refused.unreadable',
};

const OP_REFUSAL_KEY: Record<FileOpRefusal, TranslationKey> = {
  not_granted: 'fm.error.notGranted',
  bad_path: 'fm.error.badPath',
  exists: 'fm.error.exists',
  not_found: 'fm.error.notFound',
  busy: 'fm.error.busy',
  failed: 'fm.error.failed',
};

const LOCAL_OP_CODE: Record<string, FileOpRefusal> = {
  BAD_PATH: 'bad_path',
  EXISTS: 'exists',
  NOT_FOUND: 'not_found',
  FAILED: 'failed',
};

const JOB_FAILURE_KEY: Record<string, TranslationKey> = {
  declined: 'fm.failure.declined',
  not_granted: 'fileManager.download.notGranted',
  bad_path: 'fileManager.download.badPath',
  unreadable: 'fileManager.download.unreadable',
  too_many: 'fileManager.download.tooMany',
  unsupported: 'fm.error.tooOld',
  no_answer: 'fm.error.noAnswer',
  failed: 'files.state.failed',
};

/** The `code` of an `IpcError`, or an empty string for anything else. */
function ipcCode(error: unknown): string {
  return typeof error === 'object' && error !== null && 'code' in error
    ? String((error as { code: unknown }).code)
    : '';
}

/** A message at the bottom of the window. */
interface Notice {
  text: string;
  error: boolean;
  at: number;
}

/** An entry being dragged from one pane towards the other. */
interface Drag {
  side: Side;
  names: string[];
  startX: number;
  startY: number;
  x: number;
  y: number;
  /** Whether the pointer has moved far enough for this to be a drag at all. */
  moving: boolean;
}

/** Where something dragged is over: a pane, and a folder row in it if any. */
export interface DropTarget {
  side: Side;
  folder: string | null;
}

/** How far a press has to move before it is a drag rather than a click. */
const DRAG_THRESHOLD_PX = 5;

/** A transfer's bytes per second, smoothed over the last few polls. */
interface Speed {
  moved: number;
  at: number;
  rate: number;
}

/** Options the window's entry point passes in. */
export interface FileManagerOptions {
  peer: string;
  locale: Locale;
  commands: FileManagerCommands;
  fileCommands: FileCommands;
  /** Redraw; called whenever something on screen changed. */
  onChange: () => void;
  /** Where the remote pane was left last time, if the window remembers. */
  remoteStart?: string | null;
  /** Where the local pane was left last time. */
  localStart?: string | null;
  now?: () => number;
}

/** The window's state and everything a person can do in it. */
export class FileManager {
  readonly local: Pane = emptyPane('local');
  readonly remote: Pane = emptyPane('remote');
  /** The pane the keyboard acts on. */
  active: Side = 'local';
  /** What Ctrl+C took, waiting for a Ctrl+V on the other side. */
  clip: { side: Side; items: QueueItem[] } | null = null;
  notice: Notice | null = null;
  /** Entries waiting for "yes, delete them". */
  confirming: { side: Side; names: string[] } | null = null;
  /** The right-click menu, where it was opened. */
  menu: { side: Side; x: number; y: number } | null = null;
  /** Home and every drive, for the local pane's picker. */
  roots: string[] = [];
  transfers: FileTransfers = { offers: [], transfers: [] };
  /** Whether the last poll reached the session at all. */
  connected = true;
  drag: Drag | null = null;
  /** Where a drag — from the other pane or from the desktop — would land. */
  dropTarget: DropTarget | null = null;
  readonly queue: TransferQueue;
  /** Transfer rows the person cleared from the list. */
  private readonly hidden = new Set<number>();
  private readonly speeds = new Map<number, Speed>();
  private status: RemoteDir | null = null;
  private remoteAsk: { path: string; before: number; at: number; pushBack: boolean } | null = null;
  private remoteNext: { path: string; pushBack: boolean } | null = null;
  /** How many of `REMOTE_ROOTS` the first listing has tried. */
  private probed = 0;
  private probing = true;
  private readonly pendingOps = new Map<number, true>();
  /** The folder to land on once a pane has gone up out of it. */
  private readonly landOn: Record<Side, string | null> = { local: null, remote: null };
  readonly peer: string;
  private readonly locale: Locale;
  private readonly commands: FileManagerCommands;
  private readonly fileCommands: FileCommands;
  private readonly onChange: () => void;
  private readonly now: () => number;
  private readonly remoteStart: string | null;
  private readonly localStart: string | null;

  constructor(options: FileManagerOptions) {
    this.peer = options.peer;
    this.locale = options.locale;
    this.commands = options.commands;
    this.fileCommands = options.fileCommands;
    this.onChange = options.onChange;
    this.now = options.now ?? (() => Date.now());
    this.remoteStart = options.remoteStart ?? null;
    this.localStart = options.localStart ?? null;
    this.queue = new TransferQueue(this.peer, this.commands, this.now);
  }

  private text(key: TranslationKey, arg?: string): string {
    return t(this.locale, key, arg);
  }

  pane(side: Side): Pane {
    return side === 'local' ? this.local : this.remote;
  }

  other(side: Side): Side {
    return side === 'local' ? 'remote' : 'local';
  }

  /** Fills both panes for the first time. */
  async start(): Promise<void> {
    void this.commands.localRoots().then(
      (roots) => {
        this.roots = roots;
        this.onChange();
      },
      () => undefined,
    );
    const localFirst = this.localStart
      ? this.openLocal(this.localStart, false).then((ok) => ok || this.openLocal(undefined, false))
      : this.openLocal(undefined, false);
    await localFirst;
    // Where the remote pane was left last time, and the roots after it if
    // that folder is gone: `probing` stays on until one of them answers.
    if (this.remoteStart) {
      await this.openRemote(this.remoteStart, false);
    } else {
      await this.probeNextRoot();
    }
  }

  // --- navigation ----------------------------------------------------------

  /** Lists a local directory; `undefined` is the home directory. */
  async openLocal(path: string | undefined, pushBack = true): Promise<boolean> {
    const pane = this.local;
    pane.loading = true;
    this.onChange();
    try {
      const listed = await this.commands.localList(path);
      if (pushBack && pane.path && pane.path !== listed.path) {
        pane.back.push(pane.path);
      }
      const samePlace = pane.path === listed.path;
      pane.path = listed.path;
      pane.parent = listed.parent;
      pane.entries = listed.entries;
      pane.truncated = listed.truncated;
      pane.problem = null;
      this.keepSelection(pane, samePlace);
      this.land(pane);
      return true;
    } catch {
      pane.problem = 'fileManager.localUnreadable';
      if (path !== undefined && pane.path) {
        // The directory on screen is still the one it was; the problem is the
        // one that would not open, which a message says rather than a blank.
        pane.problem = null;
        this.say(this.text('fileManager.localUnreadable'), true);
      }
      return false;
    } finally {
      pane.loading = false;
      this.onChange();
    }
  }

  /**
   * Asks the host for a directory. One listing is in flight at a time — the
   * answer carries no path of its own — so a request made while another is
   * out waits for it, and only the newest such request is kept.
   */
  async openRemote(path: string, pushBack = true): Promise<void> {
    if (this.remoteAsk) {
      this.remoteNext = { path, pushBack };
      return;
    }
    const pane = this.remote;
    pane.loading = true;
    this.onChange();
    try {
      const before = (await this.commands.remoteStatus()).listings_answered;
      await this.commands.remoteList(path);
      this.remoteAsk = { path, before, at: this.now(), pushBack };
    } catch (error: unknown) {
      pane.loading = false;
      const code = ipcCode(error);
      if (code === 'PEER_TOO_OLD') {
        this.probing = false;
        pane.problem = 'fm.error.tooOld';
      } else if (code === 'CORE') {
        // A path the actor will not send: refused here before the host saw
        // it, so it is this window's to say.
        this.probing = false;
        if (pane.path) {
          this.say(this.text('fileManager.refused.badPath'), true);
        } else {
          pane.problem = 'fileManager.refused.badPath';
        }
      } else {
        // The session is not there right now — dropped, or coming back
        // (ADR 0105). The request waits for it rather than failing.
        this.connected = false;
        this.remoteNext = { path, pushBack };
      }
      this.onChange();
    }
  }

  private async probeNextRoot(): Promise<void> {
    const root = REMOTE_ROOTS[this.probed];
    this.probed += 1;
    if (root === undefined) {
      this.probing = false;
      return;
    }
    await this.openRemote(root, false);
  }

  refresh(side: Side): void {
    const pane = this.pane(side);
    if (side === 'local') {
      void this.openLocal(pane.path || undefined, false);
    } else if (pane.path) {
      void this.openRemote(pane.path, false);
    }
  }

  goUp(side: Side): void {
    const pane = this.pane(side);
    if (!pane.parent) {
      return;
    }
    // Land on the folder just left, the way every file manager does.
    this.landOn[side] = baseName(pane.path);
    if (side === 'local') {
      void this.openLocal(pane.parent);
    } else {
      void this.openRemote(pane.parent);
    }
  }

  private land(pane: Pane): void {
    const name = this.landOn[pane.side];
    this.landOn[pane.side] = null;
    if (name && pane.entries.some((entry) => entry.name === name)) {
      pane.cursor = name;
      pane.selected = new Set([name]);
      pane.anchor = name;
    }
  }

  goBack(side: Side): void {
    const pane = this.pane(side);
    const previous = pane.back.pop();
    if (previous === undefined) {
      return;
    }
    if (side === 'local') {
      void this.openLocal(previous, false);
    } else {
      void this.openRemote(previous, false);
    }
  }

  navigate(side: Side, path: string): void {
    const trimmed = path.trim();
    if (!trimmed) {
      return;
    }
    if (side === 'local') {
      void this.openLocal(trimmed);
    } else {
      void this.openRemote(trimmed);
    }
  }

  /** Double-click or Enter: into a folder, or the file across to the other side. */
  activate(side: Side, name: string): void {
    const pane = this.pane(side);
    if (name === '..') {
      this.goUp(side);
      return;
    }
    const entry = pane.entries.find((candidate) => candidate.name === name);
    if (!entry) {
      return;
    }
    if (entry.is_dir) {
      this.navigate(side, joinPath(pane.path, entry.name));
    } else {
      this.transfer(side, [entry.name]);
    }
  }

  private keepSelection(pane: Pane, samePlace: boolean): void {
    const names = new Set(pane.entries.map((entry) => entry.name));
    if (samePlace) {
      pane.selected = new Set([...pane.selected].filter((name) => names.has(name)));
      if (pane.cursor && !names.has(pane.cursor)) {
        pane.cursor = null;
      }
    } else {
      pane.selected = new Set();
      pane.cursor = null;
      pane.anchor = null;
      pane.editing = null;
    }
  }

  // --- selection -----------------------------------------------------------

  /** The names of a pane in on-screen order. */
  order(side: Side): string[] {
    const pane = this.pane(side);
    return sortEntries(pane.entries, pane.sort, pane.ascending, this.locale).map(
      (entry) => entry.name,
    );
  }

  select(side: Side, name: string, mods: { ctrl: boolean; shift: boolean }): void {
    const pane = this.pane(side);
    this.active = side;
    if (mods.shift && pane.anchor) {
      const range = rangeBetween(this.order(side), pane.anchor, name);
      pane.selected = new Set(mods.ctrl ? [...pane.selected, ...range] : range);
    } else if (mods.ctrl) {
      if (pane.selected.has(name)) {
        pane.selected.delete(name);
      } else {
        pane.selected.add(name);
      }
      pane.anchor = name;
    } else {
      pane.selected = new Set([name]);
      pane.anchor = name;
    }
    pane.cursor = name;
    this.onChange();
  }

  selectAll(side: Side): void {
    const pane = this.pane(side);
    pane.selected = new Set(pane.entries.map((entry) => entry.name));
    this.onChange();
  }

  clearSelection(side: Side): void {
    const pane = this.pane(side);
    pane.selected = new Set();
    pane.anchor = null;
    this.onChange();
  }

  /** Arrow keys: the keyboard row moves, and the selection with it. */
  moveCursor(side: Side, delta: number, extend: boolean): void {
    const order = this.order(side);
    if (order.length === 0) {
      return;
    }
    const pane = this.pane(side);
    const at = pane.cursor ? order.indexOf(pane.cursor) : -1;
    const next = Math.min(order.length - 1, Math.max(0, at < 0 ? 0 : at + delta));
    const name = order[next];
    if (name !== undefined) {
      this.select(side, name, { ctrl: false, shift: extend });
    }
  }

  /** The selected entries of a pane, in on-screen order. */
  selection(side: Side): FileEntry[] {
    const pane = this.pane(side);
    return sortEntries(pane.entries, pane.sort, pane.ascending, this.locale).filter((entry) =>
      pane.selected.has(entry.name),
    );
  }

  sortBy(side: Side, key: SortKey): void {
    const pane = this.pane(side);
    if (pane.sort === key) {
      pane.ascending = !pane.ascending;
    } else {
      pane.sort = key;
      pane.ascending = true;
    }
    this.onChange();
  }

  // --- moving files --------------------------------------------------------

  /**
   * Whether files can move between the panes right now: both are showing a
   * folder, and the remote one is not refused or still loading.
   */
  canTransfer(): boolean {
    const remote = this.remote;
    return (
      this.connected &&
      Boolean(this.local.path) &&
      Boolean(remote.path) &&
      remote.problem === null &&
      !remote.loading
    );
  }

  /**
   * Queues the named entries of `side` for the other side, into `folder` or
   * the directory the other pane is showing.
   */
  transfer(side: Side, names: readonly string[], folder: string | null = null): void {
    if (!this.canTransfer() || names.length === 0) {
      return;
    }
    const from = this.pane(side);
    const to = this.pane(this.other(side));
    const target = folder ? joinPath(to.path, folder) : to.path;
    const items: QueueItem[] = names.flatMap((name) => {
      const entry = from.entries.find((candidate) => candidate.name === name);
      return entry
        ? [{ source: joinPath(from.path, entry.name), isDir: entry.is_dir, size: entry.size }]
        : [];
    });
    this.queue.add(side === 'local' ? 'upload' : 'download', items, target);
    this.onChange();
    void this.pumpNow();
  }

  /** The Upload / Download button: the selection across. */
  transferSelection(side: Side): void {
    this.transfer(
      side,
      this.selection(side).map((entry) => entry.name),
    );
  }

  copy(side: Side): void {
    const pane = this.pane(side);
    const items = this.selection(side).map((entry) => ({
      source: joinPath(pane.path, entry.name),
      isDir: entry.is_dir,
      size: entry.size,
    }));
    if (items.length === 0) {
      return;
    }
    this.clip = { side, items };
    this.say(this.text('fm.copied', String(items.length)), false);
  }

  canPaste(side: Side): boolean {
    return this.clip !== null && this.clip.side !== side && this.canTransfer();
  }

  paste(side: Side): void {
    const clip = this.clip;
    if (!clip) {
      return;
    }
    if (clip.side === side) {
      this.say(this.text('fm.pasteOther'), false);
      return;
    }
    if (!this.canTransfer()) {
      return;
    }
    const target = this.pane(side).path;
    this.queue.add(clip.side === 'local' ? 'upload' : 'download', clip.items, target);
    this.onChange();
    void this.pumpNow();
  }

  /**
   * Files dragged in from the desktop. Only the remote pane takes them: a
   * copy from this machine to itself is the desktop's own job.
   */
  async dropFromDesktop(paths: readonly string[], target: DropTarget | null): Promise<void> {
    this.dropTarget = null;
    if (!target || target.side === 'local') {
      this.say(this.text('fm.dropLocal'), false);
      this.onChange();
      return;
    }
    if (!this.canTransfer() || paths.length === 0) {
      this.onChange();
      return;
    }
    const remote = this.remote;
    const into = target.folder ? joinPath(remote.path, target.folder) : remote.path;
    // Whether a dropped path is a folder is a fact about this machine's disk
    // the page cannot see; a listing that succeeds is the answer.
    const items = await Promise.all(
      paths.map(async (source) => {
        const isDir = await this.commands.localList(source).then(
          () => true,
          () => false,
        );
        return { source, isDir, size: 0 };
      }),
    );
    this.queue.add('upload', items, into);
    this.onChange();
    void this.pumpNow();
  }

  // --- changing files ------------------------------------------------------

  startCreate(side: Side): void {
    const pane = this.pane(side);
    if (!pane.path || pane.problem !== null) {
      return;
    }
    this.active = side;
    pane.editing = { kind: 'create' };
    this.onChange();
  }

  startRename(side: Side): void {
    const pane = this.pane(side);
    const target = pane.cursor && pane.selected.has(pane.cursor) ? pane.cursor : [...pane.selected][0];
    if (!target || pane.selected.size !== 1) {
      return;
    }
    this.active = side;
    pane.editing = { kind: 'rename', name: target };
    this.onChange();
  }

  cancelEdit(side: Side): void {
    this.pane(side).editing = null;
    this.onChange();
  }

  /** Enter in the name field: make the folder, or rename the entry. */
  async commitEdit(side: Side, value: string): Promise<void> {
    const pane = this.pane(side);
    const editing = pane.editing;
    pane.editing = null;
    const name = value.trim();
    if (!editing || !name) {
      this.onChange();
      return;
    }
    if (editing.kind === 'rename') {
      if (name === editing.name) {
        this.onChange();
        return;
      }
      pane.selected = new Set([name]);
      pane.cursor = name;
      pane.anchor = name;
      await this.change(side, { kind: 'rename', path: joinPath(pane.path, editing.name), name });
    } else {
      pane.selected = new Set([name]);
      pane.cursor = name;
      pane.anchor = name;
      await this.change(side, { kind: 'make_dir', path: joinPath(pane.path, name) });
    }
  }

  askDelete(side: Side): void {
    const names = this.selection(side).map((entry) => entry.name);
    if (names.length === 0) {
      return;
    }
    this.confirming = { side, names };
    this.onChange();
  }

  cancelDelete(): void {
    this.confirming = null;
    this.onChange();
  }

  async confirmDelete(): Promise<void> {
    const confirming = this.confirming;
    this.confirming = null;
    if (!confirming) {
      return;
    }
    const pane = this.pane(confirming.side);
    const directory = pane.path;
    pane.selected = new Set();
    for (const name of confirming.names) {
      await this.change(confirming.side, { kind: 'delete', path: joinPath(directory, name) }, false);
    }
    // The remote pane re-reads itself once the host has answered the last of
    // them (`applyOpResults`); asking now would list the folder before it.
    if (confirming.side === 'local') {
      this.refresh('local');
    }
  }

  /** Runs one change on the side it belongs to, and re-reads that side. */
  private async change(side: Side, op: FileOp, refreshAfter = true): Promise<void> {
    if (side === 'local') {
      try {
        await this.commands.localOp(op);
      } catch (error: unknown) {
        const refusal = LOCAL_OP_CODE[ipcCode(error)] ?? 'failed';
        this.say(this.text(OP_REFUSAL_KEY[refusal]), true);
      }
      if (refreshAfter) {
        this.refresh('local');
      }
      this.onChange();
      return;
    }
    try {
      const id = await this.commands.remoteOp(op);
      this.pendingOps.set(id, true);
    } catch (error: unknown) {
      this.say(
        this.text(ipcCode(error) === 'PEER_TOO_OLD' ? 'fm.error.tooOld' : 'fm.error.badPath'),
        true,
      );
    }
    this.onChange();
  }

  // --- transfers -----------------------------------------------------------

  answerOffer(accept: boolean, fromClipboard: boolean): void {
    void this.fileCommands.accept(this.peer, accept, fromClipboard).then(
      () => this.pumpNow(),
      () => this.pumpNow(),
    );
  }

  cancelTransfer(transferId: number): void {
    void this.fileCommands.abort(this.peer, transferId).then(
      () => this.pumpNow(),
      () => this.pumpNow(),
    );
  }

  cancelJob(jobId: number): void {
    if (this.queue.cancelQueued(jobId)) {
      this.onChange();
    }
  }

  clearFinished(): void {
    this.queue.clearFinished();
    for (const row of this.rows()) {
      if (row.state !== 'running') {
        this.hidden.add(row.transfer_id);
      }
    }
    this.onChange();
  }

  /** This session's transfer rows the person has not cleared. */
  rows(): TransferRow[] {
    return this.transfers.transfers.filter(
      (row) => row.peer_label === this.peer && !this.hidden.has(row.transfer_id),
    );
  }

  /** Bytes per second of one running transfer, once two polls have seen it. */
  speed(transferId: number): number {
    return this.speeds.get(transferId)?.rate ?? 0;
  }

  private trackSpeeds(): void {
    const at = this.now();
    for (const row of this.transfers.transfers) {
      if (row.peer_label !== this.peer || row.state !== 'running') {
        this.speeds.delete(row.transfer_id);
        continue;
      }
      const seen = this.speeds.get(row.transfer_id);
      if (!seen) {
        this.speeds.set(row.transfer_id, { moved: row.moved, at, rate: 0 });
        continue;
      }
      const seconds = (at - seen.at) / 1000;
      if (seconds <= 0) {
        continue;
      }
      const instant = Math.max(0, row.moved - seen.moved) / seconds;
      const rate = seen.rate === 0 ? instant : seen.rate * 0.6 + instant * 0.4;
      this.speeds.set(row.transfer_id, { moved: row.moved, at, rate });
    }
  }

  private async pumpNow(): Promise<void> {
    try {
      this.transfers = await this.fileCommands.list();
    } catch {
      return;
    }
    const finished = await this.queue.pump(this.transfers, this.status);
    this.afterFinished(finished);
    this.onChange();
  }

  private afterFinished(finished: readonly Job[]): void {
    // Re-read whichever pane is showing a folder something just landed in.
    for (const job of finished) {
      const to: Side = job.direction === 'upload' ? 'remote' : 'local';
      if (this.pane(to).path === job.target) {
        this.refresh(to);
      }
    }
  }

  // --- the poll ------------------------------------------------------------

  /** One round: the remote pane's state, the transfer list, the queue. */
  async poll(): Promise<void> {
    let status: RemoteDir;
    try {
      status = await this.commands.remoteStatus();
      this.transfers = await this.fileCommands.list();
    } catch {
      // The session dropped or is coming back (ADR 0105): nothing here is
      // reachable until it does, and the window says so rather than
      // pretending the last listing is still current.
      if (this.connected) {
        this.connected = false;
        this.onChange();
      }
      return;
    }
    const reconnected = !this.connected;
    this.connected = true;
    if (reconnected && this.remoteNext) {
      this.sendNextRemote();
    } else if (reconnected && this.remote.path) {
      this.refresh('remote');
    }
    this.status = status;
    this.applyRemoteAnswer(status);
    this.applyOpResults(status);
    this.trackSpeeds();
    const finished = await this.queue.pump(this.transfers, status);
    this.afterFinished(finished);
    if (this.notice && this.now() - this.notice.at > NOTICE_MS) {
      this.notice = null;
    }
    this.onChange();
  }

  private applyRemoteAnswer(status: RemoteDir): void {
    const ask = this.remoteAsk;
    if (!ask) {
      return;
    }
    const pane = this.remote;
    if (status.listings_answered === ask.before) {
      if (this.now() - ask.at > REMOTE_LIST_TIMEOUT_MS) {
        this.remoteAsk = null;
        pane.loading = false;
        if (pane.path) {
          this.say(this.text('fm.error.noAnswer'), true);
        } else {
          pane.problem = 'fm.error.noAnswer';
        }
        this.probing = false;
        this.sendNextRemote();
      }
      return;
    }
    this.remoteAsk = null;
    pane.loading = false;
    if (status.refused !== null) {
      if (this.probing && status.refused !== 'not_granted' && this.probed < REMOTE_ROOTS.length) {
        // The folder remembered from last time is gone, or this is the wrong
        // operating system's root: try the next one.
        void this.probeNextRoot();
        return;
      }
      this.probing = false;
      if (status.refused === 'not_granted' || !pane.path) {
        pane.problem = DIR_REFUSAL_KEY[status.refused];
        pane.entries = [];
        pane.path = pane.path || ask.path;
      } else {
        // The folder on screen is still the one it was; the one that would
        // not open is a message, not a blank pane.
        this.say(this.text(DIR_REFUSAL_KEY[status.refused]), true);
      }
      this.sendNextRemote();
      return;
    }
    this.probing = false;
    if (ask.pushBack && pane.path && pane.path !== status.path) {
      pane.back.push(pane.path);
    }
    const samePlace = pane.path === status.path;
    pane.path = status.path;
    pane.parent = status.parent;
    pane.entries = status.entries;
    pane.truncated = status.truncated;
    pane.problem = null;
    this.keepSelection(pane, samePlace);
    this.land(pane);
    this.sendNextRemote();
  }

  private sendNextRemote(): void {
    const next = this.remoteNext;
    this.remoteNext = null;
    if (next) {
      void this.openRemote(next.path, next.pushBack);
    }
  }

  private applyOpResults(status: RemoteDir): void {
    let changed = false;
    for (const result of status.op_results) {
      if (!this.pendingOps.delete(result.id)) {
        continue;
      }
      changed = true;
      if (result.refused !== null) {
        this.say(this.text(OP_REFUSAL_KEY[result.refused]), true);
      }
    }
    if (changed && this.pendingOps.size === 0) {
      this.refresh('remote');
    }
  }

  say(text: string, error: boolean): void {
    this.notice = { text, error, at: this.now() };
    this.onChange();
  }

  dismissNotice(): void {
    this.notice = null;
    this.onChange();
  }

  // --- pointer drag between the panes -------------------------------------

  /**
   * A press on a row that may become a drag. The row is selected first if it
   * was not, so what is dragged is always what is highlighted.
   */
  pressRow(side: Side, name: string, event: PointerEvent): void {
    if (event.button !== 0) {
      return;
    }
    const pane = this.pane(side);
    const ctrl = event.ctrlKey || event.metaKey;
    if (ctrl || event.shiftKey || !pane.selected.has(name)) {
      this.select(side, name, { ctrl, shift: event.shiftKey });
    } else {
      // Pressing a row that is already part of the selection keeps the whole
      // selection, so it can be dragged; a click without a drag narrows it
      // afterwards.
      this.active = side;
      pane.cursor = name;
    }
    this.drag = {
      side,
      names: [...pane.selected],
      startX: event.clientX,
      startY: event.clientY,
      x: event.clientX,
      y: event.clientY,
      moving: false,
    };
  }

  /** Pointer motion anywhere in the window while a press is held. */
  dragMove(x: number, y: number, target: DropTarget | null): void {
    const drag = this.drag;
    if (!drag) {
      return;
    }
    drag.x = x;
    drag.y = y;
    if (!drag.moving && Math.hypot(x - drag.startX, y - drag.startY) >= DRAG_THRESHOLD_PX) {
      drag.moving = true;
    }
    if (drag.moving) {
      this.dropTarget = target && target.side !== drag.side ? target : null;
      this.onChange();
    }
  }

  /** The press let go: a drop on the other pane moves the files there. */
  dragEnd(): boolean {
    const drag = this.drag;
    const target = this.dropTarget;
    this.drag = null;
    this.dropTarget = null;
    if (!drag?.moving) {
      return false;
    }
    if (target && target.side !== drag.side) {
      this.transfer(drag.side, drag.names, target.folder);
    }
    this.onChange();
    return true;
  }

  /** A drag from the desktop is over the window. */
  desktopDragOver(target: DropTarget | null): void {
    this.dropTarget = target;
    this.onChange();
  }

  // --- keyboard --------------------------------------------------------------

  /**
   * The window's keys. Returns whether the key was the window's own, so the
   * caller can keep the webview from acting on it too — F5 and Ctrl+R would
   * otherwise reload the page out from under a transfer.
   */
  onKey(event: KeyboardEvent): boolean {
    const ctrl = event.ctrlKey || event.metaKey;
    const key = event.key;
    if (key === 'F5' || (ctrl && (key === 'r' || key === 'R'))) {
      this.refresh(this.active);
      return true;
    }
    if (this.confirming) {
      if (key === 'Escape') {
        this.cancelDelete();
        return true;
      }
      return false;
    }
    if (this.menu && key === 'Escape') {
      this.closeMenu();
      return true;
    }
    const target = event.target as HTMLElement | null;
    if (target && (target.tagName === 'INPUT' || target.tagName === 'SELECT')) {
      return false;
    }
    const side = this.active;
    const pane = this.pane(side);
    if (pane.editing) {
      return false;
    }
    switch (key) {
      case 'ArrowDown':
        this.moveCursor(side, 1, event.shiftKey);
        return true;
      case 'ArrowUp':
        if (event.altKey) {
          this.goUp(side);
        } else {
          this.moveCursor(side, -1, event.shiftKey);
        }
        return true;
      case 'ArrowLeft':
        if (event.altKey) {
          this.goBack(side);
          return true;
        }
        return false;
      case 'Home':
        this.moveCursor(side, -Infinity, event.shiftKey);
        return true;
      case 'End':
        this.moveCursor(side, Infinity, event.shiftKey);
        return true;
      case 'PageDown':
        this.moveCursor(side, 10, event.shiftKey);
        return true;
      case 'PageUp':
        this.moveCursor(side, -10, event.shiftKey);
        return true;
      case 'Enter':
        if (pane.cursor) {
          this.activate(side, pane.cursor);
        }
        return true;
      case 'Backspace':
        this.goUp(side);
        return true;
      case 'Delete':
        this.askDelete(side);
        return true;
      case 'F2':
        this.startRename(side);
        return true;
      case 'F7':
        this.startCreate(side);
        return true;
      case 'Escape':
        this.clearSelection(side);
        return true;
      default:
        break;
    }
    if (ctrl && !event.altKey) {
      switch (key.toLowerCase()) {
        case 'a':
          this.selectAll(side);
          return true;
        case 'c':
          this.copy(side);
          return true;
        case 'v':
          this.paste(side);
          return true;
        case 'n':
          if (event.shiftKey) {
            this.startCreate(side);
            return true;
          }
          return false;
        default:
          return false;
      }
    }
    return false;
  }

  // --- context menu ----------------------------------------------------------

  openMenu(side: Side, x: number, y: number, name: string | null): void {
    const pane = this.pane(side);
    this.active = side;
    if (name === null) {
      pane.selected = new Set();
    } else if (!pane.selected.has(name)) {
      pane.selected = new Set([name]);
      pane.anchor = name;
      pane.cursor = name;
    }
    this.menu = { side, x, y };
    this.onChange();
  }

  closeMenu(): void {
    if (this.menu) {
      this.menu = null;
      this.onChange();
    }
  }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const ICON = {
  folder: html`<svg class="fm-glyph is-folder" viewBox="0 0 16 16" aria-hidden="true"><path d="M1.75 4.25A1.25 1.25 0 0 1 3 3h3.1l1.4 1.6H13a1.25 1.25 0 0 1 1.25 1.25v6.4A1.25 1.25 0 0 1 13 13.5H3a1.25 1.25 0 0 1-1.25-1.25Z" fill="currentColor"/></svg>`,
  file: html`<svg class="fm-glyph is-file" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 1.75h5.2L12.75 5.3v8.2c0 .4-.3.75-.75.75H4a.75.75 0 0 1-.75-.75V2.5c0-.4.35-.75.75-.75Z" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/><path d="M9 1.9v3.6h3.6" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>`,
  up: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 13V3M3.5 7.5 8 3l4.5 4.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  back: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8H3M7.5 3.5 3 8l4.5 4.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  refresh: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><path d="M13.5 2.5V6H10" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  newFolder: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M1.75 4.25A1.25 1.25 0 0 1 3 3h3.1l1.4 1.6H13a1.25 1.25 0 0 1 1.25 1.25v6.4A1.25 1.25 0 0 1 13 13.5H3a1.25 1.25 0 0 1-1.25-1.25Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M8 7v4.2M5.9 9.1h4.2" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg>`,
  rename: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M10.5 2.5 13.5 5.5 6 13H3v-3Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/></svg>`,
  trash: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 4.5h11M6.5 4.5V3h3v1.5M4 4.5l.7 8.5c.05.6.5 1 1.1 1h4.4c.6 0 1.05-.4 1.1-1l.7-8.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  toRemote: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 8h10M9 4.5 12.5 8 9 11.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  toLocal: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M13.5 8h-10M7 4.5 3.5 8 7 11.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  computer: html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1.75" y="2.5" width="12.5" height="8.5" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M5.5 13.5h5M8 11v2.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg>`,
  remote: html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1.75" y="2.5" width="12.5" height="8.5" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M5.5 13.5h5M8 11v2.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><path d="M6.2 5.2c1-.9 2.6-.9 3.6 0M7.1 6.6c.5-.4 1.3-.4 1.8 0" fill="none" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/><circle cx="8" cy="8.2" r=".7" fill="currentColor"/></svg>`,
  lock: html`<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="5" y="10.5" width="14" height="10" rx="2" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M8.5 10.5V7.5a3.5 3.5 0 0 1 7 0v3" fill="none" stroke="currentColor" stroke-width="1.5"/></svg>`,
  close: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4.5 4.5 7 7m0-7-7 7" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>`,
  arrowUp: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 13V3.5M4 7.5 8 3.5l4 4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  arrowDown: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3v9.5M4 8.5l4 4 4-4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
};

/** Everything the window draws, from the controller's state. */
export function renderFileManager(
  fm: FileManager,
  locale: Locale,
  options: { standalone: boolean; onDisconnect: () => void },
): TemplateResult {
  return html`
    <div class="fm ${fm.drag?.moving ? 'is-dragging' : ''}" data-testid="file-manager">
      <header class="fm-head">
        <div class="fm-head-title">
          <span class="fm-head-icon" aria-hidden="true">${ICON.folder}</span>
          <h1>${t(locale, 'fileManager.heading')}</h1>
        </div>
        <p class="fm-head-hint">${t(locale, 'fm.hint')}</p>
        ${options.standalone
          ? html`<button
              type="button"
              class="fm-btn is-danger"
              data-testid="fm-disconnect"
              @click=${options.onDisconnect}
            >
              ${t(locale, 'fm.disconnect')}
            </button>`
          : nothing}
      </header>
      ${fm.connected
        ? nothing
        : html`<div class="fm-banner" role="status" data-testid="fm-lost">${t(locale, 'fm.lost')}</div>`}
      <div class="fm-panes">${renderPane(fm, 'local', locale)} ${renderPane(fm, 'remote', locale)}</div>
      ${renderTransfers(fm, locale)} ${renderNotice(fm)} ${renderMenu(fm, locale)}
      ${renderConfirm(fm, locale)} ${renderGhost(fm, locale)}
    </div>
  `;
}

function renderPane(fm: FileManager, side: Side, locale: Locale): TemplateResult {
  const pane = fm.pane(side);
  const isLocal = side === 'local';
  const title = t(locale, isLocal ? 'fileManager.local' : 'fileManager.remote');
  const entries = sortEntries(pane.entries, pane.sort, pane.ascending, locale);
  const selected = fm.selection(side);
  const selectedBytes = selected.reduce((total, entry) => total + (entry.is_dir ? 0 : entry.size), 0);
  const usable = Boolean(pane.path) && pane.problem === null;
  const dropping = fm.dropTarget?.side === side;
  const cursorId = pane.cursor ? rowId(side, pane.cursor) : '';
  const canSend = fm.canTransfer() && selected.length > 0;
  const sortMark = (key: SortKey): 'ascending' | 'descending' | 'none' =>
    pane.sort === key ? (pane.ascending ? 'ascending' : 'descending') : 'none';

  const header = (key: SortKey, label: TranslationKey, cls: string): TemplateResult => html`
    <th class=${cls} aria-sort=${sortMark(key)} scope="col">
      <button type="button" class="fm-sort" @click=${() => fm.sortBy(side, key)}>
        ${t(locale, label)}
        <span class="fm-sort-mark" aria-hidden="true"
          >${pane.sort === key ? (pane.ascending ? '▲' : '▼') : ''}</span
        >
      </button>
    </th>
  `;

  return html`
    <section
      class="fm-pane ${fm.active === side ? 'is-active' : ''} ${dropping ? 'is-drop' : ''}"
      data-pane=${side}
      data-testid=${`fm-pane-${side}`}
      aria-label=${title}
      @focusin=${() => {
        fm.active = side;
      }}
    >
      <div class="fm-pane-head">
        <span class="fm-pane-icon" aria-hidden="true">${isLocal ? ICON.computer : ICON.remote}</span>
        <h2>${title}</h2>
        <button
          type="button"
          class="fm-btn is-primary"
          data-testid=${isLocal ? 'fm-upload' : 'fm-download'}
          ?disabled=${!canSend}
          @click=${() => fm.transferSelection(side)}
        >
          ${isLocal ? nothing : ICON.toLocal}
          <span>${t(locale, isLocal ? 'fileManager.upload' : 'fileManager.download')}</span>
          ${isLocal ? ICON.toRemote : nothing}
        </button>
      </div>
      <div class="fm-nav">
        <button
          type="button"
          class="fm-icon-btn"
          data-testid=${`fm-${side}-back`}
          aria-label=${t(locale, 'fm.back')}
          title=${t(locale, 'fm.back')}
          ?disabled=${pane.back.length === 0}
          @click=${() => fm.goBack(side)}
        >
          ${ICON.back}
        </button>
        <button
          type="button"
          class="fm-icon-btn"
          data-testid=${`fm-${side}-up`}
          aria-label=${t(locale, 'fileManager.up')}
          title=${t(locale, 'fileManager.up')}
          ?disabled=${!pane.parent}
          @click=${() => fm.goUp(side)}
        >
          ${ICON.up}
        </button>
        <button
          type="button"
          class="fm-icon-btn"
          data-testid=${`fm-${side}-refresh`}
          aria-label=${t(locale, 'fm.refresh')}
          title=${t(locale, 'fm.refresh')}
          ?disabled=${!pane.path}
          @click=${() => fm.refresh(side)}
        >
          ${ICON.refresh}
        </button>
        ${isLocal && fm.roots.length > 0
          ? html`<select
              class="fm-roots"
              data-testid="fm-local-roots"
              aria-label=${t(locale, 'fm.drives')}
              title=${t(locale, 'fm.drives')}
              @change=${(event: Event) => {
                const select = event.target as HTMLSelectElement;
                if (select.value) {
                  fm.navigate('local', select.value);
                }
                select.value = '';
              }}
            >
              <option value="" selected>${t(locale, 'fm.drives')}</option>
              ${fm.roots.map((root) => html`<option value=${root}>${root}</option>`)}
            </select>`
          : nothing}
        <input
          class="fm-path"
          data-testid=${`fm-${side}-path`}
          aria-label=${t(locale, 'fm.path')}
          dir="ltr"
          spellcheck="false"
          autocomplete="off"
          .value=${pane.path}
          @keydown=${(event: KeyboardEvent) => {
            const input = event.target as HTMLInputElement;
            if (event.key === 'Enter') {
              event.preventDefault();
              fm.navigate(side, input.value);
              input.blur();
            } else if (event.key === 'Escape') {
              input.value = pane.path;
              input.blur();
            }
          }}
          @blur=${(event: FocusEvent) => {
            (event.target as HTMLInputElement).value = pane.path;
          }}
        />
      </div>
      <div class="fm-tools">
        <button
          type="button"
          class="fm-tool"
          data-testid=${`fm-${side}-new-folder`}
          ?disabled=${!usable}
          @click=${() => fm.startCreate(side)}
        >
          ${ICON.newFolder}<span>${t(locale, 'fm.newFolder')}</span>
        </button>
        <button
          type="button"
          class="fm-tool"
          data-testid=${`fm-${side}-rename`}
          ?disabled=${!usable || selected.length !== 1}
          @click=${() => fm.startRename(side)}
        >
          ${ICON.rename}<span>${t(locale, 'fm.rename')}</span>
        </button>
        <button
          type="button"
          class="fm-tool is-danger"
          data-testid=${`fm-${side}-delete`}
          ?disabled=${!usable || selected.length === 0}
          @click=${() => fm.askDelete(side)}
        >
          ${ICON.trash}<span>${t(locale, 'fm.delete')}</span>
        </button>
      </div>
      <div
        class="fm-table-wrap"
        @contextmenu=${(event: MouseEvent) => {
          if ((event.target as HTMLElement).closest('input')) {
            return;
          }
          event.preventDefault();
          const row = (event.target as HTMLElement).closest<HTMLElement>('[data-name]');
          const name = row?.dataset.name ?? null;
          fm.openMenu(side, event.clientX, event.clientY, name === '..' ? null : name);
        }}
        @pointerdown=${(event: PointerEvent) => {
          if (!(event.target as HTMLElement).closest('[data-name]') && event.button === 0) {
            fm.active = side;
            fm.clearSelection(side);
          }
        }}
      >
        ${pane.problem !== null
          ? renderProblem(pane.problem, locale, side)
          : html`
              <table
                class="fm-table"
                role="grid"
                aria-multiselectable="true"
                aria-label=${title}
                tabindex="0"
                aria-activedescendant=${cursorId || nothing}
                data-testid=${`fm-${side}-list`}
                @focus=${() => {
                  fm.active = side;
                }}
              >
                <thead>
                  <tr>
                    ${header('name', 'fm.col.name', 'fm-col-name')}
                    ${header('size', 'fm.col.size', 'fm-col-size')}
                    ${header('modified', 'fm.col.modified', 'fm-col-date')}
                  </tr>
                </thead>
                <tbody>
                  ${pane.parent
                    ? html`<tr
                        class="fm-row is-up"
                        data-name=".."
                        aria-selected="false"
                        @dblclick=${() => fm.goUp(side)}
                      >
                        <td class="fm-col-name">
                          <span class="fm-name">${ICON.folder}<span>..</span></span>
                        </td>
                        <td class="fm-col-size"></td>
                        <td class="fm-col-date"></td>
                      </tr>`
                    : nothing}
                  ${pane.editing?.kind === 'create' ? renderNameRow(fm, side, locale, null) : nothing}
                  ${entries.map((entry) =>
                    pane.editing?.kind === 'rename' && pane.editing.name === entry.name
                      ? renderNameRow(fm, side, locale, entry)
                      : renderRow(fm, side, entry, locale),
                  )}
                </tbody>
              </table>
              ${pane.loading && pane.entries.length === 0
                ? html`<p class="fm-empty" data-testid=${`fm-${side}-loading`}>
                    ${t(locale, 'fileManager.loading')}
                  </p>`
                : !pane.loading && pane.entries.length === 0 && pane.editing === null
                  ? html`<p class="fm-empty" data-testid=${`fm-${side}-empty`}>
                      ${t(locale, 'fileManager.empty')}
                    </p>`
                  : nothing}
              ${dropping
                ? html`<div class="fm-drop-hint" aria-hidden="true">
                    ${t(locale, 'fm.dropHere')}
                  </div>`
                : nothing}
            `}
      </div>
      <footer class="fm-pane-status" data-testid=${`fm-${side}-status`}>
        <span>${t(locale, 'fm.items', String(pane.entries.length))}</span>
        ${selected.length > 0
          ? html`<span
              >${t(locale, 'fm.selected', String(selected.length))}${selectedBytes > 0
                ? html` · ${formatSize(selectedBytes)}`
                : nothing}</span
            >`
          : nothing}
        ${pane.truncated
          ? html`<span class="fm-warn" data-testid=${`fm-${side}-truncated`}
              >${t(locale, 'fileManager.truncated')}</span
            >`
          : nothing}
        ${pane.loading && pane.entries.length > 0
          ? html`<span class="fm-spinner" aria-label=${t(locale, 'fileManager.loading')}></span>`
          : nothing}
      </footer>
    </section>
  `;
}

function rowId(side: Side, name: string): string {
  // An id has to be unique in the page and survive any name: the name's
  // code points in hex are both.
  const hex = [...name].map((c) => c.codePointAt(0)?.toString(16) ?? '0').join('-');
  return `fm-${side}-row-${hex}`;
}

function renderRow(fm: FileManager, side: Side, entry: FileEntry, locale: Locale): TemplateResult {
  const pane = fm.pane(side);
  const selected = pane.selected.has(entry.name);
  const isCursor = pane.cursor === entry.name;
  const isDropFolder =
    entry.is_dir && fm.dropTarget?.side === side && fm.dropTarget.folder === entry.name;
  return html`
    <tr
      id=${rowId(side, entry.name)}
      class="fm-row ${selected ? 'is-selected' : ''} ${isCursor ? 'is-cursor' : ''} ${isDropFolder
        ? 'is-drop-folder'
        : ''}"
      data-name=${entry.name}
      data-dir=${entry.is_dir ? '1' : '0'}
      data-testid=${`fm-${side}-entry`}
      aria-selected=${selected ? 'true' : 'false'}
      @pointerdown=${(event: PointerEvent) => fm.pressRow(side, entry.name, event)}
      @click=${(event: MouseEvent) => {
        if (event.ctrlKey || event.metaKey || event.shiftKey) {
          return;
        }
        // A plain click on one of several selected rows narrows to it, once
        // it is clear the press was not the start of a drag.
        if (pane.selected.size > 1 && selected) {
          fm.select(side, entry.name, { ctrl: false, shift: false });
        }
      }}
      @dblclick=${() => fm.activate(side, entry.name)}
    >
      <td class="fm-col-name">
        <span class="fm-name">
          ${entry.is_dir ? ICON.folder : ICON.file}
          <span class="fm-name-text" dir="auto" title=${entry.name}>${entry.name}</span>
        </span>
      </td>
      <td class="fm-col-size">${entry.is_dir ? '' : formatSize(entry.size)}</td>
      <td class="fm-col-date">${formatModified(entry.modified_unix, locale)}</td>
    </tr>
  `;
}

/** The row whose name is being typed: a rename, or a new folder (`entry` null). */
function renderNameRow(
  fm: FileManager,
  side: Side,
  locale: Locale,
  entry: FileEntry | null,
): TemplateResult {
  const initial = entry ? entry.name : t(locale, 'fm.newFolder');
  let done = false;
  const commit = (input: HTMLInputElement): void => {
    if (done) {
      return;
    }
    done = true;
    void fm.commitEdit(side, input.value);
  };
  return html`
    <tr class="fm-row is-editing" data-testid=${`fm-${side}-editing`}>
      <td class="fm-col-name" colspan="3">
        <span class="fm-name">
          ${entry && !entry.is_dir ? ICON.file : ICON.folder}
          <input
            class="fm-name-input"
            data-testid=${`fm-${side}-name-input`}
            dir="auto"
            aria-label=${t(locale, entry ? 'fm.rename' : 'fm.newFolder')}
            spellcheck="false"
            autocomplete="off"
            .value=${initial}
            @keydown=${(event: KeyboardEvent) => {
              event.stopPropagation();
              if (event.key === 'Enter') {
                event.preventDefault();
                commit(event.target as HTMLInputElement);
              } else if (event.key === 'Escape') {
                done = true;
                fm.cancelEdit(side);
              }
            }}
            @blur=${(event: FocusEvent) => commit(event.target as HTMLInputElement)}
          />
        </span>
      </td>
    </tr>
  `;
}

function renderProblem(problem: TranslationKey, locale: Locale, side: Side): TemplateResult {
  return html`
    <div class="fm-problem" role="status" data-testid=${`fm-${side}-problem`}>
      <span class="fm-problem-icon" aria-hidden="true">${ICON.lock}</span>
      <p>${t(locale, problem)}</p>
    </div>
  `;
}

function renderTransfers(fm: FileManager, locale: Locale): TemplateResult {
  const offers = fm.transfers.offers.filter((offer) => offer.peer_label === fm.peer);
  const rows = fm.rows();
  const byTransfer = new Set(rows.map((row) => row.transfer_id));
  // A job shows as its own row until it has a transfer row to show instead.
  const jobs = fm.queue.jobs.filter(
    (job) => job.transferId === null || !byTransfer.has(job.transferId),
  );
  const active = rows.filter((row) => row.state === 'running').length +
    jobs.filter((job) => job.state === 'queued' || job.state === 'starting').length;
  const finished =
    rows.some((row) => row.state !== 'running') ||
    jobs.some((job) => job.state === 'done' || job.state === 'failed' || job.state === 'cancelled');
  const empty = offers.length === 0 && rows.length === 0 && jobs.length === 0;

  return html`
    <section class="fm-transfers" aria-label=${t(locale, 'fm.transfers')} data-testid="fm-transfers">
      <header class="fm-transfers-head">
        <h2>${t(locale, 'fm.transfers')}</h2>
        ${active > 0 ? html`<span class="fm-count">${active}</span>` : nothing}
        <button
          type="button"
          class="fm-link"
          data-testid="fm-clear-finished"
          ?disabled=${!finished}
          @click=${() => fm.clearFinished()}
        >
          ${t(locale, 'fm.clearFinished')}
        </button>
      </header>
      ${empty
        ? html`<p class="fm-transfers-empty">${t(locale, 'fm.noTransfers')}</p>`
        : html`
            <ul class="fm-xfers" aria-live="polite">
              ${offers.map(
                (offer) => html`
                  <li class="fm-xfer is-offer" data-testid="file-offer">
                    <span class="fm-xfer-dir" aria-hidden="true">${ICON.arrowDown}</span>
                    <span class="fm-xfer-name" dir="auto" title=${offer.name}>${offer.name}</span>
                    <span class="fm-xfer-meta">${t(locale, 'fm.incoming')} · ${formatSize(offer.size)}</span>
                    <span class="fm-xfer-actions">
                      <button
                        type="button"
                        class="fm-btn is-primary is-small"
                        data-testid="file-accept"
                        aria-label=${`${t(locale, 'files.accept')}: ${offer.name}`}
                        @click=${() => fm.answerOffer(true, offer.from_clipboard)}
                      >
                        ${t(locale, 'files.accept')}
                      </button>
                      <button
                        type="button"
                        class="fm-btn is-small"
                        data-testid="file-decline"
                        aria-label=${`${t(locale, 'files.decline')}: ${offer.name}`}
                        @click=${() => fm.answerOffer(false, offer.from_clipboard)}
                      >
                        ${t(locale, 'files.decline')}
                      </button>
                    </span>
                  </li>
                `,
              )}
              ${jobs.map((job) => renderJob(fm, job, locale))}
              ${rows.map((row) => renderTransferRow(fm, row, locale))}
            </ul>
          `}
    </section>
  `;
}

function renderJob(fm: FileManager, job: Job, locale: Locale): TemplateResult {
  const state =
    job.state === 'queued'
      ? t(locale, 'fm.queued')
      : job.state === 'starting'
        ? t(locale, 'fm.preparing')
        : job.state === 'done'
          ? t(locale, 'files.state.completed')
          : job.state === 'cancelled'
            ? t(locale, 'files.state.cancelled')
            : job.state === 'failed'
              ? t(locale, JOB_FAILURE_KEY[job.failure ?? 'failed'] ?? 'files.state.failed')
              : '';
  return html`
    <li class="fm-xfer is-${job.state}" data-testid="fm-job">
      <span class="fm-xfer-dir" aria-label=${t(locale, job.direction === 'upload' ? 'files.outgoing' : 'files.incoming')}
        >${job.direction === 'upload' ? ICON.arrowUp : ICON.arrowDown}</span
      >
      <span class="fm-xfer-name" dir="auto" title=${job.source}>
        ${job.isDir ? ICON.folder : nothing}${job.name}
      </span>
      <span class="fm-xfer-meta">${job.size > 0 ? formatSize(job.size) : ''}</span>
      <span class="fm-xfer-state ${job.state === 'failed' ? 'is-error' : ''}" data-testid="fm-job-state"
        >${state}</span
      >
      <span class="fm-xfer-actions">
        ${job.state === 'queued'
          ? html`<button
              type="button"
              class="fm-icon-btn is-small"
              aria-label=${`${t(locale, 'files.cancel')}: ${job.name}`}
              title=${t(locale, 'files.cancel')}
              @click=${() => fm.cancelJob(job.id)}
            >
              ${ICON.close}
            </button>`
          : nothing}
      </span>
    </li>
  `;
}

function renderTransferRow(fm: FileManager, row: TransferRow, locale: Locale): TemplateResult {
  const done = row.size === 0 ? 100 : Math.min(100, Math.round((row.moved / row.size) * 100));
  const speed = fm.speed(row.transfer_id);
  return html`
    <li class="fm-xfer is-${row.state}" data-testid="file-transfer">
      <span class="fm-xfer-dir" aria-label=${t(locale, row.incoming ? 'files.incoming' : 'files.outgoing')}
        >${row.incoming ? ICON.arrowDown : ICON.arrowUp}</span
      >
      <span class="fm-xfer-name" dir="auto" title=${row.name}>
        ${row.directory ? ICON.folder : nothing}${row.name}
      </span>
      ${row.state === 'running'
        ? html`<progress
              class="fm-progress"
              max="100"
              .value=${done}
              aria-label=${`${row.name}: ${done}%`}
              data-testid="file-progress"
            ></progress>
            <span class="fm-xfer-meta"
              >${formatSize(row.moved)} / ${formatSize(row.size)}${speed > 0
                ? html` · ${formatSize(speed)}/s`
                : nothing}</span
            >`
        : html`<span class="fm-xfer-meta">${formatSize(row.size)}</span>
            <span
              class="fm-xfer-state ${row.state === 'failed' ? 'is-error' : row.state === 'completed' ? 'is-ok' : ''}"
              data-testid="file-state"
              >${t(
                locale,
                row.state === 'completed'
                  ? 'files.state.completed'
                  : row.state === 'cancelled'
                    ? 'files.state.cancelled'
                    : 'files.state.failed',
              )}</span
            >`}
      <span class="fm-xfer-actions">
        ${row.state === 'running'
          ? html`<button
              type="button"
              class="fm-icon-btn is-small"
              data-testid="file-cancel"
              aria-label=${`${t(locale, 'files.cancel')}: ${row.name}`}
              title=${t(locale, 'files.cancel')}
              @click=${() => fm.cancelTransfer(row.transfer_id)}
            >
              ${ICON.close}
            </button>`
          : nothing}
      </span>
    </li>
  `;
}

function renderNotice(fm: FileManager): TemplateResult | typeof nothing {
  const notice = fm.notice;
  if (!notice) {
    return nothing;
  }
  return html`
    <div
      class="fm-notice ${notice.error ? 'is-error' : ''}"
      role=${notice.error ? 'alert' : 'status'}
      data-testid="fm-notice"
    >
      <span>${notice.text}</span>
      <button
        type="button"
        class="fm-icon-btn is-small"
        aria-label="×"
        @click=${() => fm.dismissNotice()}
      >
        ${ICON.close}
      </button>
    </div>
  `;
}

function renderMenu(fm: FileManager, locale: Locale): TemplateResult | typeof nothing {
  const menu = fm.menu;
  if (!menu) {
    return nothing;
  }
  const side = menu.side;
  const pane = fm.pane(side);
  const selection = fm.selection(side);
  const single = selection.length === 1 ? selection[0] : undefined;
  const usable = Boolean(pane.path) && pane.problem === null;
  const item = (
    label: TranslationKey,
    shortcut: string,
    enabled: boolean,
    run: () => void,
    danger = false,
  ): TemplateResult => html`
    <button
      type="button"
      role="menuitem"
      class="fm-menu-item ${danger ? 'is-danger' : ''}"
      ?disabled=${!enabled}
      @click=${() => {
        fm.closeMenu();
        run();
      }}
    >
      <span>${t(locale, label)}</span><kbd>${shortcut}</kbd>
    </button>
  `;
  // Kept inside the window: a menu opened near the right or bottom edge
  // opens towards the middle instead of off the edge.
  const left = Math.min(menu.x, window.innerWidth - 230);
  const top = Math.min(menu.y, window.innerHeight - 300);
  return html`
    <div class="fm-menu-scrim" @pointerdown=${() => fm.closeMenu()} @contextmenu=${(event: Event) => {
      event.preventDefault();
      fm.closeMenu();
    }}></div>
    <div
      class="fm-menu"
      role="menu"
      data-testid="fm-menu"
      style=${`left:${Math.max(4, left)}px;top:${Math.max(4, top)}px`}
    >
      ${single?.is_dir
        ? item('fm.open', 'Enter', true, () => fm.activate(side, single.name))
        : nothing}
      ${item(
        side === 'local' ? 'fileManager.upload' : 'fileManager.download',
        '',
        fm.canTransfer() && selection.length > 0,
        () => fm.transferSelection(side),
      )}
      <div class="fm-menu-sep" role="separator"></div>
      ${item('fm.copy', 'Ctrl+C', selection.length > 0, () => fm.copy(side))}
      ${item('fm.paste', 'Ctrl+V', fm.canPaste(side), () => fm.paste(side))}
      <div class="fm-menu-sep" role="separator"></div>
      ${item('fm.newFolder', 'F7', usable, () => fm.startCreate(side))}
      ${item('fm.rename', 'F2', usable && selection.length === 1, () => fm.startRename(side))}
      ${item('fm.delete', 'Del', usable && selection.length > 0, () => fm.askDelete(side), true)}
      <div class="fm-menu-sep" role="separator"></div>
      ${item('fm.refresh', 'F5', Boolean(pane.path), () => fm.refresh(side))}
    </div>
  `;
}

function renderConfirm(fm: FileManager, locale: Locale): TemplateResult | typeof nothing {
  const confirming = fm.confirming;
  if (!confirming) {
    return nothing;
  }
  const [first] = confirming.names;
  const body =
    confirming.names.length === 1 && first !== undefined
      ? t(locale, 'fm.confirmDelete.one', first)
      : t(locale, 'fm.confirmDelete.many', String(confirming.names.length));
  return html`
    <div class="fm-scrim" @pointerdown=${(event: Event) => {
      if (event.target === event.currentTarget) {
        fm.cancelDelete();
      }
    }}>
      <div
        class="fm-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="fm-confirm-title"
        aria-describedby="fm-confirm-body"
        data-testid="fm-confirm"
      >
        <h2 id="fm-confirm-title">${t(locale, 'fm.confirmDelete.title')}</h2>
        <p id="fm-confirm-body">${body}</p>
        <div class="fm-dialog-actions">
          <button type="button" class="fm-btn" data-testid="fm-confirm-cancel" @click=${() => fm.cancelDelete()}>
            ${t(locale, 'fm.cancel')}
          </button>
          <button
            type="button"
            class="fm-btn is-danger-solid"
            data-testid="fm-confirm-delete"
            @click=${() => void fm.confirmDelete()}
          >
            ${t(locale, 'fm.delete')}
          </button>
        </div>
      </div>
    </div>
  `;
}

function renderGhost(fm: FileManager, locale: Locale): TemplateResult | typeof nothing {
  const drag = fm.drag;
  if (!drag?.moving) {
    return nothing;
  }
  const [first] = drag.names;
  const label =
    drag.names.length === 1 && first !== undefined ? first : t(locale, 'fm.selected', String(drag.names.length));
  return html`
    <div
      class="fm-ghost ${fm.dropTarget ? 'is-over' : ''}"
      aria-hidden="true"
      style=${`left:${drag.x + 14}px;top:${drag.y + 10}px`}
    >
      ${drag.side === 'local' ? ICON.arrowUp : ICON.arrowDown}<span>${label}</span>
    </div>
  `;
}
