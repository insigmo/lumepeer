#!/bin/bash
# Makes the Mac test VM (macos_new in VMware on beta: macOS 26 on an AMD host,
# no GPU, so WindowServer draws everything on the CPU) quick enough to test
# lumepeer on: no sleep, no screen lock, no background updates, no Spotlight
# index, no transparency, no animations.
#
#     sudo bash mac-vm-tune.sh install     # boot daemon installed, and one run now
#     sudo bash mac-vm-tune.sh uninstall   # daemon removed, the settings stay as they are
#
# Installed, it runs as root at every boot (/Library/LaunchDaemons/local.vm-tune.plist),
# because macOS updates switch some of this back on. Log: /var/log/vm-tune.log.
#
# Apple's background services (Siri, Photos analysis, widgets, analytics) are
# stopped only with SIP off: with SIP on, launchd ignores `launchctl disable`
# for them and refuses `bootout`. Tried 2026-10-13: the system ones stayed
# recorded as disabled and ran after a reboot anyway, and login dropped the
# per-user records.
#
# Kept on purpose, lumepeer's tests need them or a real Mac has them: sound
# (coreaudiod), crash reports (ReportCrash, spindump), the unified log, Bonjour,
# Tailscale, Notification Center, the Spotlight menu (in macOS 26 it is also the
# app launcher), App Nap (a real Mac naps a background host too).
set -u

LABEL=local.vm-tune
DEST=/usr/local/libexec/vm-tune.sh
PLIST=/Library/LaunchDaemons/$LABEL.plist
LOG=/var/log/vm-tune.log

# Per-user agents (gui/<uid>): Photos and media analysis, iCloud media, Siri,
# Apple Intelligence, suggestions and Duet, widgets, Game Center, Screen Time.
USER_SERVICES="
com.apple.photoanalysisd com.apple.mediaanalysisd com.apple.photolibraryd com.apple.cloudphotod
com.apple.mediastream.mstreamd com.apple.amp.mediasharingd com.apple.itunescloudd com.apple.bird
com.apple.Siri.agent com.apple.assistantd com.apple.assistant_service com.apple.assistant_cdmd
com.apple.siriactionsd com.apple.siriinferenced com.apple.siriknowledged com.apple.sirittsd
com.apple.SiriTTSTrainingAgent
com.apple.intelligencecontextd com.apple.intelligenceflowd com.apple.intelligenceplatformd
com.apple.intelligencetasksd com.apple.callintelligenced
com.apple.suggestd com.apple.knowledge-agent com.apple.knowledgeconstructiond com.apple.duetexpertd
com.apple.proactived com.apple.proactiveeventtrackerd com.apple.routined com.apple.biomesyncd
com.apple.triald com.apple.parsecd com.apple.parsec-fbf
com.apple.chronod com.apple.weatherd com.apple.newsd com.apple.financed com.apple.homed
com.apple.gamed com.apple.tipsd com.apple.helpd com.apple.translationd com.apple.familycircled
com.apple.followupd com.apple.ScreenTimeAgent
"
# System daemons: Time Machine, analytics, power telemetry, trial experiments.
SYSTEM_SERVICES="
com.apple.backupd com.apple.backupd-helper
com.apple.SubmitDiagInfo com.apple.analyticsd com.apple.ecosystemanalyticsd
com.apple.appleseed.fbahelperd com.apple.triald.system
com.apple.PerfPowerServices com.apple.PerfPowerServicesExtended com.apple.perfpowermetricd
"

log() { echo "$(date '+%F %T') $*"; }
# One refused tweak must not stop the rest; what failed goes to the log.
run() { "$@" >/dev/null 2>&1 || log "failed: $*"; }

real_users() {
  dscl . -list /Users UniqueID | awk '$2 >= 501 { print $1 }' | while read -r u; do
    [ -d "/Users/$u" ] && echo "$u"
  done
}

