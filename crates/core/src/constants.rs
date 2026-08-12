//! Single point of truth for every numeric constant of the project
//! (design doc §14). Magic numbers duplicating these values are a defect.

/// Maximum size of one control frame payload, checked before allocation (§9.1).
pub const MAX_CONTROL_FRAME_BYTES: usize = 65_536;
/// Per-`NodeId` rate limit on `ConsentRequest` (§9.2).
pub const CONSENT_RATE_PER_MINUTE: u32 = 5;
/// Total size of the host-side consent queue across all guests (§8.1).
pub const MAX_PENDING_CONSENTS: usize = 3;
/// Window in which a dropped session may be resumed by the same peer (§10).
pub const RECONNECT_WINDOW_SECS: u64 = 60;
/// Control-channel keepalive interval (§9.1).
pub const PING_INTERVAL_SECS: u64 = 20;
/// Cumulative active session time granted to the trial plan (§12.3).
pub const TRIAL_SESSION_LIMIT_SECS: u64 = 30 * 60;
/// Thresholds before license expiry at which `LicenseWarn` is sent (§9.1).
pub const LICENSE_WARN_BEFORE_SECS: [u64; 2] = [300, 60];
/// Client heartbeat interval towards the broker (§12.2).
pub const HEARTBEAT_INTERVAL_SECS: u64 = 180;
/// Offline grace without a successful heartbeat, Pro plan (§12.3).
pub const OFFLINE_GRACE_PRO_DAYS: u64 = 7;
/// Offline grace without a successful heartbeat, Team plan (§12.3).
pub const OFFLINE_GRACE_TEAM_DAYS: u64 = 3;
/// Concurrent guest connections allowed on the trial plan (§8.2).
pub const MAX_CONCURRENT_GUESTS_TRIAL: u8 = 1;
/// Concurrent guest connections allowed on the Pro plan (§8.2).
pub const MAX_CONCURRENT_GUESTS_PRO: u8 = 1;
/// Concurrent guest connections allowed on the Team plan, controller
/// included in the ceiling (§8.2).
pub const MAX_CONCURRENT_GUESTS_TEAM: u8 = 5;
/// TTL of a short-link entry (§7).
pub const SHORT_LINK_TTL_SECS: u64 = 600;
/// Width of the opaque short-link identifier (§7).
pub const SHORT_LINK_ID_BITS: usize = 128;
/// Maximum clipboard payload, text/plain UTF-8 only (§9.2).
pub const CLIPBOARD_MAX_BYTES: usize = 64 * 1024;
/// Maximum size of a single offered file (§9.2).
pub const FILE_OFFER_MAX_BYTES: u64 = 500 * 1024 * 1024;
/// Maximum number of pending file offers per session (§9.2).
pub const MAX_PENDING_FILE_OFFERS: usize = 3;
/// Idle desktop RSS budget (§15).
pub const IDLE_RAM_BUDGET_MIB: u32 = 60;
/// Extra RSS budget for an active session with hardware encode (§15).
pub const ACTIVE_SESSION_EXTRA_RAM_BUDGET_MIB: u32 = 150;
/// Width of the random invite identifier (§7).
pub const INVITE_ID_BITS: usize = 128;
/// TTL of a one-shot invite ticket (§7).
pub const INVITE_TICKET_TTL_SECS: u64 = 10 * 60;
/// Short-link creation rate limit per IP (§7).
pub const SHORT_LINK_CREATE_RATE_PER_MIN: u32 = 10;
/// Short-link resolution rate limit per IP (§7).
pub const SHORT_LINK_RESOLVE_RATE_PER_MIN: u32 = 30;
/// Hard cutoff for an already active session after a wall-clock rollback (§12.3).
pub const CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS: u64 = 10 * 60;
/// Default encoder frame rate (§11).
pub const ENCODE_DEFAULT_FPS: u8 = 30;
/// Default encoder bitrate (§11).
pub const ENCODE_DEFAULT_BITRATE_KBPS: u32 = 4_000;
/// Lower bound of the adaptive bitrate range (§11).
pub const ABR_MIN_BITRATE_KBPS: u32 = 300;
/// Upper bound of the adaptive bitrate range (§11).
pub const ABR_MAX_BITRATE_KBPS: u32 = 12_000;
/// Receiver feedback interval sent by the guest (§11).
pub const ABR_FEEDBACK_INTERVAL_MS: u32 = 500;
/// Maximum rate at which the host applies bitrate changes (§11).
pub const ABR_ADJUST_MAX_RATE_PER_SEC: u32 = 1;
/// Log rotation by age (§16.1).
pub const LOG_ROTATION_DAYS: u32 = 7;
/// Log rotation by size (§16.1).
pub const LOG_ROTATION_MAX_MIB: u32 = 100;
