// When the main window asks which remembered hosts are there (ADR 0127).
//
// The webview decides *when* and nothing else. Which hosts are dialed, over
// which transport, and what their answers mean is the actor's, and the answers
// come back in `connection_history` like every other fact about a row. What
// only this side knows is whether its window is the one in front — and every
// check is a real dial at every saved machine, so it runs while somebody is
// looking at the list, not all day behind other windows.

/** How often the hosts are asked again while the window stays in front. */
export const PRESENCE_RECHECK_MS = 5 * 60 * 1000;

/**
 * The least time between two checks brought on by the window coming to the
 * front. Switching back and forth between windows is not a new fact about
 * the hosts each time, and each check knocks on every one of them.
 */
export const PRESENCE_REFOCUS_GAP_MS = 30 * 1000;

/** Decides when to ask; the asking itself is `probe`. */
export class PresenceSchedule {
  private last = Number.NEGATIVE_INFINITY;

  constructor(
    private readonly probe: () => void,
    /** Whether the window is in front and this node can reach anything yet. */
    private readonly active: () => boolean,
    private readonly now: () => number = () => Date.now(),
  ) {}

  /** The window came to the front: ask now, unless a check just ran. */
  focused(): void {
    this.askIfOlderThan(PRESENCE_REFOCUS_GAP_MS);
  }

  /**
   * Called often; asks again once the last check is [`PRESENCE_RECHECK_MS`]
   * old and the window is still in front. It is also what catches up on a
   * window that came to the front before this node reached the network.
   */
  tick(): void {
    this.askIfOlderThan(PRESENCE_RECHECK_MS);
  }

  private askIfOlderThan(age: number): void {
    if (!this.active() || this.now() - this.last < age) {
      return;
    }
    this.last = this.now();
    this.probe();
  }
}

/** Asks the actor to check every remembered host (ADR 0127). */
export async function probeSavedHosts(): Promise<void> {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('history_probe');
  } catch (error) {
    console.error('history_probe failed:', error);
  }
}
