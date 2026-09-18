// The picture the connection list shows for a remembered host: kept by the
// view window, read by the main window, and never worth an error.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { forgetThumbnail, rememberThumbnail, thumbnailOf } from './peer-thumbnails';

const IMAGE = 'data:image/jpeg;base64,AAAA';

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('peer thumbnails', () => {
  it('gives back the picture it was handed', () => {
    rememberThumbnail('host-a', IMAGE);
    expect(thumbnailOf('host-a')).toBe(IMAGE);
  });

  it('answers nothing for a host that has none', () => {
    expect(thumbnailOf('host-b')).toBeNull();
  });

  it('refuses anything that is not an image', () => {
    localStorage.setItem('lumepeer.thumb.host-c', JSON.stringify({ at: 1, image: 'javascript:1' }));
    expect(thumbnailOf('host-c')).toBeNull();
  });

  it('survives a stored value that is not the shape it expects', () => {
    localStorage.setItem('lumepeer.thumb.host-d', 'not json');
    expect(thumbnailOf('host-d')).toBeNull();
  });

  it('forgets one host without touching the others', () => {
    rememberThumbnail('host-a', IMAGE);
    rememberThumbnail('host-b', IMAGE);
    forgetThumbnail('host-a');
    expect(thumbnailOf('host-a')).toBeNull();
    expect(thumbnailOf('host-b')).toBe(IMAGE);
  });

  it('keeps only the newest dozen', () => {
    for (let i = 0; i < 15; i += 1) {
      vi.setSystemTime(new Date(1_000_000 + i * 1000));
      rememberThumbnail(`host-${i}`, IMAGE);
    }
    vi.useRealTimers();
    const kept = Object.keys(localStorage).filter((key) => key.startsWith('lumepeer.thumb.'));
    expect(kept).toHaveLength(12);
    // The three oldest are the ones that went.
    expect(thumbnailOf('host-0')).toBeNull();
    expect(thumbnailOf('host-2')).toBeNull();
    expect(thumbnailOf('host-3')).toBe(IMAGE);
    expect(thumbnailOf('host-14')).toBe(IMAGE);
  });

  it('keeps the newest picture when storage refuses to grow', () => {
    rememberThumbnail('host-old', IMAGE);
    let full = true;
    const real = Storage.prototype.setItem;
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(function (
      this: Storage,
      key: string,
      value: string,
    ) {
      if (full) {
        full = false;
        throw new DOMException('quota', 'QuotaExceededError');
      }
      real.call(this, key, value);
    });

    rememberThumbnail('host-new', IMAGE);
    expect(thumbnailOf('host-new')).toBe(IMAGE);
    expect(thumbnailOf('host-old')).toBeNull();
  });

  it('says nothing and throws nothing when storage is unusable', () => {
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('disabled');
    });
    vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
      throw new Error('disabled');
    });
    expect(() => {
      rememberThumbnail('host-e', IMAGE);
    }).not.toThrow();
    expect(() => {
      forgetThumbnail('host-e');
    }).not.toThrow();
    expect(thumbnailOf('host-e')).toBeNull();
  });
});
