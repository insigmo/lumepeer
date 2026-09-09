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
export const CHUNK_RESPONSE_HEADER_BYTES = 9;

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

/**
 * Video codec identifier as [`MessageKind::MediaCodec`]'s wire byte and the
 * ninth byte of every `view_next_chunk` header encode it
 * (`crates/core/src/protocol.rs`, `apps/desktop/src-tauri/src/view.rs`; ADR
 * 0067). H.264 is the mandatory baseline and the value every session starts
 * on until a `MediaCodec` message says otherwise.
 */
export enum WireCodec {
  H264 = 0,
  Av1 = 1,
  H265 = 2,
  Vp9 = 3,
}

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
  /** The codec this batch of frames is encoded with (ADR 0067). */
  codec: WireCodec;
  /** Encoded pictures, in the order the host produced them. */
  frames: ChunkFrame[];
}

/**
 * Parses the binary response of `view_next_chunk`.
 *
 * Layout, little endian: `status:u8 | flags:u8 | count:u16 | reserved:u32 |
 * codec:u8`, then `count` frames of `keyframe:u8 | timestamp_us:u64 |
 * length:u32 | bitstream`.
 *
 * A response that does not describe itself consistently is refused rather
 * than half-read: it is the same untrusted-input rule the rest of the wire
 * follows (§21), and a truncated length here would otherwise be handed
 * straight to a decoder. An unrecognized codec byte is refused the same way
 * (ADR 0067) — this side never guesses at a codec it has no name for.
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
  const codecByte = header.getUint8(8);
  const codec = codecByte as WireCodec;
  if (WireCodec[codec] === undefined) {
    throw new Error(`unknown view codec ${codecByte}`);
  }

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
    codec,
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
 * Fixed `VideoDecoder` config string for each optional codec (ADR 0067,
 * ADR 0069).
 *
 * Unlike H.264 (see {@link avcCodecString}), nothing here reads a profile out
 * of the stream itself. For AV1 that is now a decision rather than a gap: an
 * AV1 temporal unit is a sequence of OBUs, not Annex-B NAL units, so its
 * profile and level sit inside a sequence header OBU behind variable-length
 * bit fields — and the host's Media Foundation encoder produces Main profile
 * 8-bit and nothing else, which is the whole of what this string has to say.
 * For H.265 the host's encoders produce Main profile 8-bit and nothing else
 * either, so the same reasoning applies; VP9 still has no encoder anywhere in
 * this workspace to read a stream from (batch 09).
 *
 * AV1 is Main profile, level 4.1, main tier, 8-bit. Level 4.1 rather than 4.0
 * because it is the first level whose sample rate covers 1080p at 60 Hz, and
 * `MAX_PICTURE_PIXELS` is 1080p while nothing pins the host to 30. This is a
 * ceiling the decoder is asked to clear, not a description of any particular
 * stream, so naming one level above what a default session needs costs
 * nothing — and a machine that cannot clear it says so, and the session stays
 * on H.264.
 *
 * H.265 is `hev1`, Main profile, main tier, level 4.0 (ADR 0071), and both
 * halves of that are a decision about matching the stream rather than a
 * default:
 *
 * - `hev1` rather than `hvc1`. In ISOBMFF the two differ in where the
 *   parameter sets live: `hvc1` promises they are out of band and never
 *   change, `hev1` allows them in the bitstream. The host's encoders put the
 *   VPS/SPS/PPS in band ahead of every IRAP picture — an Annex-B stream, the
 *   same shape the H.264 path already produces — which is what `hev1`
 *   describes. WebCodecs decides the container from whether `description` is
 *   present, and this configuration deliberately has none, so the string and
 *   the stream agree on both counts.
 * - Level 4.0 (`L120`, H.265 counting levels in thirtieths) rather than 3.1.
 *   3.1 stops at 720p, and this pipeline's default picture is 1080p; the
 *   VA-API encoder writes exactly this level into its own sequence header
 *   (`HEVC_LEVEL_IDC`). A decoder asked for less than the stream carries is
 *   free to accept the configuration and then fail on the pictures, which is
 *   a black window rather than an honest fallback to H.264.
 */
