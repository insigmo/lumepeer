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
  | 'invite.refresh'
  | 'invite.connectLabel'
  | 'invite.connect'
  | 'invite.connectPlaceholder'
  | 'invite.connecting'
  | 'invite.denied'
  | 'invite.failed'
  | 'status.inputOn'
  | 'status.inputOff'
  | 'status.revoke'
  | 'status.reconnect'
  | 'status.endedJustNow'
  | 'status.endedMinutesAgo'
  | 'status.endedHoursAgo'
  | 'status.endedDaysAgo'
  | 'status.role.viewOnly'
  | 'status.role.controlLimited'
  | 'status.role.fullControl'
  | 'status.ready'
  | 'status.notReady'
  | 'titlebar.minimize'
  | 'titlebar.maximize'
  | 'titlebar.close'
  | 'sidebar.inviteLabel'
  | 'sidebar.copyCode'
  | 'sidebar.copied'
  | 'sidebar.serverless'
  | 'panel.heading'
  | 'panel.subtext'
  | 'connections.header'
  | 'connections.refresh'
  | 'connections.emptyTitle'
  | 'connections.emptySubtext'
  | 'view.canvasLabel'
  | 'view.waiting'
  | 'view.reconnecting'
  | 'view.failed.title'
  | 'view.failed.body'
  | 'view.failed.dismiss'
  | 'chat.logLabel'
  | 'chat.inputLabel'
  | 'chat.inputPlaceholder'
  | 'chat.send'
  | 'chat.open'
  | 'chat.close';

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
  'invite.refresh': 'Refresh invite',
  'invite.connectLabel': 'Enter invite code:',
  'invite.connect': 'Connect',
  'invite.connectPlaceholder': 'Paste invite code here',
  'invite.connecting': 'Connecting',
  'invite.denied': 'The other device declined the request.',
  'invite.failed': 'The connection ended before it was accepted.',
  'status.inputOn': 'input on',
  'status.inputOff': 'input off',
  'status.revoke': 'Revoke',
  'status.reconnect': 'Connect again',
  'status.endedJustNow': 'Ended just now',
  'status.endedMinutesAgo': (n) => `Ended ${n}m ago`,
  'status.endedHoursAgo': (n) => `Ended ${n}h ago`,
  'status.endedDaysAgo': (n) => `Ended ${n}d ago`,
  'status.role.viewOnly': 'view only',
  'status.role.controlLimited': 'limited control',
  'status.role.fullControl': 'full control',
  'status.ready': 'Ready to connect',
  'status.notReady': 'Not ready to connect',
  'titlebar.minimize': 'Minimize',
  'titlebar.maximize': 'Maximize',
  'titlebar.close': 'Close',
  'sidebar.inviteLabel': 'Your invite code',
  'sidebar.copyCode': 'Copy code',
  'sidebar.copied': 'Copied',
  'sidebar.serverless': 'P2P · serverless',
  'panel.heading': 'Connect to device',
  'panel.subtext': 'Paste an invite code to connect to a remote device.',
  'connections.header': 'Connections',
  'connections.refresh': 'Refresh',
  'connections.emptyTitle': 'No connections yet',
  'connections.emptySubtext': 'Connected devices will appear here.',
  'view.canvasLabel': 'Remote screen',
  'view.waiting': 'Waiting for the remote screen…',
  'view.reconnecting': 'Connection lost, reconnecting…',
  'view.failed.title': 'Connection lost',
  'view.failed.body': 'The remote screen could not be reconnected, so the session has ended.',
  'view.failed.dismiss': 'Close',
  'chat.logLabel': 'Chat',
  'chat.inputLabel': 'Chat message',
  'chat.inputPlaceholder': 'Type a message…',
  'chat.send': 'Send',
  'chat.open': 'Chat',
  'chat.close': 'Close chat',
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
  'invite.refresh': 'تحديث الدعوة',
  'invite.connectLabel': 'أدخل رمز الدعوة:',
  'invite.connect': 'الاتصال',
  'invite.connectPlaceholder': 'الصق رمز الدعوة هنا',
  'invite.connecting': 'جارٍ الاتصال',
  'invite.denied': 'رفض الجهاز الآخر الطلب.',
  'invite.failed': 'انتهى الاتصال قبل قبوله.',
  'status.inputOn': 'الإدخال مفعّل',
  'status.inputOff': 'الإدخال معطّل',
  'status.revoke': 'إلغاء',
  'status.reconnect': 'الاتصال مرة أخرى',
  'status.endedJustNow': 'انتهت للتو',
  'status.endedMinutesAgo': (n) => `انتهت قبل ${n} د`,
  'status.endedHoursAgo': (n) => `انتهت قبل ${n} س`,
  'status.endedDaysAgo': (n) => `انتهت قبل ${n} يوم`,
  'status.role.viewOnly': 'مشاهدة فقط',
  'status.role.controlLimited': 'تحكم محدود',
  'status.role.fullControl': 'تحكم كامل',
  'status.ready': 'جاهز للاتصال',
  'status.notReady': 'غير جاهز للاتصال',
  'titlebar.minimize': 'تصغير',
  'titlebar.maximize': 'تكبير',
  'titlebar.close': 'إغلاق',
  'sidebar.inviteLabel': 'رمز الدعوة الخاص بك',
  'sidebar.copyCode': 'نسخ الرمز',
  'sidebar.copied': 'تم النسخ',
  'sidebar.serverless': 'اتصال مباشر · بلا خوادم',
  'panel.heading': 'الاتصال بجهاز',
  'panel.subtext': 'الصق رمز الدعوة للاتصال بجهاز بعيد.',
  'connections.header': 'الاتصالات',
  'connections.refresh': 'تحديث',
  'connections.emptyTitle': 'لا توجد اتصالات بعد',
  'connections.emptySubtext': 'ستظهر الأجهزة المتصلة هنا.',
  'view.canvasLabel': 'الشاشة البعيدة',
  'view.waiting': 'في انتظار الشاشة البعيدة…',
  'view.reconnecting': 'انقطع الاتصال، جارٍ إعادة الاتصال…',
  'view.failed.title': 'انقطع الاتصال',
  'view.failed.body': 'تعذّرت إعادة الاتصال بالشاشة البعيدة، لذلك انتهت الجلسة.',
  'view.failed.dismiss': 'إغلاق',
  'chat.logLabel': 'المحادثة',
  'chat.inputLabel': 'رسالة المحادثة',
  'chat.inputPlaceholder': 'اكتب رسالة…',
  'chat.send': 'إرسال',
  'chat.open': 'المحادثة',
  'chat.close': 'إغلاق المحادثة',
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
