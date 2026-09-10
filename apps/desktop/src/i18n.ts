// Design doc §19 phase 6: consent screen must be localized in at least two
// languages and support RTL. Arabic is chosen for the second locale precisely
// because it is RTL, not just a second LTR translation — that is the only way
// the `dir` switch actually gets exercised.

import { ar } from './locales/ar';
import { de } from './locales/de';
import { en } from './locales/en';
import { es } from './locales/es';
import { fr } from './locales/fr';
import { it } from './locales/it';
import { ja } from './locales/ja';
import { pl } from './locales/pl';
import { pt } from './locales/pt';
import { ru } from './locales/ru';
import { tr } from './locales/tr';
import { uk } from './locales/uk';
import { zh } from './locales/zh';

export type Locale = 'en' | 'ar' | 'ru' | 'de' | 'fr' | 'es' | 'pt' | 'it' | 'tr' | 'uk' | 'pl' | 'zh' | 'ja';

export const SUPPORTED_LOCALES: readonly Locale[] = [
  'en',
  'ar',
  'ru',
  'de',
  'fr',
  'es',
  'pt',
  'it',
  'tr',
  'uk',
  'pl',
  'zh',
  'ja',
];
export const DEFAULT_LOCALE: Locale = 'en';

// Each language's own name for itself, for the manual picker in the settings
// panel — a menu of languages is only readable if a person who doesn't yet
// read the current UI language can still find their own in it.
export const LOCALE_NAMES: Record<Locale, string> = {
  en: 'English',
  ar: 'العربية',
  ru: 'Русский',
  de: 'Deutsch',
  fr: 'Français',
  es: 'Español',
  pt: 'Português',
  it: 'Italiano',
  tr: 'Türkçe',
  uk: 'Українська',
  pl: 'Polski',
  zh: '中文',
  ja: '日本語',
};

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
  | 'invite.refresh.note'
  | 'invite.connectLabel'
  | 'invite.connect'
  | 'invite.connectPlaceholder'
  | 'invite.connecting'
  | 'invite.connecting.dialing'
  | 'invite.connecting.awaitingConsent'
  | 'invite.connecting.awaitingCredentials'
  | 'invite.cancel'
  | 'invite.denied'
  | 'invite.failed'
  | 'invite.unreachable'
  | 'invite.badTicket'
  | 'invite.offline'
  | 'invite.versionMismatch'
  | 'status.inputOn'
  | 'status.inputOff'
  | 'status.revoke'
  | 'status.secureDesktop.active'
  | 'status.recording.start'
  | 'status.recording.stop'
  | 'status.recording.on'
  | 'status.recording.needsGrant'
  | 'status.recording.requested'
  | 'status.recording.allow'
  | 'status.recording.decline'
  | 'status.recording.banner'
  | 'recordings.heading'
  | 'recordings.empty'
  | 'recordings.export'
  | 'recordings.exportAgain'
  | 'recordings.exporting'
  | 'recordings.exportedTo'
  | 'recordings.exportedNothing'
  | 'recordings.exportFailed'
  | 'recordings.megabytes'
  | 'recordings.kilobytes'
  | 'audit.heading'
  | 'audit.empty'
  | 'audit.disabled'
  | 'audit.filterFrom'
  | 'audit.filterTo'
  | 'audit.filterKind'
  | 'audit.filterAll'
  | 'audit.apply'
  | 'audit.time'
  | 'audit.peer'
  | 'audit.event'
  | 'audit.detail'
  | 'audit.export'
  | 'audit.exported'
  | 'audit.exportFailed'
  | 'audit.clear'
  | 'audit.clearConfirm'
  | 'audit.clearYes'
  | 'audit.clearNo'
  | 'audit.cleared'
  | 'audit.clearFailed'
  | 'audit.loadFailed'
  | 'audit.kind.consent_requested'
  | 'audit.kind.consent_granted'
  | 'audit.kind.consent_revoked'
  | 'audit.kind.consent_rejected_queue_full'
  | 'audit.kind.consent_rejected_guest_limit'
  | 'audit.kind.input_toggled'
  | 'audit.kind.recording_toggled'
  | 'audit.kind.file_action'
  | 'audit.kind.protocol_violation'
  | 'audit.kind.grant_changed'
  | 'audit.kind.unattended_login'
  | 'audit.kind.device_trust_changed'
  | 'system.heading'
  | 'system.autostart'
  | 'system.autostartNote'
  | 'system.autostartFailed'
  | 'system.checkUpdates'
  | 'system.checking'
  | 'system.upToDate'
  | 'system.available'
  | 'system.installUpdate'
  | 'system.installing'
  | 'system.installedRestart'
  | 'system.updateFailed'
  | 'system.language'
  | 'system.language.systemDefault'
  | 'status.clipboardSynced'
  | 'status.reconnect'
  | 'history.remove'
  | 'history.remove.confirm'
  | 'history.forgetPassword'
  | 'history.forgetPassword.hint'
  | 'history.forgetPassword.confirm'
  | 'status.lastSeenJustNow'
  | 'status.lastSeenMinutesAgo'
  | 'status.lastSeenHoursAgo'
  | 'status.lastSeenDaysAgo'
  | 'status.role.viewOnly'
  | 'status.role.controlLimited'
  | 'status.role.fullControl'
  | 'status.ready'
  | 'status.notReady'
  | 'status.noCapture'
  | 'status.noEncoder'
  | 'titlebar.minimize'
  | 'titlebar.maximize'
  | 'titlebar.close'
  | 'sidebar.inviteLabel'
  | 'sidebar.copyCode'
  | 'sidebar.copied'
  | 'sidebar.serverless'
  | 'sidebar.settings'
  | 'settings.heading'
  | 'settings.close'
  | 'settings.tab.access'
  | 'settings.tab.recordings'
  | 'settings.tab.system'
  | 'settings.tabs.label'
  | 'panel.heading'
  | 'panel.subtext'
  | 'connections.header'
  | 'connections.refresh'
  | 'connections.emptyTitle'
  | 'connections.emptySubtext'
  | 'hostbar.expand'
  | 'hostbar.collapse'
  | 'hostbar.openApp'
  | 'view.canvasLabel'
  | 'view.waiting'
  | 'view.reconnecting'
  | 'view.failed.title'
  | 'view.failed.body'
  | 'view.failed.dismiss'
  | 'view.unavailable.title'
  | 'view.unavailable.noCapture'
  | 'view.unavailable.noEncoder'
  | 'view.unavailable.dismiss'
  | 'view.recording'
  | 'chat.logLabel'
  | 'chat.inputLabel'
  | 'chat.inputPlaceholder'
  | 'chat.send'
  | 'chat.open'
  | 'chat.close'
  | 'files.heading'
  | 'files.sendHint'
  | 'files.fromClipboard'
  | 'files.directory'
  | 'files.accept'
  | 'files.decline'
  | 'files.cancel'
  | 'files.incoming'
  | 'files.outgoing'
  | 'files.state.completed'
  | 'files.state.cancelled'
  | 'files.state.failed'
  | 'fileManager.heading'
  | 'fileManager.local'
  | 'fileManager.remote'
  | 'fileManager.up'
  | 'fileManager.loading'
  | 'fileManager.empty'
  | 'fileManager.truncated'
  | 'fileManager.localUnreadable'
  | 'fileManager.folder'
  | 'fileManager.download'
  | 'fileManager.upload'
  | 'fileManager.refused.notGranted'
  | 'fileManager.refused.badPath'
  | 'fileManager.refused.unreadable'
  | 'fileManager.download.notGranted'
  | 'fileManager.download.badPath'
  | 'fileManager.download.unreadable'
  | 'fileManager.download.tooMany'
  | 'toolbar.dragHandle'
  | 'toolbar.settings'
  | 'toolbar.settings.placeholder'
  | 'toolbar.monitors'
  | 'toolbar.monitors.empty'
  | 'toolbar.monitors.entry'
  | 'toolbar.chat'
  | 'toolbar.chat.unread'
  | 'tunnel.heading'
  | 'tunnel.active'
  | 'tunnel.streams'
  | 'tunnel.none'
  | 'tunnel.addressLabel'
  | 'tunnel.addressPlaceholder'
  | 'tunnel.allow'
  | 'tunnel.deny'
  | 'tunnel.closeAll'
  | 'tunnel.badAddress'
  | 'toolbar.files'
  | 'toolbar.mic'
  | 'toolbar.cad'
  | 'toolbar.record'
  | 'toolbar.record.asked'
  | 'toolbar.collapse'
  | 'toolbar.expand'
  | 'toolbar.fullscreen'
  | 'toolbar.fullscreen.exit'
  | 'toolbar.settings.displayMode'
  | 'toolbar.settings.localCursor'
  | 'toolbar.settings.cursorEmbedded'
  | 'toolbar.settings.quality'
  | 'toolbar.quality.performance'
  | 'toolbar.quality.balance'
  | 'toolbar.quality.quality'
  | 'toolbar.settings.zoom'
  | 'toolbar.zoom.in'
  | 'toolbar.zoom.out'
  | 'toolbar.settings.hostResolution'
  | 'toolbar.settings.hostResolutionWarning'
  | 'toolbar.hostResolution.empty.notGranted'
  | 'toolbar.hostResolution.empty.platformUnsupported'
  | 'toolbar.hostResolution.empty.noModesReported'
  | 'toolbar.display.fit'
  | 'toolbar.display.actual'
  | 'toolbar.display.scaled'
  | 'toolbar.hotkeys'
  | 'toolbar.hotkey.toggle-fullscreen'
  | 'toolbar.hotkey.cycle-display-mode'
  | 'toolbar.hotkey.reset-view'
  | 'toolbar.hotkey.toggle-chat'
  | 'toolbar.hotkey.send-cad'
  | 'toolbar.hotkey.toggle-toolbar'
  | 'unattended.heading'
  | 'unattended.explain'
  | 'unattended.indicator'
  | 'unattended.indicator.title'
  | 'unattended.state.on'
  | 'unattended.state.off'
  | 'unattended.password.label'
  | 'unattended.password.placeholder'
  | 'unattended.password.set'
  | 'unattended.password.change'
  | 'unattended.password.saved'
  | 'unattended.disable'
  | 'unattended.disable.confirm'
  | 'unattended.totp.label'
  | 'unattended.totp.on'
  | 'unattended.totp.off'
  | 'unattended.totp.secretHeading'
  | 'unattended.totp.secretBody'
  | 'unattended.totp.uriLabel'
  | 'unattended.totp.done'
  | 'unattended.role.label'
  | 'unattended.needsTrust'
  | 'book.heading'
  | 'book.explain'
  | 'book.empty'
  | 'book.name.label'
  | 'book.tags.label'
  | 'book.notes.label'
  | 'book.save'
  | 'book.remove'
  | 'book.remove.confirm'
  | 'book.trusted'
  | 'book.untrusted'
  | 'book.trust.confirmTitle'
  | 'book.trust.confirmBody'
  | 'book.trust.confirmAction'
  | 'book.trust.cancel'
  | 'book.untrust.confirm'
  | 'book.filter.label'
  | 'book.filter.all'
  | 'book.connected'
  | 'book.addFromSession'
  | 'creds.heading'
  | 'creds.body'
  | 'creds.password.label'
  | 'creds.password.placeholder'
  | 'creds.code.label'
  | 'creds.code.placeholder'
  | 'creds.submit'
  | 'creds.close'
  | 'creds.checking'
  | 'creds.remember'
  | 'creds.badPassword'
  | 'creds.badCode'
  | 'creds.lockedOut'
  | 'creds.unavailable'
  | 'quality.path.direct'
  | 'quality.path.relay'
  | 'quality.path.mixed'
  | 'quality.path.unknown'
  | 'quality.rttLabel'
  | 'quality.lossLabel'
  | 'quality.goodputLabel'
  | 'quality.bitrateLabel'
  | 'quality.fpsLabel'
  | 'quality.relayLabel'
  | 'quality.ms'
  | 'quality.percent'
  | 'quality.kbps'
  | 'quality.fpsValue'
  | 'quality.unknown';