user_default() { local u=$1; shift; run sudo -u "$u" -H defaults write "$@"; }

# launchctl disable|enable <domain> <services...>, one log line for what it refused.
switch_services() {
  local verb=$1 domain=$2 s refused=""
  shift 2
  for s in "$@"; do launchctl "$verb" "$domain/$s" >/dev/null 2>&1 || refused="$refused $s"; done
  [ -z "$refused" ] || log "launchctl $verb $domain refused:$refused"
}

# Disabled from now on, and stopped if running.
stop_services() {
  local domain=$1 s
  shift
  switch_services disable "$domain" "$@"
  for s in "$@"; do launchctl bootout "$domain/$s" >/dev/null 2>&1; done
}

# A user's agents live in gui/<uid>, which exists only once somebody is logged
# in: wait up to 10 minutes for the console user (autologin brings it up soon
# after boot).
console_uid() {
  local u i
  for i in $(seq 120); do
    u=$(stat -f %Su /dev/console)
    if [ -n "$u" ] && [ "$u" != root ] && launchctl print "gui/$(id -u "$u")" >/dev/null 2>&1; then
      id -u "$u"
      return 0
    fi
    sleep 5
  done
  return 1
}

tune_services() {
  local uid
  if ! csrutil status | grep -q 'status: disabled'; then
    log "SIP on: Apple services left running (launchd ignores disabling them)"
    return
  fi
  stop_services system $SYSTEM_SERVICES
  if uid=$(console_uid); then
    stop_services "gui/$uid" $USER_SERVICES
  else
    log "nobody logged in within 10 minutes: per-user services left running"
  fi
}

tune_system() {
  # Idle sleep hangs this VM, and a display that sleeps locks the screen,
  # which only the owner can unlock.
  run pmset -a sleep 0 displaysleep 0 disksleep 0
  for kv in "standby 0" "autopoweroff 0" "powernap 0" "hibernatemode 0" "proximitywake 0"; do
    run pmset -a $kv
  done
  rm -f /var/vm/sleepimage
  run defaults write /Library/Preferences/com.apple.screensaver loginWindowIdleTime -int 0

  run mdutil -a -i off

  # No update downloads, installs and reboots in the background; updates are
  # installed by hand when wanted.
  for k in AutomaticCheckEnabled AutomaticDownload AutomaticallyInstallMacOSUpdates \
           CriticalUpdateInstall ConfigDataInstall; do
    run defaults write /Library/Preferences/com.apple.SoftwareUpdate "$k" -bool false
  done
  run defaults write /Library/Preferences/com.apple.commerce AutoUpdate -bool false

  run defaults write "/Library/Application Support/CrashReporter/DiagnosticMessagesHistory.plist" AutoSubmit -bool false
  run defaults write "/Library/Application Support/CrashReporter/DiagnosticMessagesHistory.plist" ThirdPartyDataSubmit -bool false
}

