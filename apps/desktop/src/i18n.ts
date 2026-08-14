// Design doc §19 phase 6: consent screen must be localized in at least two
// languages and support RTL. Arabic is chosen for the second locale precisely
// because it is RTL, not just a second LTR translation — that is the only way
// the `dir` switch actually gets exercised.

export type Locale = 'en' | 'ar';

export const SUPPORTED_LOCALES: readonly Locale[] = ['en', 'ar'];
export const DEFAULT_LOCALE: Locale = 'en';

export type TranslationKey =
  | 'consent.none.title'
  | 'consent.none.body'
  | 'consent.request.title'
  | 'consent.request.body'
  | 'consent.action.deny'
  | 'consent.action.allowView'
  | 'consent.action.allowFull'
  | 'invite.heading'
  | 'invite.create'
  | 'invite.qrAlt'
  | 'invite.connectLabel'
  | 'invite.connect'
  | 'status.notSharing'
  | 'status.heading'
  | 'status.inputOn'
  | 'status.inputOff'
  | 'status.revoke'
  | 'status.role.viewOnly'
  | 'status.role.controlLimited'
  | 'status.role.fullControl';

type Dictionary = Record<TranslationKey, string | ((arg: string) => string)>;

const en: Dictionary = {
  'consent.none.title': 'No pending requests',
  'consent.none.body': 'Nobody is asking to connect right now.',
  'consent.request.title': (peer) => `${peer} wants to connect`,
  'consent.request.body':
    'Granting view lets them see this screen. Input, clipboard, files and recording stay off until you enable each one separately.',
  'consent.action.deny': 'Deny',
  'consent.action.allowView': 'Allow view only',
  'consent.action.allowFull': 'Allow full control',
  'invite.heading': 'Invite and connect',
  'invite.create': 'Create invite',
  'invite.qrAlt': 'QR code',
  'invite.connectLabel': 'Enter invite code:',
  'invite.connect': 'Connect',
  'status.notSharing': 'Not sharing.',
  'status.heading': 'Active sessions',
  'status.inputOn': 'input on',
  'status.inputOff': 'input off',
  'status.revoke': 'Revoke',
  'status.role.viewOnly': 'view only',
  'status.role.controlLimited': 'limited control',
  'status.role.fullControl': 'full control',
};

const ar: Dictionary = {
  'consent.none.title': 'لا توجد طلبات معلّقة',
  'consent.none.body': 'لا أحد يطلب الاتصال الآن.',
  'consent.request.title': (peer) => `${peer} يريد الاتصال`,
  'consent.request.body':
    'منح المشاهدة يتيح لهم رؤية هذه الشاشة. الإدخال والحافظة والملفات والتسجيل تبقى معطّلة حتى تفعّل كلاً منها بشكل منفصل.',
  'consent.action.deny': 'رفض',
  'consent.action.allowView': 'السماح بالمشاهدة فقط',
  'consent.action.allowFull': 'السماح بالتحكم الكامل',
  'invite.heading': 'الدعوة والاتصال',
  'invite.create': 'إنشاء دعوة',
  'invite.qrAlt': 'رمز الاستجابة السريعة',
  'invite.connectLabel': 'أدخل رمز الدعوة:',
  'invite.connect': 'الاتصال',
  'status.notSharing': 'لا مشاركة حالياً.',
  'status.heading': 'الجلسات النشطة',
  'status.inputOn': 'الإدخال مفعّل',
  'status.inputOff': 'الإدخال معطّل',
  'status.revoke': 'إلغاء',
  'status.role.viewOnly': 'مشاهدة فقط',
  'status.role.controlLimited': 'تحكم محدود',
  'status.role.fullControl': 'تحكم كامل',
};

const dictionaries: Record<Locale, Dictionary> = { en, ar };

export function dirOf(locale: Locale): 'ltr' | 'rtl' {
  return locale === 'ar' ? 'rtl' : 'ltr';
}

export function detectLocale(nav: Pick<Navigator, 'language' | 'languages'>): Locale {
  const candidates = nav.languages && nav.languages.length > 0 ? nav.languages : [nav.language];
  for (const raw of candidates) {
    const base = raw.split('-')[0]?.toLowerCase();
    if (base && (SUPPORTED_LOCALES as readonly string[]).includes(base)) {
      return base as Locale;
    }
  }
  return DEFAULT_LOCALE;
}

export function t(locale: Locale, key: TranslationKey, arg?: string): string {
  const entry = dictionaries[locale][key];
  return typeof entry === 'function' ? entry(arg ?? '') : entry;
}
