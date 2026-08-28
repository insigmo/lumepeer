// The audit log panel (§15; ADR 0041).
//
// The store is Rust; what these tests pin down is the half §15 asks for on
// top of it — that the log can be read, filtered, taken away and erased, that
// erasing asks first, and that a host with no log says so instead of showing
// an empty table that looks like "nothing happened".
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  auditPanel,
  onAuditStateChange,
  resetAuditPanel,
  type AuditCommands,
  type AuditRow,
} from './audit-log';
import { t } from './i18n';

const rows: AuditRow[] = [
  { at_unix_secs: 1_700_000_100, peer: 'ab12cd34', kind: 'consent_granted', detail: 'full_control' },
  { at_unix_secs: 1_700_000_000, peer: 'ab12cd34', kind: 'consent_requested', detail: 'view_only' },
];

let container: HTMLElement;
let commands: AuditCommands;
let listMock: ReturnType<typeof vi.fn>;
let exportMock: ReturnType<typeof vi.fn>;
let clearMock: ReturnType<typeof vi.fn>;

/** Re-renders on every state change, exactly as main.ts wires it. */
function mount(): void {
  const paint = (): void => {
    render(auditPanel('en', commands), container);
  };
  onAuditStateChange(paint);
  paint();
}

/** Lets the panel's own fetch settle. */
async function settle(): Promise<void> {
  for (let i = 0; i < 10; i += 1) {
    await Promise.resolve();
  }
}

beforeEach(() => {
  resetAuditPanel();
  container = document.createElement('div');
  document.body.appendChild(container);
  listMock = vi.fn().mockResolvedValue(rows);
  exportMock = vi.fn().mockResolvedValue('/home/host/lumepeer-audit.csv');
  clearMock = vi.fn().mockResolvedValue(2);
  commands = {
    status: vi.fn().mockResolvedValue(true),
    kinds: vi.fn().mockResolvedValue(['consent_requested', 'consent_granted']),
    list: listMock as unknown as AuditCommands['list'],
    export: exportMock as unknown as AuditCommands['export'],
    clear: clearMock as unknown as AuditCommands['clear'],
  };
});

afterEach(() => {
  container.remove();
  resetAuditPanel();
});

describe('audit log panel', () => {
  it('renders one row per record, naming the peer by its hash prefix', async () => {
    mount();
    await settle();
    const table = container.querySelectorAll('[data-testid="audit-row"]');
    expect(table).toHaveLength(2);
    expect(container.textContent).toContain('ab12cd34');
    expect(container.textContent).toContain(t('en', 'audit.kind.consent_granted'));
  });

  it('asks the core with the filter the host set, day-inclusive at both ends', async () => {
    mount();
    await settle();

    const from = container.querySelector('[data-testid="audit-from"]') as HTMLInputElement;
    const to = container.querySelector('[data-testid="audit-to"]') as HTMLInputElement;
    const kind = container.querySelector('[data-testid="audit-kind"]') as HTMLSelectElement;
    from.value = '2026-08-01';
    from.dispatchEvent(new Event('change'));
    to.value = '2026-08-02';
    to.dispatchEvent(new Event('change'));
    kind.value = 'consent_granted';
    kind.dispatchEvent(new Event('change'));
    (container.querySelector('[data-testid="audit-apply"]') as HTMLButtonElement).click();
    await settle();

    const [since, until, chosen] = listMock.mock.calls.at(-1) as [number, number, string];
    expect(chosen).toBe('consent_granted');
    // The "to" bound covers the whole named day, or a filter ending today
    // would hide everything that happened today.
    expect(until - since).toBe(2 * 24 * 60 * 60 - 1);
  });

  it('says so when this host keeps no log at all', async () => {
    commands.status = vi.fn().mockResolvedValue(false);
    listMock.mockResolvedValue([]);
    mount();
    await settle();
    expect(container.querySelector('[data-testid="audit-disabled"]')).not.toBeNull();
    // The export and the purge are pointless without a log, and an enabled
    // button that does nothing is exactly what §18 rules out.
    expect(
      (container.querySelector('[data-testid="audit-export"]') as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it('never erases on the first press', async () => {
    mount();
    await settle();
    (container.querySelector('[data-testid="audit-clear"]') as HTMLButtonElement).click();
    await settle();
    expect(clearMock).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="audit-confirm"]')).not.toBeNull();

    (container.querySelector('[data-testid="audit-clear-cancel"]') as HTMLButtonElement).click();
    await settle();
    expect(clearMock).not.toHaveBeenCalled();
  });

  it('erases once confirmed, and reports how many records went', async () => {
    mount();
    await settle();
    (container.querySelector('[data-testid="audit-clear"]') as HTMLButtonElement).click();
    await settle();
    (container.querySelector('[data-testid="audit-clear-confirm"]') as HTMLButtonElement).click();
    await settle();

    expect(clearMock).toHaveBeenCalledTimes(1);
    expect(container.querySelector('[data-testid="audit-row"]')).toBeNull();
    expect(container.querySelector('[data-testid="audit-notice"]')?.textContent).toContain('2');
  });

  it('exports through the core and shows where it landed', async () => {
    mount();
    await settle();
    (container.querySelector('[data-testid="audit-export"]') as HTMLButtonElement).click();
    await settle();

    expect(exportMock).toHaveBeenCalledTimes(1);
    // The panel never names a path: it only shows the one Rust chose.
    expect(exportMock).toHaveBeenCalledWith();
    expect(container.querySelector('[data-testid="audit-notice"]')?.textContent).toContain(
      '/home/host/lumepeer-audit.csv',
    );
  });

  it('says the log could not be read rather than showing an empty one', async () => {
    listMock.mockRejectedValue(new Error('database is locked'));
    vi.spyOn(console, 'error').mockImplementation(() => {});
    mount();
    await settle();
    expect(container.querySelector('[data-testid="audit-notice"]')?.textContent).toBe(
      t('en', 'audit.loadFailed'),
    );
  });
});