tune_user() {
  local u=$1

  # Reduce transparency and Reduce motion: Liquid Glass, blur and the window
  # animations are what keeps WindowServer busy here. AppKit reads them from
  # com.apple.universalaccess, which TCC keeps from this daemon (install_tcc_keys);
  # macOS 26 holds the same two switches here too, kept in step.
  user_default "$u" com.apple.Accessibility EnhancedBackgroundContrastEnabled -int 1
  user_default "$u" com.apple.Accessibility ReduceMotionEnabled -int 1

  user_default "$u" -g NSAutomaticWindowAnimationsEnabled -bool false
  user_default "$u" -g NSWindowResizeTime -float 0.001
  user_default "$u" -g NSScrollAnimationEnabled -bool false
  user_default "$u" -g NSToolbarFullScreenAnimationDuration -float 0
  user_default "$u" -g NSBrowserColumnAnimationSpeedMultiplier -float 0
  user_default "$u" -g QLPanelAnimationDuration -float 0
  user_default "$u" -g NSQuitAlwaysKeepsWindows -bool false
  user_default "$u" com.apple.dock launchanim -bool false
  user_default "$u" com.apple.dock expose-animation-duration -float 0.1
  user_default "$u" com.apple.dock autohide-delay -float 0
  user_default "$u" com.apple.dock autohide-time-modifier -float 0
  user_default "$u" com.apple.dock mineffect -string scale
  user_default "$u" com.apple.dock show-recents -bool false
  user_default "$u" com.apple.dock springboard-show-duration -float 0
  user_default "$u" com.apple.dock springboard-hide-duration -float 0
  user_default "$u" com.apple.finder DisableAllAnimations -bool true
  user_default "$u" com.apple.WindowManager StandardHideWidgets -bool true
  user_default "$u" com.apple.WindowManager StageManagerHideWidgets -bool true

  # The screen saver never starts, so it never locks the screen either.
  run sudo -u "$u" -H defaults -currentHost write com.apple.screensaver idleTime -int 0
  # Login reopens nothing from the last session.
  user_default "$u" com.apple.loginwindow TALLogoutSavesState -bool false
  user_default "$u" com.apple.loginwindow LoginwindowLaunchesRelaunchApps -bool false

  user_default "$u" com.apple.coreservices.useractivityd ActivityAdvertisingAllowed -bool false
  user_default "$u" com.apple.coreservices.useractivityd ActivityReceivingEnabled -bool false
  user_default "$u" com.apple.assistant.support "Assistant Enabled" -bool false
  user_default "$u" com.apple.Siri StatusMenuVisible -bool false
  user_default "$u" com.apple.lookup.shared LookupSuggestionsDisabled -bool true
}

apply() {
  log "start"
  tune_system
  for u in $(real_users); do tune_user "$u"; done
  # A Dock and Finder already running (install while logged in) re-read their settings.
  killall Dock Finder >/dev/null 2>&1
  tune_services
  log "done"
}

# com.apple.universalaccess is TCC-protected: a launchd daemon may not write it,
# a shell holding Full Disk Access may (an ssh session does, sshd has it).
# Written once at install; the setting stays.
install_tcc_keys() {
  local u
  for u in $(real_users); do
    sudo -u "$u" -H defaults write com.apple.universalaccess reduceTransparency -bool true >/dev/null 2>&1 &&
      sudo -u "$u" -H defaults write com.apple.universalaccess reduceMotion -bool true >/dev/null 2>&1 ||
      echo "$u: Reduce transparency/motion refused (this shell has no Full Disk Access); install over ssh, or switch them on in System Settings > Accessibility > Display"
  done
}

install() {
  install_tcc_keys
  mkdir -p "$(dirname "$DEST")"
  cp "${BASH_SOURCE[0]}" "$DEST" && chown root:wheel "$DEST" && chmod 755 "$DEST" || exit 1
  cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array><string>/bin/bash</string><string>$DEST</string></array>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF
  chown root:wheel "$PLIST" && chmod 644 "$PLIST"
  launchctl bootout "system/$LABEL" >/dev/null 2>&1
  # RunAtLoad: bootstrapping it runs it once right now, as at every boot.
  launchctl bootstrap system "$PLIST" && echo "installed; log: $LOG"
}

uninstall() {
  launchctl bootout "system/$LABEL" >/dev/null 2>&1
  rm -f "$PLIST" "$DEST"
  switch_services enable system $SYSTEM_SERVICES
  for u in $(real_users); do switch_services enable "gui/$(id -u "$u")" $USER_SERVICES; done
  echo "removed; Apple services start again from the next boot, the other settings stay as they are"
}

[ "$(id -u)" = 0 ] || { echo "needs root: sudo bash $0 ${1:-}"; exit 1; }
case "${1:-}" in
  install) install ;;
  uninstall) uninstall ;;
  "") apply ;;
  *) echo "usage: sudo bash $0 [install|uninstall]"; exit 2 ;;
esac
