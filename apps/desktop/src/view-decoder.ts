// Decoding the host's picture inside the view window (ADR 0058).
//
// The window used to be handed decoded RGBA pixels: a sandboxed worker process
// on the Rust side turned the bitstream into a picture and the picture crossed
// the IPC boundary. One 1080p frame is 8 MiB, and 8 MiB through WebView2's IPC
// is well over a hundred milliseconds — per frame, before a single pixel is
// drawn, and with the window polling for the next one only after that. The
// same picture as H.264 is a few tens of kilobytes.
//
// So the bitstream crosses instead, and `VideoDecoder` turns it into a
// `VideoFrame` here. Nothing about the trust boundary gets weaker: the
// bitstream is still untrusted input from the network and it is still decoded
// inside a sandbox — Chromium's renderer sandbox rather than the worker
// process of §11.3 — and it still lands on the platform's hardware decoder.
// What disappears is a process, a shared-memory ring, an NV12-to-RGBA
// conversion, and the 8 MiB.
//
// Everything here is pure except `NativeDecoder`, which owns the decoder and
// the canvas; the parsing and the codec-string derivation are functions the
// tests drive directly.

import type { ViewStatus } from './view-window';

// Append-only, and the same table `view-window.ts` keeps: this array's index
// is the byte `ViewStatus::code` writes (`apps/desktop/src-tauri/src/view.rs`).
const STATUS_BY_CODE: readonly ViewStatus[] = [
  'waiting',
  'live',
  'reconnecting',
  'failed',
  'no-capture',
  'no-encoder',
  'secure-desktop',
];

/** Bytes of the fixed header every `view_next_chunk` response carries. */
export const CHUNK_RESPONSE_HEADER_BYTES = 8;

/** Bytes of the header in front of each frame inside a chunk response. */
export const CHUNK_FRAME_HEADER_BYTES = 13;

/** Flags byte bit: the session's `input` grant is live right now. */
const CHUNK_FLAG_INPUT = 0b001;
/** Flags byte bit: the host says it is recording this session (§17). */
const CHUNK_FLAG_RECORDING = 0b010;
/**
 * Flags byte bit: frames were lost, or the media connection was redialled.
 *
 * Everything the decoder is holding references pictures it will never see, so
 * it has to be thrown away and rebuilt from the next intra frame. This is the
 * one thing a bitstream transport has to say that a pixel transport does not.
 */
const CHUNK_FLAG_DESYNC = 0b100;

/** One encoded picture as `view_next_chunk` delivers it. */
export interface ChunkFrame {
  /** Whether this frame can be decoded without any frame before it. */
  keyframe: boolean;
  /** Capture timestamp, microseconds, carried through from the host. */
  timestampUs: number;
  /** Annex-B H.264 bitstream. */
  data: Uint8Array;
}

/** One `view_next_chunk` response. */
export interface ViewChunk {
  status: ViewStatus;
  /** Whether the session's `input` grant is live right now. */
  input: boolean;
  /** Whether the host says it is recording this session (§17). */
  recording: boolean;
  /** Whether the decoder must be reset before these frames are used. */
  desync: boolean;
  /** Encoded pictures, in the order the host produced them. */
  frames: ChunkFrame[];
}

/**
 * Parses the binary response of `view_next_chunk`.
 *
 * Layout, little endian: `status:u8 | flags:u8 | count:u16 | reserved:u32`,
 * then `count` frames of `keyframe:u8 | timestamp_us:u64 | length:u32 |
 * bitstream`.
 *
 * A response that does not describe itself consistently is refused rather
 * than half-read: it is the same untrusted-input rule the rest of the wire
 * follows (§21), and a truncated length here would otherwise be handed
 * straight to a decoder.
 */
