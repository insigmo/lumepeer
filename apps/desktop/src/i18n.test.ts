import { afterEach, describe, expect, it } from 'vitest';

import {
  DEFAULT_LOCALE,
  detectLocale,
  dirOf,
  getStoredLocaleChoice,
  resolveLocale,
  setStoredLocaleChoice,
  SUPPORTED_LOCALES,
  t,
  type TranslationKey,
} from './i18n';
import { en } from './locales/en';

// The `en` dictionary is a `Record<TranslationKey, ...>`, so TypeScript
// already guarantees it has exactly one entry per key — its own keys are
// therefore a reliable, always-up-to-date list of every TranslationKey,
// without hand-maintaining a second copy that could drift.
const ALL_KEYS = Object.keys(en) as TranslationKey[];

describe('i18n', () => {
  it('falls back to the default locale when nothing matches', () => {
    // Korean isn't a locale this app ships, unlike French below — picking an
    // actually-unsupported code keeps this test meaningful as more languages
    // are added.
    expect(detectLocale({ language: 'ko-KR', languages: ['ko-KR'] })).toBe(DEFAULT_LOCALE);
  });

  it('picks a supported locale from navigator.languages', () => {
    expect(detectLocale({ language: 'ar-EG', languages: ['ar-EG', 'en-US'] })).toBe('ar');
    expect(detectLocale({ language: 'fr-FR', languages: ['fr-FR'] })).toBe('fr');
  });

  it('marks every supported locale LTR except Arabic, the only RTL locale', () => {
    for (const locale of SUPPORTED_LOCALES) {
      expect(dirOf(locale)).toBe(locale === 'ar' ? 'rtl' : 'ltr');
    }
  });

  it('has every supported locale translate every key with no leftover placeholders', () => {
    const PROBE = 'ARG_PROBE';
    for (const locale of SUPPORTED_LOCALES) {
      for (const key of ALL_KEYS) {
        const value = t(locale, key, PROBE);
        expect(value).not.toBe('');
        // A literal `${...}` surviving into the rendered string means a
        // template placeholder was never substituted.
        expect(value).not.toMatch(/\$\{.*\}/);
        // Keys that interpolate an argument in English must do so in every
        // locale too — placeholders and their order aren't allowed to change
        // during translation, so the same key must stay a function everywhere.
        if (typeof en[key] === 'function') {
          expect(value).toContain(PROBE);
        }
      }
    }
  });

  it('interpolates the peer name into the request title', () => {
    expect(t('en', 'consent.request.title', 'guest-ab12')).toBe('guest-ab12 wants to connect');
  });
});

describe('locale resolution', () => {
  afterEach(() => {
    localStorage.clear();
  });

  it('prefers a saved manual choice over system detection', () => {
    setStoredLocaleChoice('de');
    expect(resolveLocale({ language: 'fr-FR', languages: ['fr-FR'] })).toBe('de');
  });

  it('falls back to system detection when nothing is saved', () => {
    expect(getStoredLocaleChoice()).toBeNull();
    expect(resolveLocale({ language: 'ja-JP', languages: ['ja-JP'] })).toBe('ja');
  });

  it('falls back to the default locale when neither saved nor detected', () => {
    expect(resolveLocale({ language: 'ko-KR', languages: ['ko-KR'] })).toBe(DEFAULT_LOCALE);
  });
});
