// The file manager's transfer queue (ADR 0124).
//
// The actor moves one file per request and says nothing about a request that
// went nowhere: an upload the host declines has no row, and a download it
// refuses leaves only a reason on the view. A person dragging twenty files
// across wants twenty rows that each end in "done" or in a reason, so the
// window keeps its own queue and feeds the actor one job at a time.
//
// One at a time on purpose. The actor holds at most three downloads in
// flight and names none of them, so two jobs for files called `notes.txt` in
// different folders could only be told apart by the order their rows appear
// in — which is exactly the order one-at-a-time gives for free. The transfer
// engine moves one file at full speed as well as it moves three.

import type { FileTransfers, TransferRow } from './file-transfers';

/** Which way a job moves bytes. */
export type JobDirection = 'upload' | 'download';

/**
 * Where a job is up to.
 *
 * `starting` is the stretch between asking and the first row: the sending
 * side is hashing the file, which for a large one is most of a minute.
 */
export type JobState = 'queued' | 'starting' | 'running' | 'done' | 'failed' | 'cancelled';

/** Why a job ended without its bytes arriving, as a translation key suffix. */
export type JobFailure =
  | 'declined'
  | 'not_granted'
  | 'bad_path'
  | 'unreadable'
  | 'too_many'
  | 'unsupported'
  | 'no_answer'
  | 'failed';

/** One file or folder the person asked to move. */
export interface Job {
  id: number;
  direction: JobDirection;
  /** Basename, which is also what the transfer row will be called. */
  name: string;
  /** Full path on the side it comes from. */
  source: string;
  /** Directory on the side it goes to. */
  target: string;
  isDir: boolean;
  /** Size as the pane listed it; zero for a folder, whose size is unknown. */
  size: number;
  state: JobState;
  /** The row this job became, once it has one. */
  transferId: number | null;
  failure: JobFailure | null;
  /** When the job was handed to the actor, for the no-answer timeout. */
  submittedAt: number;
  /** Rows that existed before the job was handed over, so its own is the new one. */
  baseline: ReadonlySet<number>;
  /** The refusal count before the job was handed over. */
  refusalsBefore: number;
}

/** What the queue needs from the actor; injectable so the logic is testable. */
export interface QueueCommands {
  download(path: string, into: string): Promise<void>;
  upload(localPath: string, remoteDir: string): Promise<void>;
}

/** The two refusal counts the remote pane's status carries (ADR 0124). */
export interface RefusalCounts {
  fetches_refused: number;
  uploads_refused: number;
  /** Why the last download was refused, when one was. */
  fetch_refused: 'not_granted' | 'bad_path' | 'unreadable' | 'too_many' | null;
}

/**
 * How long a file job may wait for its first row before it is called lost.
 *
 * Long, because the wait is a hash over the whole file on one side or the
 * other: tens of gigabytes take minutes, and a job failed while its file is
 * still being read would be a false alarm about a transfer that then runs.
 */
export const FILE_START_TIMEOUT_MS = 10 * 60_000;

/**
 * How long a folder job may wait for its first row.
 *
 * Short, because a folder with no files in it — only empty folders, or
 * nothing — arrives without ever producing a row: its tree is made on the
 * far side and the job has nothing left to show. A folder with files in it
 * has a row within one round trip of its walk.
 */
export const DIR_START_TIMEOUT_MS = 20_000;

/** An IPC rejection's `code`, or an empty string for anything else. */
export function ipcCode(error: unknown): string {
  return typeof error === 'object' && error !== null && 'code' in error
    ? String((error as { code: unknown }).code)
    : '';
}

/**
 * An IPC rejection's `message`, or an empty string for anything else.
 *
 * Read only to tell apart the two `CORE` refusals a transfer can meet — a
 * path the actor will not send (`malformed`) and a queue that is full for
 * now (`not permitted`) — which share one code on the IPC boundary.
 */
export function ipcMessage(error: unknown): string {
  return typeof error === 'object' && error !== null && 'message' in error
    ? String((error as { message: unknown }).message)
    : '';
}

/** The last path component, whichever separator the path uses. */
export function baseName(path: string): string {
  const trimmed = path.replace(/[/\\]+$/, '');
  const cut = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return cut < 0 ? trimmed : trimmed.slice(cut + 1);
}

/** What a queue item needs to know about the thing being moved. */
export interface QueueItem {
  source: string;
  isDir: boolean;
  size: number;
}

/** The jobs of one window, and the one-at-a-time pump that runs them. */
export class TransferQueue {
  readonly jobs: Job[] = [];
  private nextId = 1;
  private busy = false;

  constructor(
    private readonly peer: string,
    private readonly commands: QueueCommands,
    private readonly now: () => number = () => Date.now(),
  ) {}

  /** Queues one job per item, in order, all bound for `target`. */
  add(direction: JobDirection, items: readonly QueueItem[], target: string): Job[] {
    const added = items.map((item) => {
      const job: Job = {
        id: this.nextId,
        direction,
        name: baseName(item.source),
        source: item.source,
        target,
        isDir: item.isDir,
        size: item.size,
        state: 'queued',
        transferId: null,
        failure: null,
        submittedAt: 0,
        baseline: new Set(),
        refusalsBefore: 0,
      };
      this.nextId += 1;
      return job;
    });
    this.jobs.push(...added);
    return added;
  }