export function decodeViewChunk(buffer: ArrayBuffer): ViewChunk {
  if (buffer.byteLength < CHUNK_RESPONSE_HEADER_BYTES) {
    throw new Error(
      `view chunk is ${buffer.byteLength} bytes, expected at least ${CHUNK_RESPONSE_HEADER_BYTES}`,
    );
  }
  const header = new DataView(buffer, 0, CHUNK_RESPONSE_HEADER_BYTES);
  const status = STATUS_BY_CODE[header.getUint8(0)];
  if (!status) {
    throw new Error(`unknown view status ${header.getUint8(0)}`);
  }
  const flags = header.getUint8(1);
  const count = header.getUint16(2, true);

  const frames: ChunkFrame[] = [];
  let at = CHUNK_RESPONSE_HEADER_BYTES;
  for (let i = 0; i < count; i += 1) {
    if (at + CHUNK_FRAME_HEADER_BYTES > buffer.byteLength) {
      throw new Error(`view chunk claims ${count} frames but ran out after ${i}`);
    }
    const frame = new DataView(buffer, at, CHUNK_FRAME_HEADER_BYTES);
    const length = frame.getUint32(9, true);
    const start = at + CHUNK_FRAME_HEADER_BYTES;
    if (start + length > buffer.byteLength) {
      throw new Error(`view chunk frame ${i} claims ${length} bytes that are not there`);
    }
    frames.push({
      keyframe: frame.getUint8(0) !== 0,
      // Microseconds; Number is exact well past any realistic session length.
      timestampUs: Number(frame.getBigUint64(1, true)),
      data: new Uint8Array(buffer, start, length),
    });
    at = start + length;
  }

  return {
    status,
    input: (flags & CHUNK_FLAG_INPUT) !== 0,
    recording: (flags & CHUNK_FLAG_RECORDING) !== 0,
    desync: (flags & CHUNK_FLAG_DESYNC) !== 0,
    frames,
  };
}

/**
 * The `avc1.PPCCLL` codec string an Annex-B stream describes, from its own
 * sequence parameter set, or `null` when this buffer carries no SPS.
 *
 * Read out of the stream rather than assumed. `VideoDecoder.configure` wants
 * a profile and level, the host picks the profile its hardware encoder will
 * actually give (High where it can, whatever the driver falls back to where
 * it cannot), and a decoder configured for the wrong one either refuses the
 * stream outright or — worse on some builds — accepts it and decodes it
 * wrongly.
 */
export function avcCodecString(bitstream: Uint8Array): string | null {
  for (const { type, start } of annexBNalUnits(bitstream)) {
    // 7 is the sequence parameter set; the three bytes after the NAL header
    // are profile_idc, the constraint-flags byte, and level_idc, which is
    // exactly what the codec string spells out in hex.
    if (type === 7 && start + 3 < bitstream.length) {
      const hex = [bitstream[start + 1], bitstream[start + 2], bitstream[start + 3]]
        .map((byte) => (byte ?? 0).toString(16).padStart(2, '0'))
        .join('');
      return `avc1.${hex}`;
    }
  }
  return null;
}

/** Walks the NAL units of an Annex-B buffer, yielding type and header offset. */
function* annexBNalUnits(data: Uint8Array): Generator<{ type: number; start: number }> {
  for (let i = 0; i + 3 <= data.length; i += 1) {
    let length = 0;
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      length = 3;
    } else if (
      i + 4 <= data.length &&
      data[i] === 0 &&
      data[i + 1] === 0 &&
      data[i + 2] === 0 &&
      data[i + 3] === 1
    ) {
      length = 4;
    } else {
      continue;
    }
    const start = i + length;
    const header = data[start];
    if (header === undefined) {
      return;
    }
    yield { type: header & 0x1f, start };
    i = start;
  }
}

/** What a {@link NativeDecoder} tells the window after a batch of frames. */
export interface DecodeOutcome {
  /** Whether anything was painted. */
  painted: boolean;
  /** Whether the host must be asked for an intra frame. */
  needKeyframe: boolean;
}

/**
 * Whether this `WebView` can decode the host's H.264 for itself.
 *
 * Asked rather than assumed: Chromium-based `WebView2` has `VideoDecoder`,
 * WebKitGTK and WKWebView may or may not depending on the version the machine
 * happens to have, and a window that cannot decode falls back to the RGBA
 * path unchanged. The probe uses a baseline profile because the question is
 * "is there an H.264 decoder at all"; the real configuration comes from the
 * stream's own SPS later.
 */
export async function nativeDecodingAvailable(): Promise<boolean> {
  if (typeof VideoDecoder === 'undefined') {
    return false;
  }
  try {
    const support = await VideoDecoder.isConfigSupported({
      codec: 'avc1.42E01E',
      optimizeForLatency: true,
    });
    return support.supported === true;
  } catch {
    return false;
  }
}

/**
 * Turns the host's bitstream into pictures on `canvas`.
 *
 * Configured lazily from the first intra frame, because that is the first
 * moment the stream says what it is (see {@link avcCodecString}), and reset
 * whenever the stream breaks. Frames before the first intra frame are
 * discarded rather than fed to the decoder: they reference pictures this
 * process never had.
 */
