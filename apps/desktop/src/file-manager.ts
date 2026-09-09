// Two-pane file manager for a remote-view window (design doc §9.2, §18;
// ADR 0075, ADR 0076).
//
// The local pane is this machine, the remote pane is the host. Neither one
// moves a byte by itself: a download asks the host to offer the file it
// named, an upload offers a local file for the directory the remote pane is
// showing, and both then ride the transfer engine of §9.2 — same
// `rd/file/1` connection, same chunking, same BLAKE3 check before anything
// leaves staging. The progress and cancel a transfer needs are what
// `file-transfers.ts` already draws, so this panel renders that panel rather
// than growing a second one.
//
// Nothing here decides anything and nothing here touches a filesystem. Both
// listings, both paths and both transfers cross the IPC boundary as typed
// commands the Rust side authorizes: the host re-reads `file_browse` and
// `file_transfer` on every request that reaches it, and this window's own
// `capabilities/view.json` gives it no filesystem rights of its own (§2.3).
//
// A panel that cannot be used is not shown. When the host answers a listing
// with `not_granted` — never granted, or granted and then withdrawn
// mid-session — the panel and its toolbar button go away, rather than sitting
// there as a button that quietly does nothing.

import { html, type TemplateResult } from 'lit-html';

import { fileTransferPanel, formatSize, type FileCommands, type FileTransfers } from './file-transfers';
import type { Locale } from './i18n';
import { t } from './i18n';

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
}

