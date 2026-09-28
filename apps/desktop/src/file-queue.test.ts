// The file manager's transfer queue (ADR 0124).
//
// The actor says nothing about a request that went nowhere, so the queue is
// what turns "asked" into "done" or into a reason. These tests pin the rules
// it runs on: one job at a time, a job's row is the new one with its name, a
// refusal is a count that moved, a folder with no files ends without a row,
// and a full actor queue is a wait rather than a failure.
import { describe, expect, it, vi } from 'vitest';

import {
  baseName,
  DIR_START_TIMEOUT_MS,
  FILE_START_TIMEOUT_MS,
  TransferQueue,
  type QueueCommands,
  type RefusalCounts,
} from './file-queue';
import type { FileTransfers, TransferRow } from './file-transfers';

const PEER = 'host-ab12';

function commands(over: Partial<QueueCommands> = {}): QueueCommands {
  return {
    download: vi.fn().mockResolvedValue(undefined),
    upload: vi.fn().mockResolvedValue(undefined),
    ...over,
  };
}

function row(over: Partial<TransferRow>): TransferRow {
  return {
    peer_label: PEER,
    transfer_id: 1,
    name: 'a.txt',
    size: 10,
    moved: 0,
    incoming: false,
    state: 'running',
    from_clipboard: false,
    directory: false,
    ...over,
  };
}

function transfers(...rows: TransferRow[]): FileTransfers {
  return { offers: [], transfers: rows };
}

const NO_REFUSALS: RefusalCounts = { fetches_refused: 0, uploads_refused: 0, fetch_refused: null };