export type Dictionary = Record<TranslationKey, string | ((arg: string) => string)>;

const dictionaries: Record<Locale, Dictionary> = {
  en,
  ar,
  ru,
  de,
  fr,
  es,
  pt,
  it,
  tr,
  uk,
  pl,
  zh,
  ja,
};

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

// The manual language picker (settings panel) is a single-window UI setting,
// not session state, so it lives in localStorage rather than behind an
// ActorStores path (which is for real on-disk stores shared across test
// actors) or the read-only config.rs.
const LOCALE_STORAGE_KEY = 'lumepeer.locale';

/** The language a person picked by hand, or null for "system default". */
export function getStoredLocaleChoice(): Locale | null {
  try {
    const raw = localStorage.getItem(LOCALE_STORAGE_KEY);
    return raw !== null && (SUPPORTED_LOCALES as readonly string[]).includes(raw)
      ? (raw as Locale)
      : null;
  } catch {
    // Storage can be unavailable (private browsing, disabled site data). The
    // picker still works for the running session, it just won't survive a
    // restart.
    return null;
  }
}

/** Saves a manual choice, or clears it when `choice` is null ("system default"). */
export function setStoredLocaleChoice(choice: Locale | null): void {
  try {
    if (choice === null) {
      localStorage.removeItem(LOCALE_STORAGE_KEY);
    } else {
      localStorage.setItem(LOCALE_STORAGE_KEY, choice);
    }
  } catch {
    // Nothing to recover: the choice still applies to this running session.
  }
}

/** Resolution order: saved choice, then the OS/webview locale, then the default. */
export function resolveLocale(nav: Pick<Navigator, 'language' | 'languages'>): Locale {
  return getStoredLocaleChoice() ?? detectLocale(nav);
}

export function t(locale: Locale, key: TranslationKey, arg?: string): string {
  const entry = dictionaries[locale][key];
  return typeof entry === 'function' ? entry(arg ?? '') : entry;
}