  /** The job being worked on, if there is one. */
  active(): Job | undefined {
    return this.jobs.find((job) => job.state === 'starting' || job.state === 'running');
  }

  /** Whether anything is waiting or moving. */
  pending(): boolean {
    return this.jobs.some(
      (job) => job.state === 'queued' || job.state === 'starting' || job.state === 'running',
    );
  }

  /**
   * Takes a job out of the queue before it starts.
   *
   * A running job is not this method's: its bytes are the actor's, and the
   * cancel for them is `file_abort` on its row. Returns whether it was taken.
   */
  cancelQueued(jobId: number): boolean {
    const job = this.jobs.find((candidate) => candidate.id === jobId);
    if (!job || job.state !== 'queued') {
      return false;
    }
    job.state = 'cancelled';
    return true;
  }

  /** Forgets every job that has ended. */
  clearFinished(): void {
    const keep = this.jobs.filter(
      (job) => job.state === 'queued' || job.state === 'starting' || job.state === 'running',
    );
    this.jobs.splice(0, this.jobs.length, ...keep);
  }

  /**
   * One turn: moves the active job on from what the actor now reports, and
   * starts the next one when nothing is active.
   *
   * Returns the jobs that finished with their bytes delivered on this turn,
   * so the caller can re-read the folder they landed in.
   */
  async pump(transfers: FileTransfers, counts: RefusalCounts | null): Promise<Job[]> {
    if (this.busy) {
      return [];
    }
    this.busy = true;
    try {
      const finished: Job[] = [];
      const rows = transfers.transfers.filter((row) => row.peer_label === this.peer);
      const active = this.active();
      if (active) {
        this.advance(active, rows, counts);
        if (active.state === 'done') {
          finished.push(active);
        }
      }
      if (!this.active()) {
        await this.startNext(rows, counts);
      }
      return finished;
    } finally {
      this.busy = false;
    }
  }

  private advance(job: Job, rows: readonly TransferRow[], counts: RefusalCounts | null): void {
    if (job.state === 'starting') {
      const row = rows.find(
        (candidate) =>
          !job.baseline.has(candidate.transfer_id) &&
          candidate.incoming === (job.direction === 'download') &&
          candidate.name === job.name,
      );
      if (row) {
        job.transferId = row.transfer_id;
        job.state = 'running';
      } else if (counts && refusals(job.direction, counts) !== job.refusalsBefore) {
        job.state = 'failed';
        job.failure =
          job.direction === 'download' ? (counts.fetch_refused ?? 'failed') : 'declined';
        return;
      } else {
        const waited = this.now() - job.submittedAt;
        if (job.isDir && waited > DIR_START_TIMEOUT_MS) {
          // A folder with no files in it: made on the far side, no row to
          // show for it (see `DIR_START_TIMEOUT_MS`).
          job.state = 'done';
        } else if (!job.isDir && waited > FILE_START_TIMEOUT_MS) {
          job.state = 'failed';
          job.failure = 'no_answer';
        }
        return;
      }
    }
    const row = rows.find((candidate) => candidate.transfer_id === job.transferId);
    if (!row) {
      // The actor forgets finished rows only once they are long over; a row
      // gone from under a running job is one that ended.
      job.state = 'done';
      return;
    }
    switch (row.state) {
      case 'running':
        job.state = 'running';
        break;
      case 'completed':
        job.state = 'done';
        break;
      case 'cancelled':
        job.state = 'cancelled';
        break;
      case 'failed':
        job.state = 'failed';
        job.failure = 'failed';
        break;
    }
  }

  private async startNext(rows: readonly TransferRow[], counts: RefusalCounts | null): Promise<void> {
    const job = this.jobs.find((candidate) => candidate.state === 'queued');
    if (!job) {
      return;
    }
    job.baseline = new Set(rows.map((row) => row.transfer_id));
    job.refusalsBefore = counts ? refusals(job.direction, counts) : 0;
    job.submittedAt = this.now();
    job.state = 'starting';
    try {
      if (job.direction === 'download') {
        await this.commands.download(job.source, job.target);
      } else {
        await this.commands.upload(job.source, job.target);
      }
    } catch (error: unknown) {
      const code = ipcCode(error);
      const message = ipcMessage(error);
      if (code === 'CORE' && message.includes('not permitted')) {
        // The actor's own bound on downloads in flight: nothing is wrong, the
        // one before has not been offered back yet. Asked again next turn.
        job.state = 'queued';
        return;
      }
      job.state = 'failed';
      job.failure =
        code === 'PEER_TOO_OLD'
          ? 'unsupported'
          : code === 'CORE' && message.includes('malformed')
            ? 'bad_path'
            : 'failed';
    }
  }
}

function refusals(direction: JobDirection, counts: RefusalCounts): number {
  return direction === 'download' ? counts.fetches_refused : counts.uploads_refused;
}