export class NativeDecoder {
  readonly #canvas: HTMLCanvasElement;
  readonly #onResize: (width: number, height: number) => void;
  #context: CanvasRenderingContext2D | null = null;
  #decoder: VideoDecoder | null = null;
  /** Set by the decoder's own error callback, read after the next batch. */
  #broken = false;
  #painted = false;

  constructor(canvas: HTMLCanvasElement, onResize: (width: number, height: number) => void) {
    this.#canvas = canvas;
    this.#onResize = onResize;
  }

  /** Whether a picture has ever been painted. */
  get painted(): boolean {
    return this.#painted;
  }

  /**
   * Drops the decoder. The next intra frame builds a new one.
   *
   * Called on a desync and on a decode error, and it is deliberately the
   * whole object rather than `VideoDecoder.reset()`: a stream that was
   * redialled may come back at a different resolution or profile, and
   * reconfiguring covers that where a reset does not.
   */
  reset(): void {
    const decoder = this.#decoder;
    this.#decoder = null;
    this.#broken = false;
    if (!decoder) {
      return;
    }
    try {
      if (decoder.state !== 'closed') {
        decoder.close();
      }
    } catch {
      // Already gone; nothing to release.
    }
  }

  /** Releases the decoder for good. */
  close(): void {
    this.reset();
  }

  /**
   * Feeds one batch of frames and says what the window owes the host.
   *
   * Frames go in as they come: `VideoDecoder.decode` is asynchronous and the
   * pictures come back on the output callback, so this returns long before
   * anything is on screen. Painting from the callback rather than from here
   * is what keeps the decode off the critical path.
   */
  push(frames: readonly ChunkFrame[]): DecodeOutcome {
    let needKeyframe = false;
    for (const frame of frames) {
      if (!this.#decoder) {
        if (!frame.keyframe) {
          // Nothing to decode this against.
          needKeyframe = true;
          continue;
        }
        if (!this.#configure(frame)) {
          needKeyframe = true;
          continue;
        }
      }
      if (!this.#decode(frame)) {
        needKeyframe = true;
      }
    }
    if (this.#broken) {
      this.reset();
      needKeyframe = true;
    }
    return { painted: this.#painted, needKeyframe };
  }

  /** Builds a decoder for the stream this keyframe describes. */
  #configure(keyframe: ChunkFrame): boolean {
    const codec = avcCodecString(keyframe.data);
    if (!codec) {
      // A keyframe with no SPS in it is one the host split differently than
      // this expects; the next one will carry it.
      return false;
    }
    try {
      const decoder = new VideoDecoder({
        output: (frame) => this.#paint(frame),
        error: () => {
          this.#broken = true;
        },
      });
      decoder.configure({
        codec,
        // The whole point: no reordering window, emit each picture as soon as
        // it is decoded rather than waiting to see whether a later one comes
        // first. There are no B-frames in this stream (the host disables
        // them), so nothing is lost and a frame of latency is saved.
        optimizeForLatency: true,
      });
      this.#decoder = decoder;
      return true;
    } catch {
      return false;
    }
  }

  #decode(frame: ChunkFrame): boolean {
    const decoder = this.#decoder;
    if (!decoder || decoder.state !== 'configured') {
      return false;
    }
    try {
      decoder.decode(
        new EncodedVideoChunk({
          type: frame.keyframe ? 'key' : 'delta',
          timestamp: frame.timestampUs,
          data: frame.data,
        }),
      );
      return true;
    } catch {
      this.#broken = true;
      return false;
    }
  }

  /**
   * Draws one decoded picture and releases it.
   *
   * `drawImage` of a `VideoFrame`, not `putImageData` of a pixel buffer: the
   * frame can still be in GPU memory at this point and this is the path that
   * lets it stay there. `desynchronized` asks the compositor for the
   * low-latency canvas path, which is exactly what a remote desktop wants and
   * exactly what a document does not.
   */
  #paint(frame: VideoFrame): void {
    try {
      const width = frame.displayWidth;
      const height = frame.displayHeight;
      if (width === 0 || height === 0) {
        return;
      }
      if (this.#canvas.width !== width || this.#canvas.height !== height) {
        this.#canvas.width = width;
        this.#canvas.height = height;
        this.#onResize(width, height);
      }
      this.#context ??= this.#canvas.getContext('2d', {
        alpha: false,
        desynchronized: true,
      });
      this.#context?.drawImage(frame, 0, 0);
      this.#painted = true;
    } finally {
      frame.close();
    }
  }
}
