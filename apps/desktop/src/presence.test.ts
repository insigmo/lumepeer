// ADR 0127: the saved hosts are checked when the main window comes to the
// front, and again every five minutes only while it stays there.
import { describe, expect, it, vi } from 'vitest';

import { PRESENCE_RECHECK_MS, PRESENCE_REFOCUS_GAP_MS, PresenceSchedule } from './presence';

function schedule(active: { value: boolean }) {
  let now = 0;
  const probe = vi.fn();
  const presence = new PresenceSchedule(
    probe,
    () => active.value,
    () => now,
  );
  return {
    presence,
    probe,
    advance(ms: number) {
      now += ms;
    },
  };
}

describe('PresenceSchedule', () => {
  it('checks as soon as the window comes to the front', () => {
    const { presence, probe } = schedule({ value: true });
    presence.focused();
    expect(probe).toHaveBeenCalledTimes(1);
  });

  it('checks again every five minutes while the window stays in front', () => {
    const { presence, probe, advance } = schedule({ value: true });
    presence.focused();
    advance(PRESENCE_RECHECK_MS - 1);
    presence.tick();
    expect(probe).toHaveBeenCalledTimes(1);
    advance(1);
    presence.tick();
    expect(probe).toHaveBeenCalledTimes(2);
  });

  it('asks nothing while the window is behind other windows', () => {
    const active = { value: true };
    const { presence, probe, advance } = schedule(active);
    presence.focused();
    active.value = false;
    advance(PRESENCE_RECHECK_MS * 3);
    presence.tick();
    presence.focused();
    expect(probe).toHaveBeenCalledTimes(1);
  });

  it('does not knock again for every switch back to the window', () => {
    const { presence, probe, advance } = schedule({ value: true });
    presence.focused();
    advance(PRESENCE_REFOCUS_GAP_MS - 1);
    presence.focused();
    expect(probe).toHaveBeenCalledTimes(1);
    advance(1);
    presence.focused();
    expect(probe).toHaveBeenCalledTimes(2);
  });

  it('catches up on a window that came to the front before the network was ready', () => {
    const active = { value: false };
    const { presence, probe } = schedule(active);
    presence.focused();
    expect(probe).not.toHaveBeenCalled();
    active.value = true;
    presence.tick();
    expect(probe).toHaveBeenCalledTimes(1);
  });
});