/** How this panel talks to Tauri; injectable so the logic is testable. */
export interface FileManagerCommands {
  localList(path?: string): Promise<LocalDir>;
  remoteList(path: string): Promise<void>;
  remoteStatus(): Promise<RemoteDir>;
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
    async remoteList(path) {
      await call('remote_dir_list', { args: { peer, path } });
    },
    async remoteStatus() {
      return (await call('remote_dir_status', { peer })) as RemoteDir;
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
 * deliberately gains none for this: a listing request names an absolute path,
 * and which absolute path exists is exactly the fact a probe answers. A Unix
 * host answers the first; a Windows host refuses it as a path it will not
 * parse and answers the second. A host that answers neither leaves the path
 * box for the person to fill in, which is the honest end of a guess.
 */
export const REMOTE_ROOTS = ['/', 'C:\\'] as const;

/** What the panel shows besides the two listings. */
export interface FileManagerState {
  local: LocalDir | null;
  remote: RemoteDir | null;
  /** Which local entry is selected, by name, for the upload button. */
  localSelected: string | null;
  /** Which remote entry is selected, by name, for the download button. */
  remoteSelected: string | null;
  /** How many remote roots have been probed; see [`REMOTE_ROOTS`]. */
  probed: number;
  /** A local listing that failed, as its IPC code. */
  localError: string | null;
}

/** The state a freshly opened panel starts in. */
export function emptyState(): FileManagerState {
  return {
    local: null,
    remote: null,
    localSelected: null,
    remoteSelected: null,
    probed: 0,
    localError: null,
  };
}

/**
 * Whether the host has said this guest may not browse it (ADR 0076).
 *
 * The one condition that removes the panel instead of explaining itself: a
 * refusal for want of a grant is not something the person at this end can do
 * anything about from inside the panel.
 */
export function browseDenied(state: FileManagerState): boolean {
  return state.remote?.refused === 'not_granted';
}

const DIR_REFUSAL_KEY = {
  not_granted: 'fileManager.refused.notGranted',
  bad_path: 'fileManager.refused.badPath',
  unreadable: 'fileManager.refused.unreadable',
} as const;

const FETCH_REFUSAL_KEY = {
  not_granted: 'fileManager.download.notGranted',
  bad_path: 'fileManager.download.badPath',
  unreadable: 'fileManager.download.unreadable',
  too_many: 'fileManager.download.tooMany',
} as const;

/** Joins a directory and a name the way the *far* side would. */
export function joinPath(directory: string, name: string): string {
  const separator = directory.includes('\\') && !directory.includes('/') ? '\\' : '/';
  return directory.endsWith(separator) ? `${directory}${name}` : `${directory}${separator}${name}`;
}

function entryList(
  entries: readonly FileEntry[],
  selected: string | null,
  locale: Locale,
  testid: string,
  onOpen: (entry: FileEntry) => void,
  onSelect: (entry: FileEntry) => void,
): TemplateResult {
  return html`
    <ul class="fm-list" data-testid=${testid}>
      ${entries.map(
        (entry) => html`
          <li>
            <button
              type="button"
              class="fm-entry ${selected === entry.name ? 'is-selected' : ''}"
              data-testid="${testid}-entry"
              aria-pressed=${selected === entry.name ? 'true' : 'false'}
              @click=${() => (entry.is_dir ? onOpen(entry) : onSelect(entry))}
            >
              <span class="fm-name">${entry.name}</span>
              <span class="fm-size"
                >${entry.is_dir ? t(locale, 'fileManager.folder') : formatSize(entry.size)}</span
              >
            </button>
          </li>
        `,
      )}
    </ul>
  `;
}

/**
 * Renders both panes plus the transfer list they feed.
 *
 * Returns nothing at all when the host has refused for want of a grant: the
 * caller hides the panel on the same condition, and rendering an explanation
 * of a panel that is about to disappear would be the "button that quietly
 * does nothing" this exists to avoid.
 */
export function fileManagerPanel(
  peer: string,
  state: FileManagerState,
  transfers: FileTransfers,
  locale: Locale,
  commands: FileManagerCommands,
  fileCommands: FileCommands,
  onChange: () => void = () => {},
): TemplateResult {
  if (browseDenied(state)) {
    return html``;
  }

  const openLocal = (path: string): void => {
    void commands.localList(path).then(
      (local) => {
        state.local = local;
        state.localSelected = null;
        state.localError = null;
        onChange();
      },
      (error: unknown) => {
        state.localError = ipcCode(error);
        onChange();
      },
    );
  };
  const openRemote = (path: string): void => {
    void commands.remoteList(path).then(() => {
      state.remoteSelected = null;
      onChange();
    }, onChange);
  };

  const local = state.local;
  const remote = state.remote;
  const canDownload = Boolean(local && remote && state.remoteSelected);
  const canUpload = Boolean(local && remote && state.localSelected && !remote.refused);

  return html`
    <section class="fm-panel" data-testid="file-manager">
      <h3>${t(locale, 'fileManager.heading')}</h3>
      <div class="fm-panes">
        <section class="fm-pane" aria-label=${t(locale, 'fileManager.local')}>
          <h4>${t(locale, 'fileManager.local')}</h4>
          <p class="fm-path" data-testid="fm-local-path">${local?.path ?? ''}</p>
          <button
            type="button"
            data-testid="fm-local-up"
            ?disabled=${!local?.parent}
            @click=${() => local?.parent && openLocal(local.parent)}
          >
            ${t(locale, 'fileManager.up')}
          </button>
          ${local === null
            ? html`<p data-testid="fm-local-loading">${t(locale, 'fileManager.loading')}</p>`
            : entryList(
                local.entries,
                state.localSelected,
                locale,
                'fm-local',
                (entry) => openLocal(joinPath(local.path, entry.name)),
                (entry) => {
                  state.localSelected = entry.name;
                  onChange();
                },
              )}
          ${local !== null && local.entries.length === 0
            ? html`<p data-testid="fm-local-empty">${t(locale, 'fileManager.empty')}</p>`
            : ''}
          ${local?.truncated
            ? html`<p data-testid="fm-local-truncated">${t(locale, 'fileManager.truncated')}</p>`
            : ''}
          ${state.localError === null
            ? ''
            : html`<p data-testid="fm-local-error">${t(locale, 'fileManager.localUnreadable')}</p>`}
        </section>
        <section class="fm-pane" aria-label=${t(locale, 'fileManager.remote')}>
          <h4>${t(locale, 'fileManager.remote')}</h4>
          <p class="fm-path" data-testid="fm-remote-path">${remote?.path ?? ''}</p>
          <button
            type="button"
            data-testid="fm-remote-up"
            ?disabled=${!remote?.parent}
            @click=${() => remote?.parent && openRemote(remote.parent)}
          >
            ${t(locale, 'fileManager.up')}
          </button>
          ${remote === null || !remote.answered
            ? html`<p data-testid="fm-remote-loading">${t(locale, 'fileManager.loading')}</p>`
            : remote.refused !== null
              ? html`<p data-testid="fm-remote-refused">
                  ${t(locale, DIR_REFUSAL_KEY[remote.refused])}
                </p>`
              : entryList(
                  remote.entries,
                  state.remoteSelected,
                  locale,
                  'fm-remote',
                  (entry) => openRemote(joinPath(remote.path, entry.name)),
                  (entry) => {
                    state.remoteSelected = entry.name;
                    onChange();
                  },
                )}
          ${remote !== null && remote.answered && remote.refused === null && remote.entries.length === 0
            ? html`<p data-testid="fm-remote-empty">${t(locale, 'fileManager.empty')}</p>`
            : ''}
          ${remote?.truncated
            ? html`<p data-testid="fm-remote-truncated">${t(locale, 'fileManager.truncated')}</p>`
            : ''}
          ${remote?.fetch_refused
            ? html`<p data-testid="fm-download-refused">
                ${t(locale, FETCH_REFUSAL_KEY[remote.fetch_refused])}
              </p>`
            : ''}
        </section>
      </div>
      <div class="fm-actions">
        <button
          type="button"
          data-testid="fm-download"
          ?disabled=${!canDownload}
          @click=${() => {
            if (local && remote && state.remoteSelected) {
              void commands
                .download(joinPath(remote.path, state.remoteSelected), local.path)
                .then(onChange, onChange);
            }
          }}
        >
          ${t(locale, 'fileManager.download')}
        </button>
        <button
          type="button"
          data-testid="fm-upload"
          ?disabled=${!canUpload}
          @click=${() => {
            if (local && remote && state.localSelected) {
              void commands
                .upload(joinPath(local.path, state.localSelected), remote.path)
                .then(onChange, onChange);
            }
          }}
        >
          ${t(locale, 'fileManager.upload')}
        </button>
      </div>
      ${fileTransferPanel(peer, transfers, locale, fileCommands, onChange)}
    </section>
  `;
}

/** The `code` of an `IpcError`, or an empty string for anything else. */
function ipcCode(error: unknown): string {
  return typeof error === 'object' && error !== null && 'code' in error
    ? String((error as { code: unknown }).code)
    : '';
}

/**
 * Refreshes both panes once.
 *
 * The remote half is a poll rather than a reply: `remote_dir_list` is fire
 * and forget on the actor, exactly like every other guest-to-host request on
 * the control channel, so the answer is read back here. The probe of
 * [`REMOTE_ROOTS`] is what gives the pane a directory to start in without a
 * message on the wire that asks the host where it keeps its files.
 */
export async function refresh(
  state: FileManagerState,
  commands: FileManagerCommands,
): Promise<void> {
  if (state.local === null && state.localError === null) {
    try {
      state.local = await commands.localList();
    } catch (error: unknown) {
      state.localError = ipcCode(error);
    }
  }
  state.remote = await commands.remoteStatus();
  if (browseDenied(state)) {
    return;
  }
  // A root that was refused as a path this host will not parse is the wrong
  // operating system's root, so try the next one. A refusal for any other
  // reason is the host's answer and stands.
  const unanswered = !state.remote.answered;
  const wrongRoot = state.remote.refused === 'bad_path' || state.remote.refused === 'unreadable';
  if ((unanswered || wrongRoot) && state.probed < REMOTE_ROOTS.length) {
    const root = REMOTE_ROOTS[state.probed];
    state.probed += 1;
    if (root !== undefined) {
      await commands.remoteList(root);
    }
  }
}
