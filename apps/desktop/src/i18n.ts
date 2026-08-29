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
  | 'status.grants.heading'
  | 'status.grants.clipboardRead'
  | 'status.grants.clipboardWrite'
  | 'status.grants.fileTransfer'
  | 'status.grants.recording'
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
  | 'system.serviceRunning'
  | 'system.serviceOff'
  | 'system.serviceInstall'
  | 'system.serviceRemove'
  | 'system.serviceWorking'
  | 'system.serviceNote'
  | 'system.serviceFailed'
  | 'status.clipboardSynced'
  | 'status.reconnect'
  | 'history.remove'
  | 'history.remove.confirm'
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
  | 'panel.heading'
  | 'panel.subtext'
  | 'connections.header'
  | 'connections.refresh'
  | 'connections.emptyTitle'
  | 'connections.emptySubtext'
  | 'view.canvasLabel'
  | 'view.waiting'
  | 'view.reconnecting'
  | 'view.secureDesktop'
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
  | 'files.send'
  | 'files.accept'
  | 'files.decline'
  | 'files.cancel'
  | 'files.incoming'
  | 'files.outgoing'
  | 'files.state.completed'
  | 'files.state.cancelled'
  | 'files.state.failed'
  | 'toolbar.dragHandle'
  | 'toolbar.settings'
  | 'toolbar.settings.placeholder'
  | 'toolbar.monitors'
  | 'toolbar.monitors.empty'
  | 'toolbar.monitors.entry'
  | 'toolbar.chat'
  | 'toolbar.chat.unread'
  | 'toolbar.mic'
  | 'toolbar.cad'
  | 'toolbar.record'
  | 'toolbar.record.asked'
  | 'toolbar.clipboard'
  | 'toolbar.file'
  | 'toolbar.collapse'
  | 'toolbar.expand'
  | 'toolbar.fullscreen'
  | 'toolbar.fullscreen.exit'
  | 'toolbar.settings.displayMode'
  | 'toolbar.settings.localCursor'
  | 'toolbar.settings.cursorEmbedded'
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
  // Used by the settings window (docs/bugs/05); the sidebar no longer offers
  // it, because reissuing retires every code already handed out (ADR 0016).
  'invite.refresh': 'Revoke current code and issue a new one',
  'invite.refresh.note': 'The old code will stop working.',
  'invite.connectLabel': 'Enter invite code:',
  'invite.connect': 'Connect',
  'invite.connectPlaceholder': 'Paste invite code here',
  'invite.connecting': 'Connecting',
  'invite.connecting.dialing': 'Connecting',
  'invite.connecting.awaitingConsent': 'Waiting for the other device to answer',
  'invite.connecting.awaitingCredentials': 'The device is asking for a password',
  'invite.cancel': 'Cancel',
  'invite.denied': 'The other device declined the request.',
  'invite.failed': 'The connection ended before it was accepted.',
  'invite.unreachable':
    'Could not reach that device. It may be offline, or its invite code may be out of date — ask for a fresh one.',
  'invite.badTicket': 'That invite code is not valid, or it has expired.',
  'invite.offline':
    'This device is not reachable from the internet yet. Wait for the status to turn ready, then try again.',
  'invite.versionMismatch': 'The other device runs an incompatible version of Lumepeer.',
  'status.inputOn': 'input on',
  'status.inputOff': 'input off',
  'status.revoke': 'Revoke',
  // Named as consequences, not as flag names: the person deciding has to be
  // able to read what the guest gets out of a switch being on (§19 phase 6).
  'status.grants.heading': 'What this guest may do',
  'status.grants.clipboardRead': 'Read my clipboard',
  'status.grants.clipboardWrite': 'Change my clipboard',
  'status.grants.fileTransfer': 'Send and receive files',
  'status.grants.recording': 'Let this session be recorded',
  // The switch above is permission; these are the act. Both sides see an
  // indicator for as long as a recording runs — §2.2 has no quiet capture.
  'status.recording.start': 'Record session',
  'status.recording.stop': 'Stop recording',
  'status.recording.on': 'Recording',
  'status.recording.needsGrant': 'Turn on "Let this session be recorded" first.',
  'status.recording.requested': (peer) => `${peer} asks you to record this session.`,
  'status.recording.allow': 'Start recording',
  'status.recording.decline': 'Not now',
  'status.recording.banner': 'A session is being recorded on this device.',
  'recordings.heading': 'Recordings on this device',
  'recordings.empty': 'Nothing has been recorded on this device yet.',
  'recordings.export': 'Export',
  'recordings.exportAgain': 'Export again',
  'recordings.exporting': 'Exporting...',
  'recordings.exportedTo': (tracks) => `Exported: ${tracks}`,
  'recordings.exportedNothing': 'This recording holds no picture and no sound.',
  'recordings.exportFailed': 'The export failed. Nothing was written.',
  'recordings.megabytes': (size) => `${size} MB`,
  'recordings.kilobytes': (size) => `${size} kB`,
  'audit.heading': 'Audit log',
  'audit.empty': 'Nothing was recorded in the window you asked for.',
  'audit.disabled': 'This host is running without an audit log. Nothing is being recorded.',
  'audit.filterFrom': 'From',
  'audit.filterTo': 'To',
  'audit.filterKind': 'Event',
  'audit.filterAll': 'All events',
  'audit.apply': 'Apply',
  'audit.time': 'When',
  'audit.peer': 'Device',
  'audit.event': 'Event',
  'audit.detail': 'Detail',
  'audit.export': 'Export...',
  'audit.exported': (path) => `Exported to ${path}`,
  'audit.exportFailed': 'The export failed. Nothing was written.',
  'audit.clear': 'Erase the log',
  'audit.clearConfirm': 'Erase every record? This cannot be undone.',
  'audit.clearYes': 'Erase',
  'audit.clearNo': 'Keep',
  'audit.cleared': (count) => `${count} records erased.`,
  'audit.clearFailed': 'The log could not be erased.',
  'audit.loadFailed': 'The log could not be read.',
  'audit.kind.consent_requested': 'Consent requested',
  'audit.kind.consent_granted': 'Consent granted',
  'audit.kind.consent_revoked': 'Consent revoked',
  'audit.kind.consent_rejected_queue_full': 'Refused: the request queue was full',
  'audit.kind.consent_rejected_guest_limit': 'Refused: the guest limit was reached',
  'audit.kind.input_toggled': 'Input changed',
  'audit.kind.recording_toggled': 'Recording changed',
  'audit.kind.file_action': 'File transfer',
  'audit.kind.protocol_violation': 'Protocol violation',
  'audit.kind.grant_changed': 'Permission changed',
  'audit.kind.unattended_login': 'Unattended login',
  'audit.kind.device_trust_changed': 'Device trust changed',
  'system.heading': 'This device',
  'system.autostart': 'Start Lumepeer when I sign in',
  'system.autostartNote': 'Starting with your session allows nothing on its own: Lumepeer comes up and waits for you to accept each connection. Turning this off removes the startup entry.',
  'system.autostartFailed': 'This device would not change its startup setting.',
  'system.checkUpdates': 'Check for updates',
  'system.checking': 'Checking...',
  'system.upToDate': 'You are on the newest release.',
  'system.available': (version) => `Version ${version} is available`,
  'system.installUpdate': 'Install',
  'system.installing': 'Installing...',
  'system.installedRestart': 'Installed. Restart Lumepeer to run the new version.',
  'system.updateFailed': 'The update could not be completed. Nothing was installed.',
  'system.serviceRunning': 'Ctrl+Alt+Del helper: running',
  'system.serviceOff': 'Ctrl+Alt+Del helper: not running',
  'system.serviceInstall': 'Install',
  'system.serviceRemove': 'Remove',
  'system.serviceWorking': 'Working...',
  'system.serviceNote': 'A background service that does exactly one thing: send Ctrl+Alt+Del to this screen when a remote session asks for it. It lets nobody in and can be removed here at any time. Installing or removing it asks Windows for administrator permission.',
  'system.serviceFailed': 'The helper service was not changed. Administrator permission is needed.',
  'status.clipboardSynced': 'Clipboard synced',
  'status.reconnect': 'Connect again',
  'history.remove': 'Remove',
  'history.remove.confirm': (name) => `Remove ${name} from the connection list?`,
  'status.lastSeenJustNow': 'Last seen just now',
  'status.lastSeenMinutesAgo': (n) => `Last seen ${n}m ago`,
  'status.lastSeenHoursAgo': (n) => `Last seen ${n}h ago`,
  'status.lastSeenDaysAgo': (n) => `Last seen ${n}d ago`,
  'status.role.viewOnly': 'view only',
  'status.role.controlLimited': 'limited control',
  'status.role.fullControl': 'full control',
  'status.ready': 'Ready to connect',
  'status.notReady': 'Not ready to connect',
  'status.noCapture':
    'This device has no screen capture support, so anyone you invite will see nothing. Sessions still connect, and input still works.',
  'status.noEncoder':
    'This device has no video encoder, so anyone you invite will see nothing. Sessions still connect, and input still works.',
  'titlebar.minimize': 'Minimize',
  'titlebar.maximize': 'Maximize',
  'titlebar.close': 'Close',
  'sidebar.inviteLabel': 'Your invite code',
  'sidebar.copyCode': 'Copy invite code',
  'sidebar.copied': 'Copied',
  'sidebar.serverless': 'Serverless',
  'sidebar.settings': 'Settings',
  'settings.heading': 'Settings',
  'settings.close': 'Close settings',
  'panel.heading': 'Connect to device',
  'panel.subtext': 'Paste an invite code to connect to a remote device.',
  'connections.header': 'Connections',
  'connections.refresh': 'Refresh',
  'connections.emptyTitle': 'No connections yet',
  'connections.emptySubtext': 'Connected devices will appear here.',
  'view.canvasLabel': 'Remote screen',
  'view.waiting': 'Waiting for the remote screen…',
  'view.reconnecting': 'Connection lost, reconnecting…',
  'view.secureDesktop':
    'A secure prompt (an administrator request, the lock screen, or a user switch) is showing on the remote machine. Respond to it there, or wait — the picture resumes on its own.',
  'view.failed.title': 'Connection lost',
  'view.failed.body': 'The remote screen could not be reconnected, so the session has ended.',
  'view.failed.dismiss': 'Close',
  'view.unavailable.title': 'No picture from this device',
  'view.unavailable.noCapture':
    'The other device has no screen capture support, so it cannot send its screen. The connection itself is fine.',
  'view.unavailable.noEncoder':
    'The other device has no video encoder, so it cannot send its screen. The connection itself is fine.',
  'view.unavailable.dismiss': 'Close',
  'view.recording': 'This session is being recorded',
  'chat.logLabel': 'Chat',
  'chat.inputLabel': 'Chat message',
  'chat.inputPlaceholder': 'Type a message…',
  'chat.send': 'Send',
  'chat.open': 'Chat',
  'chat.close': 'Close chat',
  'files.heading': 'Files',
  'files.send': 'Send a file',
  'files.accept': 'Accept',
  'files.decline': 'Decline',
  'files.cancel': 'Cancel',
  'files.incoming': 'Receiving',
  'files.outgoing': 'Sending',
  'files.state.completed': 'Done',
  'files.state.cancelled': 'Cancelled',
  'files.state.failed': 'Failed',
  'toolbar.dragHandle': 'Drag toolbar',
  'toolbar.settings': 'Settings',
  'toolbar.settings.placeholder': 'More settings are on the way.',
  'toolbar.monitors': 'Choose screen',
  'toolbar.monitors.empty': 'The host has not announced any screens yet.',
  'toolbar.monitors.entry': (arg) => `Screen ${arg}`,
  'toolbar.chat': 'Chat',
  'toolbar.chat.unread': 'Chat — a new message',
  'toolbar.mic': 'Microphone',
  'toolbar.cad': 'Ctrl+Alt+Del',
  'toolbar.record': 'Ask the host to record this session',
  'toolbar.record.asked': 'Asked the host to record; waiting for an answer',
  'toolbar.clipboard': 'Send my clipboard to the host',
  'toolbar.file': 'Send a file to the host',
  'toolbar.collapse': 'Collapse',
  'toolbar.expand': 'Expand',
  'toolbar.fullscreen': 'Full screen',
  'toolbar.fullscreen.exit': 'Leave full screen',
  'toolbar.settings.displayMode': 'Picture size',
  'toolbar.settings.localCursor': 'Draw the pointer here',
  // Said as a fact about the other machine, not as a setting that is missing:
  // on this host the pointer is part of the picture and nothing here can
  // change that.
  'toolbar.settings.cursorEmbedded': 'This device sends the pointer inside the picture.',
  // Named for what the operator sees, not for the arithmetic: "actual size"
  // is a promise about pixels, "1:1" is a formula.
  'toolbar.display.fit': 'Fit to window',
  'toolbar.display.actual': 'Actual size',
  'toolbar.display.scaled': 'Zoom',
  'toolbar.hotkeys': 'Keyboard shortcuts',
  'toolbar.hotkey.toggle-fullscreen': 'Full screen on or off',
  'toolbar.hotkey.cycle-display-mode': 'Next picture size',
  'toolbar.hotkey.reset-view': 'Fit the picture again and centre it',
  'toolbar.hotkey.toggle-chat': 'Chat on or off',
  'toolbar.hotkey.send-cad': 'Send Ctrl+Alt+Del',
  'toolbar.hotkey.toggle-toolbar': 'Collapse or expand this toolbar',
  'unattended.heading': 'Unattended access',
  'unattended.explain': 'With this on, a device you have marked trusted can start a session by entering this device password — nobody has to be sitting here to approve it. This banner stays up whenever it is on.',
  'unattended.indicator': 'Unattended access is on',
  'unattended.indicator.title':
    'Unattended access is on: trusted devices can connect with the device password.',
  'unattended.state.on': 'On',
  'unattended.state.off': 'Off',
  'unattended.password.label': 'Device password',
  'unattended.password.placeholder': 'At least 8 characters',
  'unattended.password.set': 'Turn on',
  'unattended.password.change': 'Change password',
  'unattended.password.saved': 'Password saved',
  'unattended.disable': 'Turn off',
  'unattended.disable.confirm': 'Turn unattended access off? The device password and any second factor are deleted, and trusted devices will need someone here to approve them again.',
  'unattended.totp.label': 'Also require a one-time code',
  'unattended.totp.on': 'Required',
  'unattended.totp.off': 'Not required',
  'unattended.totp.secretHeading': 'Add this to your authenticator app',
  'unattended.totp.secretBody': 'This is shown once. If you lose it, turn the code off and on again to get a new one.',
  'unattended.totp.uriLabel': 'Setup link',
  'unattended.totp.done': 'Done',
  'unattended.role.label': 'A device that logs in this way gets',
  'unattended.needsTrust': 'Set a device password first.',
  'book.heading': 'Saved devices',
  'book.explain': 'Trusting a device does not let it in on its own. It decides who is allowed to try the device password at all.',
  'book.empty': 'No devices saved yet. Save a device from a connection to add it here.',
  'book.name.label': 'Name',
  'book.tags.label': 'Tags, comma separated',
  'book.notes.label': 'Note',
  'book.save': 'Save',
  'book.remove': 'Forget',
  'book.remove.confirm': (name) => `Forget ${name}? Any trust it has is removed with it.`,
  'book.trusted': 'Trusted',
  'book.untrusted': 'Not trusted',
  'book.trust.confirmTitle': (name) => `Trust ${name}?`,
  'book.trust.confirmBody': 'This device will be allowed to start a session by entering the device password, with nobody here to approve it. It still needs the password, and the one-time code if you require one.',
  'book.trust.confirmAction': 'Yes, trust this device',
  'book.trust.cancel': 'Cancel',
  'book.untrust.confirm': (name) => `Stop trusting ${name}? It will need someone here to approve it again.`,
  'book.filter.label': 'Filter by tag',
  'book.filter.all': 'All tags',
  'book.connected': 'Connected now',
  'book.addFromSession': 'Save this device',
  'creds.heading': 'This device asks for a password',
  'creds.body': 'Nobody is at the other end to approve you. Enter the device password its owner gave you.',
  'creds.password.label': 'Device password',
  'creds.password.placeholder': 'Device password',
  'creds.code.label': 'One-time code',
  'creds.code.placeholder': '6 digits',
  'creds.submit': 'Sign in',
  'creds.checking': 'Checking',
  'creds.remember': "Remember this device's password",
  'creds.badPassword': 'That password was not accepted.',
  'creds.badCode': 'That code was not accepted.',
  'creds.lockedOut': (secs) => `Too many attempts. Try again in ${secs} seconds.`,
  'creds.unavailable': 'This device cannot sign you in that way right now.',
  // Named for what is happening, not for the transport that makes it happen:
  // "via relay" is something a person can act on, "DERP" is not (§18).
  'quality.path.direct': 'Direct',
  'quality.path.relay': 'Via relay',
  'quality.path.mixed': 'Direct and relay',
  'quality.path.unknown': 'Path not known yet',
  'quality.rttLabel': 'Round trip',
  'quality.lossLabel': 'Frames lost',
  'quality.goodputLabel': 'Received',
  'quality.bitrateLabel': 'Sending at',
  'quality.fpsLabel': 'Frame rate',
  'quality.relayLabel': 'Relay region',
  'quality.ms': (value) => `${value} ms`,
  'quality.percent': (value) => `${value}%`,
  'quality.kbps': (value) => `${value} kbit/s`,
  'quality.fpsValue': (value) => `${value} fps`,
  // Nothing has measured this yet, which is a different fact from zero.
  'quality.unknown': 'not measured yet',
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
  'invite.refresh': 'إبطال الرمز الحالي وإصدار رمز جديد',
  'invite.refresh.note': 'لن يعمل الرمز القديم بعد الآن.',
  'invite.connectLabel': 'أدخل رمز الدعوة:',
  'invite.connect': 'الاتصال',
  'invite.connectPlaceholder': 'الصق رمز الدعوة هنا',
  'invite.connecting': 'جارٍ الاتصال',
  'invite.connecting.dialing': 'جارٍ الاتصال',
  'invite.connecting.awaitingConsent': 'في انتظار رد الجهاز الآخر',
  'invite.connecting.awaitingCredentials': 'يطلب الجهاز كلمة مرور',
  'invite.cancel': 'إلغاء',
  'invite.denied': 'رفض الجهاز الآخر الطلب.',
  'invite.failed': 'انتهى الاتصال قبل قبوله.',
  'invite.unreachable':
    'تعذّر الوصول إلى ذلك الجهاز. قد يكون غير متصل، أو أن رمز الدعوة قديم — اطلب رمزًا جديدًا.',
  'invite.badTicket': 'رمز الدعوة غير صالح أو انتهت صلاحيته.',
  'invite.offline':
    'هذا الجهاز غير قابل للوصول من الإنترنت بعد. انتظر حتى تصبح الحالة جاهزة ثم أعد المحاولة.',
  'invite.versionMismatch': 'يعمل الجهاز الآخر بإصدار غير متوافق من Lumepeer.',
  'status.inputOn': 'الإدخال مفعّل',
  'status.inputOff': 'الإدخال معطّل',
  'status.revoke': 'إلغاء',
  'status.grants.heading': 'ما الذي يُسمح به لهذا الضيف',
  'status.grants.clipboardRead': 'قراءة الحافظة الخاصة بي',
  'status.grants.clipboardWrite': 'تغيير الحافظة الخاصة بي',
  'status.grants.fileTransfer': 'إرسال الملفات واستقبالها',
  'status.grants.recording': 'السماح بتسجيل هذه الجلسة',
  'status.recording.start': 'تسجيل الجلسة',
  'status.recording.stop': 'إيقاف التسجيل',
  'status.recording.on': 'جارٍ التسجيل',
  'status.recording.needsGrant': 'فعّل «السماح بتسجيل هذه الجلسة» أولاً.',
  'status.recording.requested': (peer) => `${peer} يطلب منك تسجيل هذه الجلسة.`,
  'status.recording.allow': 'بدء التسجيل',
  'status.recording.decline': 'ليس الآن',
  'status.recording.banner': 'يجري تسجيل جلسة على هذا الجهاز.',
  'recordings.heading': 'التسجيلات على هذا الجهاز',
  'recordings.empty': 'لم يُسجَّل شيء على هذا الجهاز بعد.',
  'recordings.export': 'تصدير',
  'recordings.exportAgain': 'تصدير مرة أخرى',
  'recordings.exporting': 'جارٍ التصدير...',
  'recordings.exportedTo': (tracks) => `تم التصدير: ${tracks}`,
  'recordings.exportedNothing': 'لا يحتوي هذا التسجيل على صورة ولا صوت.',
  'recordings.exportFailed': 'فشل التصدير. لم يُكتب أي ملف.',
  'recordings.megabytes': (size) => `${size} م.ب`,
  'recordings.kilobytes': (size) => `${size} ك.ب`,
  'audit.heading': 'سجل التدقيق',
  'audit.empty': 'لا توجد سجلات ضمن النطاق المطلوب.',
  'audit.disabled': 'يعمل هذا المضيف بدون سجل تدقيق. لا يُسجَّل شيء.',
  'audit.filterFrom': 'من',
  'audit.filterTo': 'إلى',
  'audit.filterKind': 'الحدث',
  'audit.filterAll': 'كل الأحداث',
  'audit.apply': 'تطبيق',
  'audit.time': 'الوقت',
  'audit.peer': 'الجهاز',
  'audit.event': 'الحدث',
  'audit.detail': 'التفاصيل',
  'audit.export': 'تصدير...',
  'audit.exported': (path) => `تم التصدير إلى ${path}`,
  'audit.exportFailed': 'فشل التصدير. لم يُكتب أي ملف.',
  'audit.clear': 'محو السجل',
  'audit.clearConfirm': 'محو كل السجلات؟ لا يمكن التراجع عن ذلك.',
  'audit.clearYes': 'محو',
  'audit.clearNo': 'إبقاء',
  'audit.cleared': (count) => `تم محو ${count} سجلاً.`,
  'audit.clearFailed': 'تعذّر محو السجل.',
  'audit.loadFailed': 'تعذّرت قراءة السجل.',
  'audit.kind.consent_requested': 'طُلبت الموافقة',
  'audit.kind.consent_granted': 'مُنحت الموافقة',
  'audit.kind.consent_revoked': 'سُحبت الموافقة',
  'audit.kind.consent_rejected_queue_full': 'رُفض: طابور الطلبات ممتلئ',
  'audit.kind.consent_rejected_guest_limit': 'رُفض: بلغ حد الضيوف',
  'audit.kind.input_toggled': 'تغيّر الإدخال',
  'audit.kind.recording_toggled': 'تغيّر التسجيل',
  'audit.kind.file_action': 'نقل ملف',
  'audit.kind.protocol_violation': 'مخالفة بروتوكول',
  'audit.kind.grant_changed': 'تغيّر إذن',
  'audit.kind.unattended_login': 'دخول غير مراقب',
  'audit.kind.device_trust_changed': 'تغيّرت ثقة الجهاز',
  'system.heading': 'هذا الجهاز',
  'system.autostart': 'تشغيل Lumepeer عند تسجيل الدخول',
  'system.autostartNote': 'التشغيل مع الجلسة لا يمنح شيئًا بحد ذاته: يبدأ Lumepeer وينتظر قبولك لكل اتصال. إيقاف هذا الخيار يزيل مُدخل بدء التشغيل.',
  'system.autostartFailed': 'تعذّر تغيير إعداد بدء التشغيل على هذا الجهاز.',
  'system.checkUpdates': 'التحقق من التحديثات',
  'system.checking': 'جارٍ التحقق...',
  'system.upToDate': 'أنت على أحدث إصدار.',
  'system.available': (version) => `الإصدار ${version} متاح`,
  'system.installUpdate': 'تثبيت',
  'system.installing': 'جارٍ التثبيت...',
  'system.installedRestart': 'تم التثبيت. أعد تشغيل Lumepeer لتشغيل الإصدار الجديد.',
  'system.updateFailed': 'تعذّر إكمال التحديث. لم يُثبَّت شيء.',
  'system.serviceRunning': 'مساعد Ctrl+Alt+Del: قيد التشغيل',
  'system.serviceOff': 'مساعد Ctrl+Alt+Del: متوقف',
  'system.serviceInstall': 'تثبيت',
  'system.serviceRemove': 'إزالة',
  'system.serviceWorking': 'جارٍ التنفيذ...',
  'system.serviceNote': 'خدمة في الخلفية تقوم بشيء واحد فقط: إرسال Ctrl+Alt+Del إلى هذه الشاشة عندما تطلبه جلسة بعيدة. لا تسمح لأحد بالدخول ويمكن إزالتها من هنا في أي وقت. التثبيت أو الإزالة يطلبان إذن المسؤول من Windows.',
  'system.serviceFailed': 'لم تتغيّر الخدمة المساعدة. يلزم إذن المسؤول.',
  'status.clipboardSynced': 'تمت مزامنة الحافظة',
  'status.reconnect': 'الاتصال مرة أخرى',
  'history.remove': 'إزالة',
  'history.remove.confirm': (name) => `إزالة ${name} من قائمة الاتصالات؟`,
  'status.lastSeenJustNow': 'آخر ظهور قبل قليل',
  'status.lastSeenMinutesAgo': (n) => `آخر ظهور قبل ${n} د`,
  'status.lastSeenHoursAgo': (n) => `آخر ظهور قبل ${n} س`,
  'status.lastSeenDaysAgo': (n) => `آخر ظهور قبل ${n} يوم`,
  'status.role.viewOnly': 'مشاهدة فقط',
  'status.role.controlLimited': 'تحكم محدود',
  'status.role.fullControl': 'تحكم كامل',
  'status.ready': 'جاهز للاتصال',
  'status.notReady': 'غير جاهز للاتصال',
  'status.noCapture':
    'لا يدعم هذا الجهاز التقاط الشاشة، لذلك لن يرى من تدعوه أي صورة. تظل الجلسات تتصل ويظل الإدخال يعمل.',
  'status.noEncoder':
    'لا يوجد في هذا الجهاز مُرمِّز فيديو، لذلك لن يرى من تدعوه أي صورة. تظل الجلسات تتصل ويظل الإدخال يعمل.',
  'titlebar.minimize': 'تصغير',
  'titlebar.maximize': 'تكبير',
  'titlebar.close': 'إغلاق',
  'sidebar.inviteLabel': 'رمز الدعوة الخاص بك',
  'sidebar.copyCode': 'نسخ رمز الدعوة',
  'sidebar.copied': 'تم النسخ',
  'sidebar.serverless': 'بلا خوادم',
  'sidebar.settings': 'الإعدادات',
  'settings.heading': 'الإعدادات',
  'settings.close': 'إغلاق الإعدادات',
  'panel.heading': 'الاتصال بجهاز',
  'panel.subtext': 'الصق رمز الدعوة للاتصال بجهاز بعيد.',
  'connections.header': 'الاتصالات',
  'connections.refresh': 'تحديث',
  'connections.emptyTitle': 'لا توجد اتصالات بعد',
  'connections.emptySubtext': 'ستظهر الأجهزة المتصلة هنا.',
  'view.canvasLabel': 'الشاشة البعيدة',
  'view.waiting': 'في انتظار الشاشة البعيدة…',
  'view.reconnecting': 'انقطع الاتصال، جارٍ إعادة الاتصال…',
  'view.secureDesktop':
    'تظهر على الجهاز البعيد نافذة آمنة (طلب صلاحيات المسؤول، شاشة القفل، أو تبديل المستخدم). أجب عنها هناك، أو انتظر — ستعود الصورة من تلقاء نفسها.',
  'view.failed.title': 'انقطع الاتصال',
  'view.failed.body': 'تعذّرت إعادة الاتصال بالشاشة البعيدة، لذلك انتهت الجلسة.',
  'view.failed.dismiss': 'إغلاق',
  'view.unavailable.title': 'لا توجد صورة من هذا الجهاز',
  'view.unavailable.noCapture':
    'لا يدعم الجهاز الآخر التقاط الشاشة، لذلك لا يمكنه إرسال شاشته. الاتصال نفسه سليم.',
  'view.unavailable.noEncoder':
    'لا يوجد في الجهاز الآخر مُرمِّز فيديو، لذلك لا يمكنه إرسال شاشته. الاتصال نفسه سليم.',
  'view.unavailable.dismiss': 'إغلاق',
  'view.recording': 'يجري تسجيل هذه الجلسة',
  'chat.logLabel': 'المحادثة',
  'chat.inputLabel': 'رسالة المحادثة',
  'chat.inputPlaceholder': 'اكتب رسالة…',
  'chat.send': 'إرسال',
  'chat.open': 'المحادثة',
  'chat.close': 'إغلاق المحادثة',
  'files.heading': 'الملفات',
  'files.send': 'إرسال ملف',
  'files.accept': 'قبول',
  'files.decline': 'رفض',
  'files.cancel': 'إلغاء',
  'files.incoming': 'جارٍ الاستقبال',
  'files.outgoing': 'جارٍ الإرسال',
  'files.state.completed': 'تم',
  'files.state.cancelled': 'أُلغي',
  'files.state.failed': 'أخفق',
  'toolbar.dragHandle': 'اسحب شريط الأدوات',
  'toolbar.settings': 'الإعدادات',
  'toolbar.settings.placeholder': 'المزيد من الإعدادات قادم.',
  'toolbar.monitors': 'اختيار الشاشة',
  'toolbar.monitors.empty': 'لم يعلن المضيف عن أي شاشات بعد.',
  'toolbar.monitors.entry': (arg) => `الشاشة ${arg}`,
  'toolbar.chat': 'المحادثة',
  'toolbar.chat.unread': 'المحادثة — رسالة جديدة',
  'toolbar.mic': 'الميكروفون',
  'toolbar.cad': 'Ctrl+Alt+Del',
  'toolbar.record': 'اطلب من المضيف تسجيل هذه الجلسة',
  'toolbar.record.asked': 'تم إرسال طلب التسجيل؛ في انتظار الرد',
  'toolbar.clipboard': 'إرسال الحافظة الخاصة بي إلى المضيف',
  'toolbar.file': 'إرسال ملف إلى المضيف',
  'toolbar.collapse': 'طي',
  'toolbar.expand': 'توسيع',
  'toolbar.fullscreen': 'ملء الشاشة',
  'toolbar.fullscreen.exit': 'إنهاء ملء الشاشة',
  'toolbar.settings.displayMode': 'حجم الصورة',
  'toolbar.settings.localCursor': 'ارسم المؤشر هنا',
  'toolbar.settings.cursorEmbedded': 'يرسل هذا الجهاز المؤشر داخل الصورة.',
  'toolbar.display.fit': 'ملاءمة النافذة',
  'toolbar.display.actual': 'الحجم الحقيقي',
  'toolbar.display.scaled': 'تكبير',
  'toolbar.hotkeys': 'اختصارات لوحة المفاتيح',
  'toolbar.hotkey.toggle-fullscreen': 'تشغيل ملء الشاشة أو إيقافه',
  'toolbar.hotkey.cycle-display-mode': 'حجم الصورة التالي',
  'toolbar.hotkey.reset-view': 'إعادة ملاءمة الصورة وتوسيطها',
  'toolbar.hotkey.toggle-chat': 'تشغيل الدردشة أو إيقافها',
  'toolbar.hotkey.send-cad': 'إرسال Ctrl+Alt+Del',
  'toolbar.hotkey.toggle-toolbar': 'طي شريط الأدوات أو توسيعه',
  'unattended.heading': 'الوصول دون حضور',
  'unattended.explain': 'عند تفعيله يمكن لجهاز وثّقته أن يبدأ جلسة بإدخال كلمة مرور هذا الجهاز، دون حاجة إلى موافقة أحد هنا. يبقى هذا التنبيه ظاهرًا ما دام مفعّلًا.',
  'unattended.indicator': 'الوصول دون حضور مفعّل',
  'unattended.indicator.title':
    'الوصول دون حضور مفعّل: يمكن للأجهزة الموثوقة الاتصال بكلمة مرور الجهاز.',
  'unattended.state.on': 'مفعّل',
  'unattended.state.off': 'متوقف',
  'unattended.password.label': 'كلمة مرور الجهاز',
  'unattended.password.placeholder': '٨ محارف على الأقل',
  'unattended.password.set': 'تفعيل',
  'unattended.password.change': 'تغيير كلمة المرور',
  'unattended.password.saved': 'حُفظت كلمة المرور',
  'unattended.disable': 'إيقاف',
  'unattended.disable.confirm': 'إيقاف الوصول دون حضور؟ ستُحذف كلمة مرور الجهاز وأي عامل ثانٍ، وستحتاج الأجهزة الموثوقة إلى موافقة شخص هنا من جديد.',
  'unattended.totp.label': 'اطلب أيضًا رمزًا لمرة واحدة',
  'unattended.totp.on': 'مطلوب',
  'unattended.totp.off': 'غير مطلوب',
  'unattended.totp.secretHeading': 'أضف هذا إلى تطبيق المصادقة',
  'unattended.totp.secretBody': 'يُعرض مرة واحدة فقط. إن فقدته فأوقف الرمز ثم فعّله من جديد للحصول على غيره.',
  'unattended.totp.uriLabel': 'رابط الإعداد',
  'unattended.totp.done': 'تم',
  'unattended.role.label': 'الجهاز الذي يدخل بهذه الطريقة يحصل على',
  'unattended.needsTrust': 'عيّن كلمة مرور الجهاز أولًا.',
  'book.heading': 'الأجهزة المحفوظة',
  'book.explain': 'توثيق الجهاز لا يسمح له بالدخول وحده، بل يحدّد من يُسمح له أصلًا بمحاولة إدخال كلمة مرور الجهاز.',
  'book.empty': 'لا توجد أجهزة محفوظة بعد. احفظ جهازًا من إحدى الجلسات ليظهر هنا.',
  'book.name.label': 'الاسم',
  'book.tags.label': 'وسوم مفصولة بفواصل',
  'book.notes.label': 'ملاحظة',
  'book.save': 'حفظ',
  'book.remove': 'إزالة',
  'book.remove.confirm': (name) => `إزالة ${name}؟ سيُزال معه أي توثيق يملكه.`,
  'book.trusted': 'موثوق',
  'book.untrusted': 'غير موثوق',
  'book.trust.confirmTitle': (name) => `توثيق ${name}؟`,
  'book.trust.confirmBody': 'سيُسمح لهذا الجهاز ببدء جلسة بإدخال كلمة مرور الجهاز دون موافقة أحد هنا. ويبقى بحاجة إلى كلمة المرور، وإلى الرمز لمرة واحدة إن طلبته.',
  'book.trust.confirmAction': 'نعم، وثّق هذا الجهاز',
  'book.trust.cancel': 'إلغاء',
  'book.untrust.confirm': (name) => `إيقاف توثيق ${name}؟ سيحتاج إلى موافقة شخص هنا من جديد.`,
  'book.filter.label': 'تصفية حسب الوسم',
  'book.filter.all': 'كل الوسوم',
  'book.connected': 'متصل الآن',
  'book.addFromSession': 'احفظ هذا الجهاز',
  'creds.heading': 'هذا الجهاز يطلب كلمة مرور',
  'creds.body': 'لا أحد على الطرف الآخر ليوافق عليك. أدخل كلمة مرور الجهاز التي أعطاك إياها مالكه.',
  'creds.password.label': 'كلمة مرور الجهاز',
  'creds.password.placeholder': 'كلمة مرور الجهاز',
  'creds.code.label': 'رمز لمرة واحدة',
  'creds.code.placeholder': '٦ أرقام',
  'creds.submit': 'تسجيل الدخول',
  'creds.checking': 'جارٍ التحقق',
  'creds.remember': 'تذكّر كلمة مرور هذا الجهاز',
  'creds.badPassword': 'لم تُقبل كلمة المرور.',
  'creds.badCode': 'لم يُقبل الرمز.',
  'creds.lockedOut': (secs) => `محاولات كثيرة. أعد المحاولة بعد ${secs} ثانية.`,
  'creds.unavailable': 'لا يستطيع هذا الجهاز تسجيل دخولك بهذه الطريقة الآن.',
  'quality.path.direct': 'اتصال مباشر',
  'quality.path.relay': 'عبر خادم ترحيل',
  'quality.path.mixed': 'مباشر وعبر ترحيل',
  'quality.path.unknown': 'المسار غير معروف بعد',
  'quality.rttLabel': 'زمن الذهاب والإياب',
  'quality.lossLabel': 'إطارات مفقودة',
  'quality.goodputLabel': 'المستلَم',
  'quality.bitrateLabel': 'معدل الإرسال',
  'quality.fpsLabel': 'معدل الإطارات',
  'quality.relayLabel': 'منطقة خادم الترحيل',
  'quality.ms': (value) => `${value} م.ث`,
  'quality.percent': (value) => `${value}٪`,
  'quality.kbps': (value) => `${value} ك.بت/ث`,
  'quality.fpsValue': (value) => `${value} إطار/ث`,
  'quality.unknown': 'لم يُقَس بعد',
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
