import { describe, expect, it } from 'vitest';

import {
  avcCodecString,
  CHUNK_FRAME_HEADER_BYTES,
  CHUNK_RESPONSE_HEADER_BYTES,
  configStringFor,
  decodeViewChunk,
  nativeDecodingAvailable,
  supportedOptionalCodecs,
  WireCodec,
} from './view-decoder';

/**
 * Builds the bytes `view_next_chunk` would return. Mirrors
 * `encode_chunk_response`. `codec` defaults to H.264 (0), the byte a
 * zero-initialized `ArrayBuffer` already carries, so existing callers that do
 * not care about it need no change.
 */
function chunkResponse(
  status: number,
  flags: number,
  frames: readonly { keyframe: boolean; timestampUs: number; data: number[] }[],
  codec: number = WireCodec.H264,
): ArrayBuffer {
  const size =
    CHUNK_RESPONSE_HEADER_BYTES +
    frames.reduce((total, f) => total + CHUNK_FRAME_HEADER_BYTES + f.data.length, 0);
  const buffer = new ArrayBuffer(size);
  const view = new DataView(buffer);
  const bytes = new Uint8Array(buffer);
  view.setUint8(0, status);
  view.setUint8(1, flags);
  view.setUint16(2, frames.length, true);
  view.setUint8(8, codec);
  let at = CHUNK_RESPONSE_HEADER_BYTES;
  for (const frame of frames) {
    view.setUint8(at, frame.keyframe ? 1 : 0);
    view.setBigUint64(at + 1, BigInt(frame.timestampUs), true);
    view.setUint32(at + 9, frame.data.length, true);
    bytes.set(frame.data, at + CHUNK_FRAME_HEADER_BYTES);
    at += CHUNK_FRAME_HEADER_BYTES + frame.data.length;
  }
  return buffer;
}

describe('decodeViewChunk', () => {
  it('reads every frame in the order the host produced them', () => {
    const chunk = decodeViewChunk(
      chunkResponse(1, 0b01, [
        { keyframe: true, timestampUs: 1_000, data: [1, 2, 3] },
        { keyframe: false, timestampUs: 2_000, data: [4, 5] },
      ]),
    );
    expect(chunk.status).toBe('live');
    expect(chunk.input).toBe(true);
    expect(chunk.recording).toBe(false);
    expect(chunk.desync).toBe(false);
    expect(chunk.frames.map((f) => f.timestampUs)).toEqual([1_000, 2_000]);
    expect(chunk.frames[0]?.keyframe).toBe(true);
    expect(Array.from(chunk.frames[1]?.data ?? [])).toEqual([4, 5]);
  });

  it('carries the status and the live grant even with no frames in it', () => {
    // A still screen is exactly when a lowered grant has to reach the window,
    // and there is no picture to carry it.
    const chunk = decodeViewChunk(chunkResponse(6, 0b10, []));
    expect(chunk.status).toBe('secure-desktop');
    expect(chunk.input).toBe(false);
    expect(chunk.recording).toBe(true);
    expect(chunk.frames).toEqual([]);
  });

  it('reports a broken stream so the decoder is reset before the next frame', () => {
    expect(decodeViewChunk(chunkResponse(1, 0b100, [])).desync).toBe(true);
  });

  it('refuses a response that claims more frames than it carries', () => {
    const buffer = chunkResponse(1, 0, [{ keyframe: true, timestampUs: 0, data: [1] }]);
    new DataView(buffer).setUint16(2, 4, true);
    expect(() => decodeViewChunk(buffer)).toThrow();
  });

  it('refuses a frame whose length runs past the response', () => {
    const buffer = chunkResponse(1, 0, [{ keyframe: true, timestampUs: 0, data: [1, 2] }]);
    new DataView(buffer).setUint32(CHUNK_RESPONSE_HEADER_BYTES + 9, 4096, true);
    expect(() => decodeViewChunk(buffer)).toThrow();
  });

  it('defaults to H.264 when the host never negotiated a codec', () => {
    expect(decodeViewChunk(chunkResponse(1, 0, [])).codec).toBe(WireCodec.H264);
  });

  it('reads every assigned codec byte (ADR 0066)', () => {
    for (const codec of [WireCodec.H264, WireCodec.Av1, WireCodec.H265, WireCodec.Vp9]) {
      expect(decodeViewChunk(chunkResponse(1, 0, [], codec)).codec).toBe(codec);
    }
  });

  it('refuses a codec byte it has no name for, rather than guessing', () => {
    expect(() => decodeViewChunk(chunkResponse(1, 0, [], 200))).toThrow();
  });

  it('refuses a status byte it has no name for', () => {
    expect(() => decodeViewChunk(chunkResponse(200, 0, []))).toThrow();
  });

  it('refuses a response too short to hold its own header', () => {
    expect(() => decodeViewChunk(new ArrayBuffer(3))).toThrow();
  });
});