const OPTIONAL_CODEC_CONFIGS: Readonly<Record<WireCodec.Av1 | WireCodec.H265 | WireCodec.Vp9, string>> = {
  [WireCodec.Av1]: 'av01.0.05M.08',
  [WireCodec.H265]: 'hev1.1.6.L120.B0',
  [WireCodec.Vp9]: 'vp09.00.10.08',
};

/**
 * Which optional codecs (beyond the mandatory H.264 baseline) this `WebView`
 * can actually decode right now (ADR 0067).
 *
 * Asked one profile at a time from `VideoDecoder.isConfigSupported` itself,
 * never assumed from a table of "we think this platform can" — an unverified
 * assumption is the same shape of mistake as v0.0.14's blank screen (see the
 * comment above the build matrix in `.github/workflows/release.yml`), where a
 * release shipped without the software-encoder fallback a hardware-less host
 * actually needed. H.264 is not asked about here: it is the mandatory baseline
 * every peer can decode and has no
 * `Hello.features` string of its own (see {@link nativeDecodingAvailable}
 * for that one's own baseline probe).
 */
export async function supportedOptionalCodecs(): Promise<WireCodec[]> {
  if (typeof VideoDecoder === 'undefined') {
    return [];
  }
  const supported: WireCodec[] = [];
  for (const codec of [WireCodec.Av1, WireCodec.H265, WireCodec.Vp9] as const) {
    try {
      const support = await VideoDecoder.isConfigSupported({
        codec: OPTIONAL_CODEC_CONFIGS[codec],
        optimizeForLatency: true,
      });
      if (support.supported === true) {
        supported.push(codec);
      }
    } catch {
      // Treated as unsupported, the same as `nativeDecodingAvailable` does.
    }
  }
  return supported;
}

/**
 * The `VideoDecoder` config string for `codec`, given the keyframe that is
 * about to configure a decoder for it (ADR 0067).
 *
 * H.264 alone reads its profile out of the stream (see {@link avcCodecString}):
 * its encoder can pick High, Main or Baseline depending on what the host's
 * hardware gives, so nothing else can name the right string. The other three
 * have exactly one fixed {@link OPTIONAL_CODEC_CONFIGS} entry each, and
 * `keyframe.data` is deliberately not looked at for them — an AV1 stream is
 * OBUs rather than Annex-B NAL units, so the H.264 walk would find start
 * codes wherever the byte pattern happened to produce them (ADR 0069).
 */
export function configStringFor(codec: WireCodec, keyframe: ChunkFrame): string | null {
  if (codec === WireCodec.H264) {
    return avcCodecString(keyframe.data);
  }
  return OPTIONAL_CODEC_CONFIGS[codec] ?? null;
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
  /**
   * The codec the current decoder (if any) was configured for (ADR 0067).
   * `null` until the first successful {@link NativeDecoder.#configure} call,
   * so the very first keyframe of a session is never mistaken for a change.
   */
  #codec: WireCodec | null = null;

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
   * Feeds one batch of frames, encoded with `codec`, and says what the window
   * owes the host.
   *
   * Frames go in as they come: `VideoDecoder.decode` is asynchronous and the
   * pictures come back on the output callback, so this returns long before
   * anything is on screen. Painting from the callback rather than from here
   * is what keeps the decode off the critical path.
   *
   * A mid-session codec change is not a supported transition (ADR 0067): the
   * decoder holds state for the codec it was configured with, so a `codec`
   * that differs from the last call's gets the same treatment as
   * `CHUNK_FLAG_DESYNC` — thrown away and rebuilt from the next intra frame.
   */
  push(frames: readonly ChunkFrame[], codec: WireCodec): DecodeOutcome {
    if (this.#codec !== null && this.#codec !== codec) {
      this.reset();
    }
    this.#codec = codec;
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
    const codec = configStringFor(this.#codec ?? WireCodec.H264, keyframe);
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
