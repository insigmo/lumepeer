import { describe, expect, it } from 'vitest';

import { DEFAULT_LOCALE, detectLocale, dirOf, SUPPORTED_LOCALES, t } from './i18n';

describe('i18n', () => {
  it('falls back to the default locale when nothing matches', () => {
    expect(detectLocale({ language: 'fr-FR', languages: ['fr-FR'] })).toBe(DEFAULT_LOCALE);
  });

  it('picks a supported locale from navigator.languages', () => {
    expect(detectLocale({ language: 'ar-EG', languages: ['ar-EG', 'en-US'] })).toBe('ar');
  });

  it('marks Arabic as RTL and English as LTR', () => {
    expect(dirOf('en')).toBe('ltr');
    expect(dirOf('ar')).toBe('rtl');
  });

  it('has every supported locale translate every key with no leftover placeholders', () => {
    const keys: Array<Parameters<typeof t>[1]> = [
      'consent.none.title',
      'consent.action.allowFull',
      'status.role.fullControl',
    ];
    for (const locale of SUPPORTED_LOCALES) {
      for (const key of keys) {
        expect(t(locale, key)).not.toBe('');
      }
    }
  });

  it('interpolates the peer name into the request title', () => {
    expect(t('en', 'consent.request.title', 'guest-ab12')).toBe('guest-ab12 wants to connect');
  });
});
