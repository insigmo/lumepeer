// Chat panel unit tests (design doc §9.2).
//
// The panel's decisions live in `chat.ts`: transcript mirroring, the render
// shape and the poll loop. The Rust side validates every message against
// §9.2; these tests pin the UI contract around it.

import { describe, expect, it, vi } from 'vitest';

import { ChatState, startChatPolling, type ChatCommands, type ChatRow } from './chat';

describe('ChatState', () => {
  it('mirrors the authoritative transcript verbatim', () => {
    const state = new ChatState();
    const rows: ChatRow[] = [
      { outgoing: true, text: 'hello', atUnix: 100 },
      { outgoing: false, text: 'привет', atUnix: 105 },
    ];
    state.replace(rows);
    expect(state.transcript).toEqual(rows);
  });

  it('starts empty', () => {
    expect(new ChatState().transcript).toHaveLength(0);
  });
});

describe('startChatPolling', () => {
  it('polls the transcript repeatedly until stopped', async () => {
    vi.useFakeTimers();
    const rows: ChatRow[] = [{ outgoing: false, text: 'from host', atUnix: 1 }];
    const commands: ChatCommands = {
      chatTranscript: vi.fn().mockResolvedValue(rows),
      chatSend: vi.fn(),
    };
    const container = document.createElement('div');
    const stop = startChatPolling(container, new ChatState(), 'en', 'abc123', commands, 50);

    // Initial poll runs immediately.
    await vi.advanceTimersByTimeAsync(0);
    expect(commands.chatTranscript).toHaveBeenCalledTimes(1);
    expect(container.querySelector('.chat-log')).not.toBeNull();
    expect(container.textContent).toContain('from host');

    await vi.advanceTimersByTimeAsync(120);
    expect(commands.chatTranscript).toHaveBeenCalledTimes(3); // t=0, 50, 100

    stop();
    await vi.advanceTimersByTimeAsync(500);
    expect(commands.chatTranscript).toHaveBeenCalledTimes(3); // no growth after stop
    vi.useRealTimers();
  });

  it('stops polling when the session is gone instead of spinning on errors', async () => {
    vi.useFakeTimers();
    const commands: ChatCommands = {
      chatTranscript: vi.fn().mockRejectedValue(new Error('session gone')),
      chatSend: vi.fn(),
    };
    const container = document.createElement('div');
    const stop = startChatPolling(container, new ChatState(), 'en', 'abc123', commands, 50);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(500);
    // One attempt, then the loop went quiet.
    expect(commands.chatTranscript).toHaveBeenCalledTimes(1);
    stop();
    vi.useRealTimers();
  });
});