describe('avcCodecString', () => {
  /** An Annex-B buffer with one SPS NAL carrying `profile`/`flags`/`level`. */
  function withSps(profile: number, flags: number, level: number, startCode: number[]): Uint8Array {
    return new Uint8Array([...startCode, 0x67, profile, flags, level, 0x00]);
  }

  it('reads the profile, constraint flags and level out of the sequence parameter set', () => {
    // 0x64 High, no constraint flags, level 4.0 — what the host asks its
    // hardware encoder for.
    expect(avcCodecString(withSps(0x64, 0x00, 0x28, [0, 0, 0, 1]))).toBe('avc1.640028');
  });

  it('finds the parameter set behind a three-byte start code too', () => {
    expect(avcCodecString(withSps(0x42, 0xe0, 0x1e, [0, 0, 1]))).toBe('avc1.42e01e');
  });

  it('finds the parameter set when an access unit delimiter comes first', () => {
    const stream = new Uint8Array([
      0, 0, 0, 1, 0x09, 0x10, // access unit delimiter
      0, 0, 0, 1, 0x67, 0x4d, 0x40, 0x1f, 0x00,
    ]);
    expect(avcCodecString(stream)).toBe('avc1.4d401f');
  });

  it('answers nothing for a buffer with no parameter set, rather than guessing', () => {
    // Configuring the decoder for a profile the stream is not in either
    // fails outright or decodes wrongly, so a guess is worse than waiting
    // for the next keyframe.
    expect(avcCodecString(new Uint8Array([0, 0, 0, 1, 0x41, 0x9a, 0x00]))).toBeNull();
    expect(avcCodecString(new Uint8Array([]))).toBeNull();
  });

  it('answers nothing for a parameter set that is cut off', () => {
    expect(avcCodecString(new Uint8Array([0, 0, 0, 1, 0x67, 0x64]))).toBeNull();
  });
});

describe('nativeDecodingAvailable', () => {
  it('says no when this WebView has no VideoDecoder at all', async () => {
    // jsdom has none, which is the same answer WebKitGTK gives on a machine
    // whose version predates WebCodecs — and the case the RGBA fallback
    // exists for.
    expect(typeof (globalThis as { VideoDecoder?: unknown }).VideoDecoder).toBe('undefined');
    await expect(nativeDecodingAvailable()).resolves.toBe(false);
  });

  it('says no when the decoder refuses H.264 rather than throwing at it', async () => {
    const scope = globalThis as { VideoDecoder?: unknown };
    scope.VideoDecoder = { isConfigSupported: () => Promise.resolve({ supported: false }) };
    try {
      await expect(nativeDecodingAvailable()).resolves.toBe(false);
    } finally {
      delete scope.VideoDecoder;
    }
  });

  it('says yes when the decoder accepts the configuration', async () => {
    const scope = globalThis as { VideoDecoder?: unknown };
    scope.VideoDecoder = { isConfigSupported: () => Promise.resolve({ supported: true }) };
    try {
      await expect(nativeDecodingAvailable()).resolves.toBe(true);
    } finally {
      delete scope.VideoDecoder;
    }
  });
});

describe('configStringFor', () => {
  const keyframe = (data: number[]) => ({ keyframe: true, timestampUs: 0, data: new Uint8Array(data) });

  it('reads H.264 out of the stream itself, exactly like avcCodecString', () => {
    const sps = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0x00];
    expect(configStringFor(WireCodec.H264, keyframe(sps))).toBe(avcCodecString(new Uint8Array(sps)));
  });

  it('answers a fixed config string for each optional codec (ADR 0066)', () => {
    // Nothing has encoded any of these yet (batches 07/08/09), so unlike
    // H.264 there is no stream to read a profile out of — the frame's own
    // bytes are irrelevant to the answer.
    const frame = keyframe([0xff]);
    expect(configStringFor(WireCodec.Av1, frame)).toBe('av01.0.04M.08');
    expect(configStringFor(WireCodec.H265, frame)).toBe('hev1.1.6.L93.B0');
    expect(configStringFor(WireCodec.Vp9, frame)).toBe('vp09.00.10.08');
  });

  it('answers nothing for a codec byte it has no config for, rather than guessing', () => {
    expect(configStringFor(99 as WireCodec, keyframe([0]))).toBeNull();
  });
});

describe('supportedOptionalCodecs', () => {
  it('answers nothing when this WebView has no VideoDecoder at all', async () => {
    expect(typeof (globalThis as { VideoDecoder?: unknown }).VideoDecoder).toBe('undefined');
    await expect(supportedOptionalCodecs()).resolves.toEqual([]);
  });

  it('asks the browser once per optional codec, never a table of assumptions', async () => {
    const asked: unknown[] = [];
    const scope = globalThis as { VideoDecoder?: unknown };
    scope.VideoDecoder = {
      isConfigSupported: (config: { codec: string }) => {
        asked.push(config.codec);
        // Only the AV1 config this module uses is "supported" here, so a
        // table that assumed every codec is available would be caught by
        // the exact set this asserts on below.
        return Promise.resolve({ supported: config.codec === 'av01.0.04M.08' });
      },
    };
    try {
      await expect(supportedOptionalCodecs()).resolves.toEqual([WireCodec.Av1]);
      expect(asked).toEqual(['av01.0.04M.08', 'hev1.1.6.L93.B0', 'vp09.00.10.08']);
    } finally {
      delete scope.VideoDecoder;
    }
  });

  it('treats a throwing probe as unsupported rather than failing the whole call', async () => {
    const scope = globalThis as { VideoDecoder?: unknown };
    scope.VideoDecoder = {
      isConfigSupported: () => Promise.reject(new Error('not implemented')),
    };
    try {
      await expect(supportedOptionalCodecs()).resolves.toEqual([]);
    } finally {
      delete scope.VideoDecoder;
    }
  });
});