describe('TransferQueue', () => {
  it('hands the actor one job at a time, in the order they were queued', async () => {
    const cmd = commands();
    const queue = new TransferQueue(PEER, cmd);
    queue.add(
      'upload',
      [
        { source: 'C:\\in\\a.txt', isDir: false, size: 10 },
        { source: 'C:\\in\\b.txt', isDir: false, size: 20 },
      ],
      '/srv',
    );

    await queue.pump(transfers(), NO_REFUSALS);
    expect(cmd.upload).toHaveBeenCalledTimes(1);
    expect(cmd.upload).toHaveBeenCalledWith('C:\\in\\a.txt', '/srv');

    // Its row appears and runs: the second job still waits.
    await queue.pump(transfers(row({ transfer_id: 7, name: 'a.txt' })), NO_REFUSALS);
    expect(cmd.upload).toHaveBeenCalledTimes(1);
    expect(queue.jobs[0]?.state).toBe('running');

    // Done: the next one goes, and the finished job is reported once.
    const finished = await queue.pump(
      transfers(row({ transfer_id: 7, name: 'a.txt', state: 'completed', moved: 10 })),
      NO_REFUSALS,
    );
    expect(finished.map((job) => job.name)).toEqual(['a.txt']);
    expect(cmd.upload).toHaveBeenCalledTimes(2);
    expect(cmd.upload).toHaveBeenLastCalledWith('C:\\in\\b.txt', '/srv');
  });

  it('takes the new row with its name, never an older one with the same name', async () => {
    const queue = new TransferQueue(PEER, commands());
    const old = row({ transfer_id: 3, name: 'a.txt', incoming: true, state: 'completed' });
    queue.add('download', [{ source: '/srv/a.txt', isDir: false, size: 10 }], 'C:\\in');

    await queue.pump(transfers(old), NO_REFUSALS);
    await queue.pump(transfers(old), NO_REFUSALS);
    expect(queue.jobs[0]?.state).toBe('starting');

    await queue.pump(transfers(old, row({ transfer_id: 4, name: 'a.txt', incoming: true })), NO_REFUSALS);
    expect(queue.jobs[0]?.transferId).toBe(4);
  });

  it('fails a download the host refused, with the host reason', async () => {
    const queue = new TransferQueue(PEER, commands());
    queue.add('download', [{ source: '/srv/secret', isDir: false, size: 1 }], 'C:\\in');
    await queue.pump(transfers(), NO_REFUSALS);

    await queue.pump(transfers(), { fetches_refused: 1, uploads_refused: 0, fetch_refused: 'not_granted' });
    expect(queue.jobs[0]?.state).toBe('failed');
    expect(queue.jobs[0]?.failure).toBe('not_granted');
  });

  it('ends a folder with no files in it as done, and gives a file far longer', async () => {
    let now = 1_000;
    const queue = new TransferQueue(PEER, commands(), () => now);
    queue.add(
      'upload',
      [
        { source: 'C:\\in\\empty', isDir: true, size: 0 },
        { source: 'C:\\in\\huge.iso', isDir: false, size: 50e9 },
      ],
      '/srv',
    );
    await queue.pump(transfers(), NO_REFUSALS);

    now += DIR_START_TIMEOUT_MS + 1;
    await queue.pump(transfers(), NO_REFUSALS);
    expect(queue.jobs[0]?.state).toBe('done');
    expect(queue.jobs[1]?.state).toBe('starting');

    // A large file is still being hashed long after a folder would have
    // been given up on.
    now += DIR_START_TIMEOUT_MS * 3;
    await queue.pump(transfers(), NO_REFUSALS);
    expect(queue.jobs[1]?.state).toBe('starting');

    now += FILE_START_TIMEOUT_MS;
    await queue.pump(transfers(), NO_REFUSALS);
    expect(queue.jobs[1]?.state).toBe('failed');
    expect(queue.jobs[1]?.failure).toBe('no_answer');
  });

  it('waits and asks again when the actor holds as many downloads as it will', async () => {
    const download = vi
      .fn()
      .mockRejectedValueOnce({ code: 'CORE', message: 'action not permitted by current grants' })
      .mockResolvedValue(undefined);
    const queue = new TransferQueue(PEER, commands({ download }));
    queue.add('download', [{ source: '/srv/a.txt', isDir: false, size: 1 }], 'C:\\in');

    await queue.pump(transfers(), NO_REFUSALS);
    expect(queue.jobs[0]?.state).toBe('queued');
    await queue.pump(transfers(), NO_REFUSALS);
    expect(queue.jobs[0]?.state).toBe('starting');
    expect(download).toHaveBeenCalledTimes(2);
  });

  it('says a host too old for it is too old, rather than that it failed', async () => {
    const upload = vi.fn().mockRejectedValue({ code: 'PEER_TOO_OLD', message: '' });
    const queue = new TransferQueue(PEER, commands({ upload }));
    queue.add('upload', [{ source: '/home/a', isDir: false, size: 1 }], 'C:\\');
    await queue.pump(transfers(), NO_REFUSALS);
    expect(queue.jobs[0]?.failure).toBe('unsupported');
  });

  it('cancels a job that has not started, and leaves a started one to its row', async () => {
    const cmd = commands();
    const queue = new TransferQueue(PEER, cmd);
    const [first, second] = queue.add(
      'upload',
      [
        { source: '/a', isDir: false, size: 1 },
        { source: '/b', isDir: false, size: 1 },
      ],
      'C:\\',
    );
    await queue.pump(transfers(), NO_REFUSALS);

    expect(queue.cancelQueued(first!.id)).toBe(false);
    expect(queue.cancelQueued(second!.id)).toBe(true);
    await queue.pump(transfers(row({ transfer_id: 9, name: 'a', state: 'completed' })), NO_REFUSALS);
    expect(cmd.upload).toHaveBeenCalledTimes(1);

    queue.clearFinished();
    expect(queue.jobs).toEqual([]);
  });

  it('names a job after the last component in either dialect', () => {
    expect(baseName('C:\\Users\\beta\\notes.txt')).toBe('notes.txt');
    expect(baseName('/home/beta/projects/')).toBe('projects');
    expect(baseName('D:/data\\mixed/file.bin')).toBe('file.bin');
  });
});
