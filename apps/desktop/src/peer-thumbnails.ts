// The last picture of each remote desktop, so the connection list can show a
// host by what it looks like instead of by an operating-system logo (ADR 0094).
//
// Written by the view window while a session is on screen, read by the
// connection list in the main window. Both are pages of the same webview
// origin, so `localStorage` is the whole mechanism and no IPC command exists
// for this: the picture is already in the webview, it is only ever shown back
// to the person who watched it, and handing it to the actor would make the
// trusted side store screen content it has no other reason to hold (§2.3,
// §15).
//
// Keyed by the *stable* host pseudonym the remembered-hosts list uses, which
// the actor puts in the view window's URL — the per-session label is re-salted
// on every run and would lose the picture at every restart.
//
// Everything here is best-effort. Storage can be full, disabled or cleared
// between two calls; a connection list with no picture is the fallback and is
// never worth an error.

/** Prefix every stored thumbnail shares, so pruning can find them all. */
const PREFIX = 'lumepeer.thumb.';

/**
 * How many hosts keep a picture.
 *
 * Well under the connection list's own fifty rows: the older a row is, the
 * less a picture of it is worth, and `localStorage` is a few megabytes for
 * the whole origin — shared with everything else this app keeps there.
 */
const MAX_ENTRIES = 12;

/** Width every stored thumbnail is scaled to; the height follows the frame. */
export const THUMBNAIL_WIDTH = 256;

/** JPEG quality. Low on purpose: this is a 256px-wide picture of a desktop. */
const THUMBNAIL_QUALITY = 0.55;

/** One stored picture, and when it was taken. */
interface StoredThumbnail {
  /** `Date.now()` of the capture, used to decide what falls off the end. */
  at: number;
  /** `data:image/jpeg;base64,…`, ready to be a `src`. */
  image: string;
}

function storage(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    // Storage disabled by policy: the feature is simply absent.
    return null;
  }
}

/**
 * Scales `source` down to a thumbnail and returns it as a data URL, or `null`
 * if the canvas has nothing on it yet or the browser refuses to read it.
 */
export function thumbnailFrom(source: HTMLCanvasElement, width = THUMBNAIL_WIDTH): string | null {
  if (source.width === 0 || source.height === 0) {
    return null;
  }
  try {
    const scale = Math.min(1, width / source.width);
    const target = document.createElement('canvas');
    target.width = Math.max(1, Math.round(source.width * scale));
    target.height = Math.max(1, Math.round(source.height * scale));
    const context = target.getContext('2d');
    if (!context) {
      return null;
    }
    context.drawImage(source, 0, 0, target.width, target.height);
    return target.toDataURL('image/jpeg', THUMBNAIL_QUALITY);
  } catch {
    // A canvas the browser considers tainted, or one with no backing surface.
    return null;
  }
}

/** The stored picture of `host`, or `null` when there is none. */
export function thumbnailOf(host: string): string | null {
  const store = storage();
  if (!store || host === '') {
    return null;
  }
  try {
    const raw = store.getItem(PREFIX + host);
    if (raw === null) {
      return null;
    }
    const parsed: unknown = JSON.parse(raw);
    const image = (parsed as StoredThumbnail | null)?.image;
    return typeof image === 'string' && image.startsWith('data:image/') ? image : null;
  } catch {
    return null;
  }
}

/**
 * Keeps `image` as the picture of `host`, dropping the oldest ones once more
 * than [`MAX_ENTRIES`] hosts have one.
 */
export function rememberThumbnail(host: string, image: string | null): void {
  const store = storage();
  if (!store || host === '' || image === null) {
    return;
  }
  const entry: StoredThumbnail = { at: Date.now(), image };
  try {
    store.setItem(PREFIX + host, JSON.stringify(entry));
    prune(store);
  } catch {
    // Quota, most likely. Make room by dropping everything this module owns
    // and keep only the picture being written — a list whose pictures are all
    // missing is worse than one whose oldest are.
    try {
      for (const key of keys(store)) {
        store.removeItem(key);
      }
      store.setItem(PREFIX + host, JSON.stringify(entry));
    } catch {
      // Storage is unusable: no picture, no error.
    }
  }
}

/**
 * Drops the picture of `host` — what a row being removed or its password
 * being forgotten has to take with it, so nothing outlives the row that
 * explained it.
 */
export function forgetThumbnail(host: string): void {
  const store = storage();
  if (!store) {
    return;
  }
  try {
    store.removeItem(PREFIX + host);
  } catch {
    // Nothing to do: the picture is either gone or unreachable, and both
    // read the same from here.
  }
}

function keys(store: Storage): string[] {
  const out: string[] = [];
  for (let i = 0; i < store.length; i += 1) {
    const key = store.key(i);
    if (key !== null && key.startsWith(PREFIX)) {
      out.push(key);
    }
  }
  return out;
}

function prune(store: Storage): void {
  const stored = keys(store);
  if (stored.length <= MAX_ENTRIES) {
    return;
  }
  const dated = stored
    .map((key) => {
      let at = 0;
      try {
        at = (JSON.parse(store.getItem(key) ?? '{}') as StoredThumbnail).at ?? 0;
      } catch {
        // Unparseable: treat it as the oldest there is, so it goes first.
      }
      return { key, at };
    })
    .sort((a, b) => b.at - a.at);
  for (const { key } of dated.slice(MAX_ENTRIES)) {
    store.removeItem(key);
  }
}
