//! Single point of truth for every numeric constant of the project
//! (design doc §14). Magic numbers duplicating these values are a defect.

/// Maximum size of one control frame payload, checked before allocation (§9.1).
pub const MAX_CONTROL_FRAME_BYTES: usize = 65_536;
/// Maximum size of one media frame payload on `rd/media/1`, checked before
/// allocation exactly as `MAX_CONTROL_FRAME_BYTES` is on the control channel
/// (§3.2, §9.1). Encoded video needs far more room than a control message, but
/// still a bound: even at the `ABR_MAX_BITRATE_KBPS` ceiling a single keyframe
/// stays well below this. It is deliberately at or under
/// `lumepeer_media::decode::SLOT_PAYLOAD_BYTES`, so a frame that passed this
/// check always fits the decoder's shared-memory slot.
///
/// Equal to that slot rather than half of it since [`MAX_STREAM_PIXELS`]: an
/// intra frame of a 4K desktop, encoded at a quality preset's bitrate, is the
/// one frame in the stream that can genuinely approach a megabyte or three,
/// and dropping it is worse than carrying it - everything after it references
/// it, so the guest sees nothing at all until the next one.
///
/// It did **not** have to move when [`MAX_STREAM_PIXELS`] did (ADR 0074),
/// and the reason is that it was never a function of the picture's size. What
/// bounds an encoded frame is rate control: at [`ABR_MAX_BITRATE_KBPS`] a
/// whole *second* of video is 3.1 MiB, so one frame cannot reach this bound
/// without the encoder having ignored its own bitrate — which is what the
/// assertion below states, and it holds at any resolution. Measured on real
/// hardware to check the arithmetic against reality: a 4K intra frame at that
/// ceiling came out at 368 KiB on H.264 and 1.3 MiB on AV1, 4.4% and 15.8% of
/// this bound (`encode::windows::tests::what_a_picture_above_4k_costs`).
pub const MAX_MEDIA_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// One frame may not be asked to carry more than a second of peak bitrate
/// (§11, §14; ADR 0074).
///
/// This is what makes [`MAX_MEDIA_FRAME_BYTES`] independent of the picture
/// size: raising [`MAX_STREAM_PIXELS`] changes how many pixels a frame
/// describes, never how many bits rate control will spend describing them.
const _: () = assert!(
    (ABR_MAX_BITRATE_KBPS as usize) * 1_000 / 8 <= MAX_MEDIA_FRAME_BYTES,
    "MAX_MEDIA_FRAME_BYTES cannot hold one second at ABR_MAX_BITRATE_KBPS"
);
/// Largest picture, in pixels, the host encodes for a guest that has not said
/// what size it wants (§11, §15; ADR 0018, ADR 0060).
///
/// This is the *decoded RGBA* bound, and that is the whole reason for its
/// value: a guest that cannot decode the bitstream in its own webview falls
/// back to the sandboxed worker of §11.3, which returns pictures through a
/// shared-memory slot. One RGBA8 picture of this size is 8 MiB, which is
/// exactly what `lumepeer_media::decode::SLOT_PAYLOAD_BYTES` holds — the two
/// are asserted against each other at compile time there, so raising one
/// without the other does not build.
///
/// It is **not** a bound on what a guest may receive. Since ADR 0058 the
/// picture normally never exists as RGBA outside the webview's own decoder,
/// and a guest that knows this says so with
/// [`crate::protocol::MessageKind::StreamSizeRequest`], which the host honors
/// up to [`MAX_STREAM_PIXELS`]. Applying this ceiling to such a guest is what
/// made a 1440p host arrive as a resampled 1080p picture that the guest's
/// canvas then stretched back out — softening every glyph on the screen twice
/// over.
pub const MAX_PICTURE_PIXELS: usize = 1920 * 1080;
/// Largest picture, in pixels, the host will encode for a guest that asked for
/// a size with [`crate::protocol::MessageKind::StreamSizeRequest`] (§11;
/// ADR 0060).
///
/// 5K (5120x2880), the largest desktop panel in ordinary use, raised from 4K
/// by ADR 0074. Nothing here has to fit `SLOT_PAYLOAD_BYTES`: a guest only
/// sends the request when it decodes into its own webview, where the picture
/// stays on the GPU and never crosses an IPC boundary as pixels at all.
///
/// 5K and not 8K, and that is a measurement rather than a preference. Encoded
/// frames are not the limit — see [`MAX_MEDIA_FRAME_BYTES`]. What is, on both
/// sides:
///
/// - **The encoder.** On the reference machine both hardware MFTs — H.264 and
///   AV1 — negotiate 3840x2160 and refuse every size above it. A host whose
///   own panel is larger than what its encoder takes falls back to the
///   [`MAX_PICTURE_PIXELS`] ceiling for the rest of the session rather than
///   sending nothing (ADR 0074); that is honest degradation, not a picture at
///   this size.
/// - **Main memory.** One BGRA frame at this size is 56 MiB, and the readback
///   capture path holds two of them plus an NV12 buffer — about 155 MiB,
///   against the 150 MiB `active_extra_rss_mib` budget of §15
///   (`ci/resource-budget.yml`). A host reaches this size inside its budget
///   only on the zero-copy path of ADR 0073, where those buffers are GPU
///   textures instead. 8K would be four times that in either direction, and
///   raising the budget to fit it is exactly what §15 forbids.
pub const MAX_STREAM_PIXELS: usize = 5120 * 2880;
/// Smallest picture, per axis, a guest may ask for with
/// [`crate::protocol::MessageKind::StreamSizeRequest`] (§9.1; ADR 0060).
///
/// A bound on an untrusted peer's number rather than a considered minimum:
/// below this a desktop is not a picture of anything, and an encoder handed a
/// two-pixel axis fails the frame instead of producing a smaller one.
pub const STREAM_SIZE_MIN_PX: u32 = 160;
/// Pause between redial attempts inside the one media recovery pass bounded by
/// [`RECONNECT_WINDOW_SECS`]. Not a second reconnect window: it only keeps a
/// host that refuses instantly from turning that window into a busy loop.
pub const MEDIA_REDIAL_BACKOFF_MS: u64 = 500;
/// Per-`NodeId` rate limit on `ConsentRequest` (§9.2).
pub const CONSENT_RATE_PER_MINUTE: u32 = 5;
/// Total size of the host-side consent queue across all guests (§8.1).
pub const MAX_PENDING_CONSENTS: usize = 3;
/// Window in which a dropped session may be resumed by the same peer (§10;
/// ADR 0089, widened by ADR 0091).
///
/// Was 60 under ADR 0089, which is §10's own number. Measured against a real
/// link loss — a VPN coming up on one of the two machines — 60 seconds was
/// not enough for two independent reasons, and only one of them was the
/// number:
///
/// - A resume *dial* could outlive the window it was meant to work inside.
///   It ran on the first-connection budget of ADR 0050, up to
///   [`DIAL_TOTAL_BUDGET_SECS`], so the window closed underneath an attempt
///   still in flight and [`RESUME_RETRY_SECS`] never produced a second one.
///   That is fixed by [`RESUME_ATTEMPTS`] rather than here.
/// - An interface change is not instant on either side. The machine whose
///   network moved has to notice, rebind, re-probe its relay and republish
///   its address before anything can reach it, and a minute is an ordinary
///   time for that — longer still if the link is simply gone for a while,
///   which is the case a person describes as "the internet dropped".
///
/// What the window bounds is how long a session's **grants** may come back
/// without anybody being asked again, and widening it does not widen *who*
/// may come back: a resume still needs the same authenticated key, the same
/// session id, and a monotonic clock that cannot be wound back (§10, §12.3;
/// ADR 0089). What it costs is that a guest slot stays held, and the person
/// at the host sees the session gone and then back, for up to this long
/// instead of up to a minute. Still bounded well below
/// [`REBOOT_WAIT_CEILING_SECS`], so a machine that is actually gone still
/// ends its sessions.
pub const RECONNECT_WINDOW_SECS: u64 = 300;
/// Pause between attempts of `WindowsCapturer` to reopen a Desktop
/// Duplication lost to the secure desktop (lock screen, UAC prompt or fast
/// user switch), in milliseconds (docs/bugs/11-uac-degradation.md).
pub const SECURE_DESKTOP_RECOVERY_BACKOFF_MS: u64 = 1_000;
/// How often the encode loop asks the privileged helper for a fresh frame of
/// the secure desktop while it holds the `secure_desktop` grant and capture
/// is stuck behind one, in milliseconds
/// (`docs/bugs/15-secure-desktop-capture.md`, ADR 0049).
///
/// Deliberately much slower than the ordinary encode cadence: a UAC prompt
/// or a lock screen is largely static, and a fresh pipe round trip plus a GDI
/// capture on a `LocalSystem` process is a real cost to spend on every frame
/// interval for a picture that mostly is not changing.
pub const SECURE_DESKTOP_CAPTURE_INTERVAL_MS: u64 = 500;
/// Control-channel keepalive interval (§9.1).
pub const PING_INTERVAL_SECS: u64 = 20;
/// Smoothing factor of the exponentially weighted moving average that turns
/// individual `Ping`/`Pong` round trips into the RTT the UI shows (§11, §18).
///
/// Not in the design doc: §9.1 fixes the keepalive interval but says nothing
/// about how the measurement it carries is reported. A raw sample jumps with
/// every retransmit and scheduling hiccup, and a number that jumps is a number
/// nobody can act on. 0.25 keeps roughly the last four probes in view — about
/// a minute and a half at [`PING_INTERVAL_SECS`] — which is slow enough to be
/// readable and fast enough to notice a path that just got worse.
pub const RTT_EWMA_ALPHA: f32 = 0.25;
/// Largest round trip, in milliseconds, that is taken as a measurement rather
/// than as a broken clock (§18).
///
/// A `Pong` that comes back after this either crossed a suspended machine or
/// was never really a round trip at all, and folding it into the
/// [`RTT_EWMA_ALPHA`] average would poison the reading for minutes. Above it
/// the sample is dropped and the previous average stands.
pub const RTT_MAX_PLAUSIBLE_MS: u32 = 60_000;
/// Deadline for one accepted connection to complete the control handshake
/// before the host drops it, so a peer that connects and then goes silent
/// cannot tie up a task (§9.1, §18).
///
/// This covers only the `Hello`/`HelloAck` exchange, which is one round trip
/// on a connection that is already up. Getting the connection up is bounded
/// separately by [`INCOMING_ACCEPT_TIMEOUT_SECS`], because the two are not the
/// same kind of wait at all (ADR 0027).
pub const CONTROL_HANDSHAKE_TIMEOUT_SECS: u64 = 10;
/// Deadline for an accepted incoming connection to finish its **QUIC**
/// handshake, before any control frame is expected of it (§9.1, §18).
///
/// Deliberately longer than a guest's own dial budget. A guest that is still
/// hole-punching has not gone silent — it is working — and a host that gives
/// up first turns a slow path into a failed session while the far side is
/// still trying (ADR 0027).
pub const INCOMING_ACCEPT_TIMEOUT_SECS: u64 = 20;
/// Attempts one outgoing connect makes before it is reported as failed
/// (§7, ADR 0027, ADR 0050).
///
/// A first attempt races whatever the host's address set said when the invite
/// was issued: a relay the host has since moved off, a hole punch that has not
/// landed, a discovery record that is a few seconds stale. Each of those is
/// gone by the next attempt, and a user who has to re-paste the code cannot
/// tell any of them from a dead host.
///
/// Was 3 under ADR 0027. Measured against a host whose relay link flaps on a
/// roughly 20-second cycle, 3 attempts at [`CONNECT_ATTEMPT_TIMEOUT_SECS`]
/// apart landed in a down window all three times, with a manual retry minutes
/// later succeeding on the first try — the automatic budget was simply too
/// short to reliably straddle one good window (ADR 0050).
pub const DIAL_ATTEMPTS: u32 = 5;
/// Pause between the attempts of [`DIAL_ATTEMPTS`], long enough for iroh's own
/// discovery to have republished, short enough not to read as a hang.
pub const DIAL_RETRY_BACKOFF_MS: u64 = 750;
/// Random extra pause, on top of [`DIAL_RETRY_BACKOFF_MS`], added to each
/// retry (ADR 0050).
///
/// A fixed backoff keeps every retry the same distance from the one before
/// it; against a host whose relay outages recur on their own roughly fixed
/// period, that lets every attempt land in the same phase of the cycle — all
/// unlucky, or all lucky, with no way to tell which in advance. Jitter breaks
/// the lockstep so a run of attempts sweeps across the cycle instead of
/// riding one point on it.
pub const DIAL_RETRY_BACKOFF_JITTER_MS: u64 = 1_500;
/// Bound on one attempt of [`DIAL_ATTEMPTS`] — dial *and* handshake together.
///
/// Without it a single attempt can hold the whole budget: a connection that
/// comes up over a relay link which then dies takes iroh's full dial timeout
/// plus the handshake's own stall before it gives up, and the retry that would
/// have worked never runs (ADR 0027).
pub const CONNECT_ATTEMPT_TIMEOUT_SECS: u64 = 20;
/// Attempts one transport of a dial plan may spend before the next transport
/// in the plan is tried (gap-tasks/23 task 1, ADR 0083).
///
/// Taken *out of* [`DIAL_ATTEMPTS`], never added to it: a plan spends the same
/// number of attempts a single-transport dial always spent, so trying two
/// transports cannot make a user wait longer than trying one. The last
/// transport of a plan gets whatever the earlier ones did not use, which is
/// all five when there is only one.
///
/// Two rather than one because a single lost packet is not evidence about a
/// transport: the first attempt on a freshly bound obfuscated endpoint races
/// its own NAT mapping, and condemning the transport on it would fall back
/// every time a handshake needed a second try. Two rather than three because
/// the fallback is what actually connects a guest whose first choice does not
/// work, and it should still have the larger half of the budget.
pub const TRANSPORT_PROBE_ATTEMPTS: u32 = 2;
/// Worst case a connect may cost the user, whichever transports its plan
/// tries (gap-tasks/23 task 1, ADR 0083).
///
/// Derived, never chosen: it is exactly what [`DIAL_ATTEMPTS`] attempts at
/// [`CONNECT_ATTEMPT_TIMEOUT_SECS`], spaced by [`DIAL_RETRY_BACKOFF_MS`] plus
/// the full [`DIAL_RETRY_BACKOFF_JITTER_MS`], already cost before a dial could
/// try more than one transport (ADR 0050). Splitting those attempts across a
/// plan's transports moves where the time is spent and not how much of it
/// there is, and this constant is what a test holds that property against.
pub const DIAL_TOTAL_BUDGET_SECS: u64 = DIAL_ATTEMPTS as u64 * CONNECT_ATTEMPT_TIMEOUT_SECS
    + ((DIAL_ATTEMPTS as u64 - 1) * (DIAL_RETRY_BACKOFF_MS + DIAL_RETRY_BACKOFF_JITTER_MS)) / 1_000;
/// Pause before a connect the user asked for is dialed again, after a whole
/// [`DIAL_ATTEMPTS`] round came back with nothing but this side's own silence
/// (ADR 0096).
///
/// The round is not the end of the attempt any more. A host whose relay link
/// is down, whose published record is stale, or whose machine is still coming
/// up answers nothing for minutes at a time, and `DIAL_TOTAL_BUDGET_SECS` of
/// trying is not long enough to outlast that — the connect used to end in a
/// failure the user could only answer by clicking the same button again.
/// While the user has a connect open, this node keeps dialing instead.
pub const CONNECT_RETRY_BACKOFF_SECS: u64 = 5;
/// Ceiling the pause of [`CONNECT_RETRY_BACKOFF_SECS`] doubles up to (ADR
/// 0096).
///
/// Bounded rather than unbounded so a host that comes back after an hour is
/// still found within half a minute, and low enough that the wait never reads
/// as a hang. A round of dialing already costs [`DIAL_TOTAL_BUDGET_SECS`], so
/// this is the smaller half of the cycle either way.
pub const CONNECT_RETRY_BACKOFF_CEILING_SECS: u64 = 30;
/// How often a guest looks up where its saved hosts are now, without dialing
/// any of them (ADR 0099).
///
/// Half an hour. What the refresh catches is a host that moved — rebooted onto
/// a new public address, changed network, had its NAT binding recycled — and
/// it costs a DNS and DHT query per saved host, which is why it is not a
/// minute. It is also not an hour, because the point is that the address in
/// hand when the user presses Connect is *recent*: a record up to half an hour
/// old is one a dial can start on, and one up to an hour old often is not.
pub const SAVED_HOST_REFRESH_SECS: u64 = 30 * 60;
/// Pause before the first sweep of [`SAVED_HOST_REFRESH_SECS`] (ADR 0099).
///
/// Long enough for this node's own endpoint to have reached a relay and
/// published itself, since a lookup from an endpoint that is not online yet
/// mostly answers nothing. Short enough that a user who starts the app and
/// then goes to click a saved host has the fresh address before they arrive.
pub const SAVED_HOST_FIRST_REFRESH_SECS: u64 = 20;
/// Bound on looking one saved host up (ADR 0099).
///
/// The DHT answers when it answers, and this runs behind nobody's spinner, so
/// the only thing this protects is the sweep itself: a host that cannot be
/// found must not hold up the one after it.
pub const SAVED_HOST_LOOKUP_TIMEOUT_SECS: u64 = 8;
/// How many saved hosts one sweep refreshes (ADR 0099).
///
/// The list holds up to fifty, and looking all of them up every half hour
/// would put fifty DHT queries on the network for a user who has one host they
/// actually use. The list is newest-first, so this is the hosts somebody has
/// actually been connecting to.
pub const SAVED_HOSTS_PER_REFRESH: usize = 8;
/// Direct addresses remembered per saved host (ADR 0093, ADR 0099).
///
/// Every one of them is offered to iroh on the next dial and costs a probe, so
/// this is a cap on the dial's own fan-out as much as on the file's size. Four
/// is a host with an IPv4 and an IPv6 address on two interfaces, which is the
/// widest real machine this has met.
pub const SAVED_HOST_ADDRS: usize = 4;
/// Handshakes the host will run concurrently. Beyond this, further incoming
/// connections are closed immediately rather than queued (§3.2).
pub const MAX_INFLIGHT_HANDSHAKES: usize = 8;
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
/// How often the desktop host re-reads its own clipboard while at least one
/// session holds a clipboard grant (§9.2; ADR 0030).
///
/// Not in the design doc: §9.2 assumes a clipboard *change* is observable,
/// and no cross-platform API delivers one. Polling is the substitute, so the
/// number is a latency/cost trade rather than a protocol value — fast enough
/// that copy-then-paste feels immediate, slow enough that an idle granted
/// session is not reading the user's clipboard hundreds of times a minute.
/// The poll runs only while a grant is live; without one the clipboard is
/// never read at all.
pub const CLIPBOARD_POLL_INTERVAL_MS: u64 = 500;
/// Maximum size of a single offered file (§9.2, as amended by ADR 0077).
///
/// 64 GiB, and it is a *policy* rather than a protection. Nothing downstream
/// allocates this: a chunk is bounded by [`FILE_CHUNK_MAX_BYTES`] and checked
/// before a byte of it is read (§9.1), the hash is computed streaming, and
/// the receiver writes to staging rather than to memory. What this number
/// actually says is "a peer claiming more than this is not describing a file
/// anyone meant to send", which the old 500 MiB said about a disk image, a
/// video and half the things a person reaches for a remote desktop to move.
///
/// The real ceiling on a receive is free space where the file is going, which
/// [`STAGING_FREE_SPACE_MARGIN_BYTES`] and the receiver's own check enforce
/// **before the first byte** rather than at ninety per cent (§18; ADR 0077).
///
/// A peer built before `PROTOCOL_MINOR` 14 decodes an offer above
/// [`FILE_OFFER_LEGACY_MAX_BYTES`] as malformed and closes the connection, so
/// a sender must not make one to a peer below that minor — the one place this
/// relaxation is visible on the wire.
pub const FILE_OFFER_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// What a peer below `PROTOCOL_MINOR` 14 accepts in a `FileOffer`,
/// `FileTransferStart` or `FilePutOffer` (ADR 0077).
///
/// Frozen at what [`FILE_OFFER_MAX_BYTES`] used to be. Kept as its own
/// constant rather than written into the sender as a number, because it is
/// the definition of a peer's behaviour and not a choice this build makes.
pub const FILE_OFFER_LEGACY_MAX_BYTES: u64 = 500 * 1024 * 1024;
/// Free space a receiver keeps back when deciding whether a transfer fits
/// (§18; ADR 0077).
///
/// A filesystem needs room for its own metadata, the destination volume can
/// be written by something else between the answer and the last chunk, and a
/// volume filled to its last byte by a transfer is a machine that stops
/// working for reasons that have nothing to do with the transfer.
pub const STAGING_FREE_SPACE_MARGIN_BYTES: u64 = 256 * 1024 * 1024;
/// How much older than this process's epoch a staging file's modification
/// time has to be before the sweep treats it as left over from an earlier
/// run (ADR 0077).
///
/// A filesystem does not stamp a file with the clock the epoch is read from.
/// Linux stamps it from the kernel's coarse clock, which trails
/// `SystemTime::now` by up to one scheduler tick, so a staging file created a
/// moment after the epoch was taken can carry a modification time from just
/// before it — and the next sweep into that directory then deleted a transfer
/// that was still running. FAT is coarser still and rounds a write time down
/// to two seconds. Two seconds covers both; the price is that a file an
/// earlier run touched within two seconds of this run's first sweep waits for
/// the next run to be removed.
pub const STAGING_SWEEP_TIMESTAMP_SLACK_SECS: u64 = 2;
/// Maximum byte length of the file name in a `FileOffer` or a
/// `FileTransferStart` (§9.2; ADR 0032).
///
/// Not in the design doc: §9.2 bounds the file, not its name. 255 is the
/// per-component limit of every filesystem this ships on, so a longer name
/// could not be written down anyway — and a name is untrusted input that ends
/// up as a path, which is the one place a missing bound is worth having.
pub const FILE_NAME_MAX_BYTES: usize = 255;
/// Maximum number of pending file offers per session (§9.2).
pub const MAX_PENDING_FILE_OFFERS: usize = 3;
/// How many times a sender picks a file up again after its stream ended
/// early, before calling the transfer failed (§10; ADR 0077).
///
/// A file connection can drop while the control connection it was authorized
/// on stays up — a NAT rebinding, a link that flapped — and the receiver's
/// staging file and acked offset both survive that. Picking the file up from
/// the last acked offset is what §10's resume point is for; a bound on the
/// number of attempts is what keeps a destination that refuses every write
/// from becoming a loop.
pub const FILE_RESUME_ATTEMPTS: u32 = 3;
/// Idle desktop RSS budget (§15).
pub const IDLE_RAM_BUDGET_MIB: u32 = 60;
/// Extra RSS budget for an active session with hardware encode (§15).
pub const ACTIVE_SESSION_EXTRA_RAM_BUDGET_MIB: u32 = 150;
/// Width of the random invite identifier (§7).
pub const INVITE_ID_BITS: usize = 128;
/// TTL of an invite ticket (§7, as amended by ADR 0062).
///
/// A year, not the ten minutes §7 first specified. The short TTL was written
/// for a one-shot invite; ADR 0016 already made a ticket reusable, and what
/// actually bounds one is the host retiring it by issuing a replacement. Ten
/// minutes did not add a bound so much as break the saved-device button: a
/// host that was reachable yesterday refused today's connection as "out of
/// date", and the only cure was reading a fresh code out loud again.
///
/// A code is still not a key to the machine — every connection is decided by
/// the host, every time (§2.3) — and it is still withdrawable at will from the
/// settings window, which is the revocation this leans on.
pub const INVITE_TICKET_TTL_SECS: u64 = 365 * 24 * 60 * 60;
/// Upper bound on the random padding added inside each obfuscated datagram
/// (task 17 Fase 2, ADR 0051).
///
/// The obfuscation codec seals `len || payload || padding` under AEAD, so the
/// padding is invisible on the wire (uniform ciphertext) and its only job is
/// to blur the datagram-length distribution — otherwise the stream would carry
/// a new fixed-length signature in place of the QUIC one it removes. Each
/// datagram draws a fresh padding length in `0..=OBFUSCATE_PADDING_MAX_BYTES`.
/// Kept small so the fixed envelope overhead plus this bound stays well inside
/// a path MTU; the endpoint-integration step (ADR 0051, §5) subtracts the
/// envelope overhead and this bound from the QUIC max datagram size.
pub const OBFUSCATE_PADDING_MAX_BYTES: usize = 128;
/// How long to wait for one STUN server's Binding response before giving up on
/// it and trying the next (task 17, ADR 0052).
///
/// A single stateless request/response over UDP, used only to learn this
/// device's public reflexive address for a serverless invite — never to carry
/// session data. Short, because a server that does not answer promptly is one
/// to move past, not to wait on; the caller walks a list of servers and the
/// mapping it wants is the same for all of them.
pub const STUN_QUERY_TIMEOUT_MS: u64 = 3_000;
/// Keep-alive interval on the obfuscated QUIC transport (task 17, ADR 0052).
/// Mandatory: without a keep-alive the QUIC idle timeout closes an otherwise
/// healthy path at ~30 s, which was once mistaken for a DPI drop
/// (project-lumepeer-quic-vs-relay-transport). Must stay below
/// [`QUIC_MAX_IDLE_TIMEOUT_SECS`].
pub const QUIC_KEEPALIVE_SECS: u64 = 15;
/// Idle timeout on the obfuscated QUIC transport (task 17, ADR 0052). Larger
/// than twice [`QUIC_KEEPALIVE_SECS`] so a single lost keep-alive never trips
/// it, but bounded so a truly dead path is eventually released.
pub const QUIC_MAX_IDLE_TIMEOUT_SECS: u64 = 60;
/// Interval on which a host holding an obfuscated-transport invite open
/// resends a STUN request, to keep its NAT mapping from expiring before a
/// guest dials in (task 17 increment 2, ADR 0053).
///
/// This is what stands in for a synchronized "simultaneous punch": an
/// endpoint-independent NAT mapping, once opened by the host's own STUN
/// query, accepts inbound from any source for as long as it stays alive, so
/// holding it open with a resend on this interval is enough — no channel to
/// coordinate timing with the guest exists or is needed. Comfortably under a
/// typical NAT UDP-binding timeout (commonly 30-120 s) so the mapping never
/// lapses between two resends.
pub const NAT_MAPPING_KEEPALIVE_SECS: u64 = 25;
/// Dial attempts a guest makes over the obfuscated transport before giving up
/// (task 17 increment 2, ADR 0053).
///
/// Kept separate from [`DIAL_ATTEMPTS`] (ADR 0050): that budget was tuned for
/// iroh relay-flap resonance, a different failure mode than a punch to a
/// freshly learned reflexive address on this transport.
pub const OBFUSCATED_CONNECT_ATTEMPTS: u32 = 5;
/// Backoff between obfuscated-transport dial attempts, milliseconds (task 17
/// increment 2, ADR 0053). See [`OBFUSCATED_CONNECT_ATTEMPTS`].
pub const OBFUSCATED_CONNECT_RETRY_BACKOFF_MS: u64 = 500;
/// How long one obfuscated dial attempt may wait for its handshake before the
/// next one goes out, milliseconds (gap-tasks/22 task 3, ADR 0082).
///
/// A dial on this transport is also the punch: the attempt's own QUIC Initial
/// is the packet that opens this side's mapping and probes the host's, sealed
/// by the same codec as everything else, so there is nothing on the wire that
/// a punch and a session do differently. What the punch needs from the clock
/// is a *cadence*, and an unbounded attempt has none — it waits out
/// [`QUIC_MAX_IDLE_TIMEOUT_SECS`] on a path that answers nothing, so
/// [`OBFUSCATED_CONNECT_ATTEMPTS`] of them become minutes of silence rather
/// than a train of packets.
///
/// Sized to a healthy handshake and not to hope: where the host's mapping is
/// open this is one or two round trips, and where it is not, no length of
/// waiting opens it — a lapsed mapping is a port the invite no longer names,
/// and a filtering NAT drops the second packet exactly as it dropped the
/// first. Attempts are what cover a lost packet or a slow first round trip,
/// so the whole train bounds a failed punch at
/// [`OBFUSCATED_CONNECT_ATTEMPTS`] × (this + [`OBFUSCATED_CONNECT_RETRY_BACKOFF_MS`]).
pub const OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS: u64 = 2_000;
/// Pause between one knock poll finishing and the next starting, seconds
/// (ADR 0113).
///
/// It bounds how long a guest waits for the host's half of the punch, and it
/// is the one recurring cost of the rendezvous: every poll is a DHT lookup.
/// Measured on the public DHT (2026-09-24) a poll takes 7-10 s and about 66
/// requests out and 20 answers in, some 15 KB, so this pause makes a host that
/// keeps an invite open cost roughly 0.8 KB/s. It is sized so that a guest's
/// knock is answered within the obfuscated share of one dial round.
pub const RENDEZVOUS_POLL_SECS: u64 = 25 * 60;
/// How often a host republishes an unchanged rendezvous record, seconds
/// (ADR 0113). DHT nodes drop a mutable item they have not been sent again
/// within about two hours; a changed address is published at once.
pub const RENDEZVOUS_REPUBLISH_SECS: u64 = 30 * 60;
/// Longest one rendezvous lookup may run before the caller takes what it has,
/// seconds (ADR 0113).
pub const RENDEZVOUS_LOOKUP_TIMEOUT_SECS: u64 = 10;
/// A knock older than this when the host first sees it is not answered,
/// seconds (ADR 0113): it is a dial that has long given up, and punching
/// towards it would only send packets at an address nobody is listening on.
pub const RENDEZVOUS_KNOCK_FRESH_SECS: u64 = 120;
/// Packets a host sends towards a guest that knocked, one per
/// [`RENDEZVOUS_PUNCH_INTERVAL_MS`] (ADR 0113). Spread over the guest's own
/// punch train, so one of them is on the wire whichever of its attempts is.
pub const RENDEZVOUS_PUNCH_PACKETS: u32 = 10;
/// Spacing of a host's punch packets, milliseconds (ADR 0113).
pub const RENDEZVOUS_PUNCH_INTERVAL_MS: u64 = 1_000;
/// Short-link creation rate limit per IP (§7).
pub const SHORT_LINK_CREATE_RATE_PER_MIN: u32 = 10;
/// Short-link resolution rate limit per IP (§7).
pub const SHORT_LINK_RESOLVE_RATE_PER_MIN: u32 = 30;
/// Hard cutoff for an already active session after a wall-clock rollback (§12.3).
pub const CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS: u64 = 10 * 60;
/// Default encoder frame rate (§11).
pub const ENCODE_DEFAULT_FPS: u8 = 30;
/// Ceiling on the worker threads the software H.264 encoder may use (§11,
/// ADR 0027).
///
/// Only the fallback encoder needs this: a hardware MFT does its own
/// scheduling. What it bounds is a host with no hardware encoder, where
/// `openh264` on one thread produced single-digit frame rates at 1080p while
/// the rest of the machine sat idle. Capped rather than "all cores" because
/// the host is someone's working machine, not a transcoding farm, and §15
/// budgets the session, not the box.
pub const ENCODE_MAX_SOFTWARE_THREADS: u16 = 4;
/// Lowest quantizer the VA-API encoder's own rate control may choose when the
/// driver offers only constant-QP encoding (§11; ADR 0088).
///
/// An Intel iGPU whose `HuC` firmware is not loaded exposes H.264 low-power
/// encoding with `VA_RC_CQP` alone, so bitrate is steered by moving the QP
/// between this and [`VAAPI_CQP_QP_MAX`]. Below 18 H.264 spends bits on detail
/// a desktop picture does not show, and a static screen would ask for it.
pub const VAAPI_CQP_QP_MIN: u8 = 18;
/// Highest quantizer the VA-API encoder's own rate control may choose
/// (§11; ADR 0088).
///
/// Above 44 text stops being readable, which on a remote desktop is the
/// picture failing rather than degrading; the adaptive ladder's lower rungs
/// (frame rate, then scale) are what take a link below that.
pub const VAAPI_CQP_QP_MAX: u8 = 44;
/// How far, in per cent, the smoothed size of a frame may drift from its share
/// of the target bitrate before the constant-QP rate control moves the QP by
/// one step (§11; ADR 0088).
///
/// A band rather than a point, so a picture whose size sits near its target
/// keeps one QP instead of alternating between two every frame.
pub const VAAPI_CQP_RATE_TOLERANCE_PERCENT: u64 = 15;
/// Weight of the newest frame in the constant-QP rate control's running frame
/// size, as a right shift: 3 is one eighth (§11; ADR 0088).
///
/// About a quarter of a second at 30 fps, which is short enough to react to a
/// window being dragged and long enough that one busy frame does not move the
/// quantizer on its own.
pub const VAAPI_CQP_SIZE_SMOOTHING_SHIFT: u32 = 3;
/// Default encoder bitrate (§11).
///
/// Where the adaptive ladder *starts*, not what it spends: the rate control
/// of ADR 0059 is variable, so a still desktop encodes to a small fraction of
/// this and only a screen that is actually moving reaches it. That asymmetry
/// is why the starting point can afford to be generous - a target too low
/// costs sharpness on every moving frame, while a target too high costs
/// nothing at all on a screen nobody is touching.
///
/// Raised with ADR 0060, which is what makes it a different number than it
/// was: a guest that names its own picture size is no longer held to
/// `MAX_PICTURE_PIXELS`, so the same figure now has to cover a 1440p or 4K
/// picture rather than a downscaled 1080p one. Recovery climbs 5% per
/// adjustment, so starting a 1440p session where a 1080p one used to start
/// meant most of a minute of soft picture before the ladder caught up.
pub const ENCODE_DEFAULT_BITRATE_KBPS: u32 = 8_000;
/// Lower bound of the adaptive bitrate range (§11).
pub const ABR_MIN_BITRATE_KBPS: u32 = 300;
/// Upper bound of the adaptive bitrate range (§11).
///
/// Reached only by a link that has carried everything offered to it without
/// loss for long enough to climb there, so this bounds what a *good* link is
/// allowed to spend rather than what a session costs. Raised alongside
/// [`ENCODE_DEFAULT_BITRATE_KBPS`] and for the same ADR 0060 reason: 12 Mbit
/// is a ceiling a 1080p desktop rarely needed and a 4K one hits immediately.
pub const ABR_MAX_BITRATE_KBPS: u32 = 25_000;
/// Receiver feedback interval sent by the guest (§11).
pub const ABR_FEEDBACK_INTERVAL_MS: u32 = 500;
/// How long the host keeps treating a guest's last
/// [`ABR_FEEDBACK_INTERVAL_MS`] report as the truth about the link before it
/// falls back to judging congestion by its own write latency (§11; ADR 0015,
/// ADR 0037).
///
/// Deliberately several report intervals: reports ride the control channel
/// while pictures ride `rd/media/1`, so one late report is ordinary and must
/// not flip the host between two disagreeing congestion signals every second.
/// Long enough for that, short enough that a guest which stops reporting
/// entirely — an old peer, a wedged view — gets the host-local estimate back
/// rather than a quality target frozen where it happened to be.
pub const ABR_FEEDBACK_STALE_AFTER_MS: u64 = 2_000;
/// Maximum rate at which the host applies quality changes (§11).
///
/// Named for the bitrate because that is the only knob §11 has, and it now
/// covers frame rate and resolution as well (ADR 0037): the ceiling is one
/// change of the *whole* target per second, not one per knob. Ripple from
/// three knobs moving independently reads worse than a steadily lower
/// picture.
pub const ABR_ADJUST_MAX_RATE_PER_SEC: u32 = 1;
/// Lower bound of the adaptive frame rate, the second rung of the degradation
/// ladder (§11; ADR 0037).
///
/// Not in the design doc: §11 adapts the bitrate only. Below this a desktop
/// stops reading as a live screen and starts reading as a broken one, which is
/// the failure `ABR_MIN_BITRATE_KBPS` exists to prevent on its own axis.
pub const ABR_MIN_FPS: u8 = 10;
/// Lower bound of the adaptive picture scale, in percent of the captured
/// size — the third and last rung of the ladder (§11; ADR 0037).
///
/// Half of each axis is a quarter of the pixels, which is as far as a remote
/// desktop can be reduced and still have readable text.
pub const ABR_MIN_SCALE_PERCENT: u32 = 50;
/// Step, in percent, by which the adaptive picture scale moves (§11; ADR 0037).
pub const ABR_SCALE_STEP_PERCENT: u32 = 25;
/// Upper bound of a guest's manual stream-scale ceiling request (§11; D7,
/// docs/bugs/13-stream-resolution.md).
///
/// The same value as `lumepeer_media::abr::FULL_SCALE_PERCENT` — restated
/// here, not imported, because `crates/core` cannot depend on
/// `crates/media`, and this is the crate that decodes the wire message and
/// has to bound it.
pub const STREAM_SCALE_MAX_PERCENT: u32 = 100;
/// Step, in frames per second, by which the adaptive frame rate moves
/// (§11; ADR 0037).
pub const ABR_FPS_STEP: u8 = 5;
/// Fraction of the current bitrate target, in percent, that observed goodput
/// must fall under before the host treats the link as unable to carry what it
/// is sending (§11; ADR 0037).
///
/// `rd/media/1` is a reliable ordered QUIC stream, so a guest never reports
/// lost bytes — the honest congestion signal it *can* report is that less
/// arrived per second than was sent. The margin keeps an idle screen, which
/// legitimately produces far less than the target, from reading as congestion:
/// goodput is only consulted while frames are actually flowing.
pub const ABR_GOODPUT_SHORTFALL_PERCENT: u32 = 70;
/// Shortest interval between two keyframes the host will force on a guest's
/// request (§11).
///
/// Not in the design doc: §11 has the request, not a budget for it. A keyframe
/// is the most expensive frame in the stream, so a guest that asks on every
/// frame would turn the request into a way to make the host send nothing else.
/// The host honours at most one request per interval and drops the rest.
pub const KEYFRAME_MIN_INTERVAL_MS: u64 = 1_000;
/// Log rotation by age (§16.1).
pub const LOG_ROTATION_DAYS: u32 = 7;
/// Log rotation by size (§16.1).
pub const LOG_ROTATION_MAX_MIB: u32 = 100;
/// Bound on how long the Windows Media Foundation hardware encoder waits for
/// an async MFT event (`METransformNeedInput`/`METransformHaveOutput`) before
/// treating the encoder as stalled. Not in the design doc: added so a wedged
/// GPU driver fails one `encode()` call instead of hanging the session
/// forever, per the "degrade towards safety, tell the user" rule of §24.5
/// (ADR 0011).
pub const ENCODE_HW_EVENT_TIMEOUT_MS: u64 = 2_000;

/// Maximum UTF-8 byte length of one chat message (§9.2). Chat rides the
/// control channel, so it must always stay well under
/// `MAX_CONTROL_FRAME_BYTES`.
pub const CHAT_MAX_BYTES: usize = 4_096;

/// Maximum pixel area of one cursor shape update (§11). A cursor is UI
/// chrome, never a second video channel; anything larger is malformed.
pub const MAX_CURSOR_SHAPE_PIXELS: usize = 128 * 128;

/// Maximum length of a path a guest may ask a host to list with
/// [`crate::protocol::MessageKind::DirListRequest`] (§9.1; ADR 0075).
///
/// A hostile-peer bound checked before the path is parsed, not a considered
/// maximum: `PATH_MAX` on Linux is this, Windows' extended-length limit is
/// larger and its ordinary one much smaller, and no directory anybody browses
/// comes near it. What it bounds is how much string a peer can make the host
/// walk before it is refused.
pub const DIR_PATH_MAX_BYTES: usize = 4_096;
/// Maximum number of entries one `DirListResponse` may carry (§9.1;
/// ADR 0075).
///
/// A directory with more than this is answered with the first
/// [`MAX_DIR_ENTRIES_PER_RESPONSE`] of them and `truncated: true`, never silently
/// cut short. The value is what fits: one entry is a name of at most
/// [`FILE_NAME_MAX_BYTES`] plus a size, a flag and a timestamp, so 200 of
/// them stay inside [`MAX_CONTROL_FRAME_BYTES`] with room for the envelope —
/// which the assertion below is what actually checks.
pub const MAX_DIR_ENTRIES_PER_RESPONSE: usize = 200;
/// Worst-case encoded size of one `DirEntry`: the longest name it may carry,
/// its postcard length prefix, and the varint forms of a `u64` size, a `bool`
/// and a `u64` timestamp.
const DIR_ENTRY_WORST_CASE_BYTES: usize = FILE_NAME_MAX_BYTES + 2 + 10 + 1 + 10;
/// A full listing may not be a control frame the receiver has to refuse
/// (§3.2, §9.1; ADR 0075).
const _: () = assert!(
    MAX_DIR_ENTRIES_PER_RESPONSE * DIR_ENTRY_WORST_CASE_BYTES <= MAX_CONTROL_FRAME_BYTES,
    "a full DirListResponse cannot fit MAX_CONTROL_FRAME_BYTES"
);

/// Maximum number of files one `DirOffer` manifest may name (§9.2;
/// ADR 0077).
///
/// A manifest is one control frame, and a control frame is
/// [`MAX_CONTROL_FRAME_BYTES`] — so this is not a judgement about how many
/// files a person might want to send, it is what fits, with the assertion
/// below as the actual check. A directory with more entries than this is
/// refused out loud before anything is offered, rather than silently
/// truncated into a tree that arrives missing files (§18).
pub const MAX_DIR_MANIFEST_ENTRIES: usize = 200;
/// Longest relative path one manifest entry may carry (§9.2; ADR 0077).
///
/// A path inside the offered directory, not a path on either machine, so
/// [`DIR_PATH_MAX_BYTES`] would be four kilobytes of room for something that
/// has to fit two hundred times into one frame. 255 is [`FILE_NAME_MAX_BYTES`]
/// again: every component of it must pass the same check a file name passes,
/// and a tree deeper than this is one the receiving filesystem would refuse
/// anyway.
pub const MANIFEST_PATH_MAX_BYTES: usize = 255;
/// Worst-case encoded size of one manifest entry: the longest relative path,
/// its postcard length prefix, and the varint forms of a `u64` size and a
/// `bool`.
const MANIFEST_ENTRY_WORST_CASE_BYTES: usize = MANIFEST_PATH_MAX_BYTES + 2 + 10 + 1;
/// A full manifest may not be a control frame the receiver has to refuse
/// (§3.2, §9.1; ADR 0077).
const _: () = assert!(
    MAX_DIR_MANIFEST_ENTRIES * MANIFEST_ENTRY_WORST_CASE_BYTES <= MAX_CONTROL_FRAME_BYTES,
    "a full DirOffer cannot fit MAX_CONTROL_FRAME_BYTES"
);

/// Most TCP streams one session may have open inside its tunnel at once
/// (§4.1; ADR 0078).
///
/// A tunnel is a guest opening sockets on the host's network, so this is the
/// bound on how many it may hold at a time. Generous enough for a web
/// interface with its images and its API calls, small enough that a
/// forgotten tunnel is not a port scanner.
pub const MAX_TUNNEL_STREAMS_PER_SESSION: usize = 32;
/// Read buffer of one tunnel stream, in bytes (§4.1; ADR 0078).
///
/// One per direction per stream, so the memory a tunnel can hold is this
/// times two times [`MAX_TUNNEL_STREAMS_PER_SESSION`] — 4 MiB at these
/// values, inside the §15 budget for an active session.
pub const TUNNEL_BUFFER_BYTES: usize = 64 * 1024;
/// How long a tunnel stream may carry nothing before it is closed (§4.1;
/// ADR 0078).
///
/// Not a keepalive interval: an idle TCP connection through a tunnel is a
/// socket held open on somebody else's machine, and the guest that opened it
/// can open another. Long enough that a paused download or an editor's idle
/// database connection survives.
pub const TUNNEL_IDLE_TIMEOUT_SECS: u64 = 300;
/// Longest host string a `TunnelOpenRequest` may carry (§9.1; ADR 0078).
///
/// A DNS name's own limit is 253 bytes and an IPv6 literal is shorter than
/// that; the bound exists because this is untrusted input that becomes a
/// resolver call.
pub const TUNNEL_HOST_MAX_BYTES: usize = 253;
/// Most targets a host may put on one session's tunnel allowlist (§8.2;
/// ADR 0078).
///
/// The list is the host naming addresses one at a time, so it is short by
/// construction; the bound is what keeps a UI bug from making it unbounded.
pub const MAX_TUNNEL_TARGETS_PER_SESSION: usize = 16;

/// Most shells one session may have running at once (§4.1; ADR 0079).
///
/// A terminal is a process on somebody else's machine, so this is the bound
/// on how many of them one guest can leave behind. Small on purpose and much
/// smaller than [`MAX_TUNNEL_STREAMS_PER_SESSION`]: a tunnel's streams are
/// short-lived connections a browser opens by itself, and every one of these
/// is a shell a person deliberately started.
pub const MAX_TERMINALS_PER_SESSION: usize = 4;
/// Largest single output frame on `rd/term/1`, in bytes (§9.1; ADR 0079).
///
/// The allocation bound of a length a peer announces, checked before anything
/// reserves it — the same job [`TUNNEL_BUFFER_BYTES`] does one channel over,
/// at the same size, because a shell that dumps a file is exactly as capable
/// of naming a large number as a forwarded socket is.
pub const TERMINAL_OUTPUT_MAX_BYTES: usize = 64 * 1024;
/// How much output one shell may hold for a guest window that has not read it
/// yet, in bytes (ADR 0079).
///
/// The scrollback bound, and it behaves like one: past this the **oldest**
/// bytes go, which is exactly what a terminal that scrolled off the top does.
/// Dropping the newest instead would leave a window showing a prompt that is
/// no longer there.
///
/// Guest-side only, and deliberately so: the host keeps no transcript of
/// anything (§15, ADR 0041), so there is nothing on that side for this to
/// bound. Sixteen times [`TERMINAL_OUTPUT_MAX_BYTES`], which is a build log's
/// worth of scroll for a window that stopped polling, and a bounded cost per
/// shell either way.
pub const TERMINAL_SCROLLBACK_BYTES: usize = 16 * TERMINAL_OUTPUT_MAX_BYTES;
/// Widest terminal a guest may ask a host to allocate, in columns (§9.1;
/// ADR 0079).
///
/// Geometry arrives from the network and becomes a PTY size, so it is bounded
/// at the parse boundary like every other untrusted number. Comfortably past
/// any real window on any real display, and far below the point where a
/// terminal buffer becomes an allocation worth refusing.
pub const TERMINAL_COLS_MAX: u16 = 500;
/// Tallest terminal a guest may ask a host to allocate, in rows (§9.1;
/// ADR 0079). Same reasoning as [`TERMINAL_COLS_MAX`].
pub const TERMINAL_ROWS_MAX: u16 = 300;

/// Maximum number of monitors one host may report in `MonitorsList` (§11).
pub const MAX_MONITORS_PER_HOST: usize = 8;

/// Audio sample rate of the Opus audio channel (§11). Opus internally
/// supports 8/12/16/24/48 kHz; 48 kHz is the only full-band rate and the
/// single fixed value keeps the negotiation trivial.
pub const AUDIO_SAMPLE_RATE_HZ: u32 = 48_000;
/// Audio channel count of the Opus audio channel (§11).
pub const AUDIO_CHANNELS: u8 = 2;
/// Duration of one encoded audio frame in milliseconds (§11). 20 ms is the
/// Opus default and keeps latency under one video frame at 30 fps.
pub const AUDIO_FRAME_MS: u32 = 20;
/// Default audio bitrate (§11): 96 kbit/s, the Opus sweet spot for a
/// stereo desktop-audio mix.
pub const AUDIO_DEFAULT_BITRATE_BPS: i32 = 96_000;
/// Maximum size of one encoded audio frame on the wire (§11). 20 ms of
/// uncompressed 48 kHz stereo s16 is 3 840 bytes; Opus output is smaller,
/// but the bound stays for the length check before allocation.
pub const AUDIO_MAX_FRAME_BYTES: usize = 8 * 1024;

/// Maximum payload of one file-transfer chunk on `rd/file/1` (§9.2). Chunks
/// ride the media framing bound, so they must stay strictly under
/// `MAX_MEDIA_FRAME_BYTES` with room for the chunk header.
pub const FILE_CHUNK_MAX_BYTES: usize = 256 * 1024;
/// Maximum concurrent file transfers per session (§9.2). Mirrors
/// `MAX_PENDING_FILE_OFFERS` for the transfer phase that follows an offer.
pub const MAX_CONCURRENT_FILE_TRANSFERS: usize = 3;
/// How long a chunk stream on `rd/file/1` waits for the `FileTransferStart`
/// that names its transfer before giving up (§9.2; ADR 0032).
///
/// Not in the design doc: it exists because the control channel and the file
/// channel are separate QUIC connections (§4), so nothing orders the start
/// message against the first chunk. The wait is short — this is two messages
/// the same peer sent at the same moment, not a network round trip — and
/// timing out aborts one transfer rather than the connection.
pub const FILE_TRANSFER_START_TIMEOUT_SECS: u64 = 15;

/// Time step of the RFC 6238 TOTP second factor (§8; ADR 0023 §2). 30 s is
/// what every mainstream authenticator app defaults to.
pub const UNATTENDED_TOTP_STEP_SECS: u64 = 30;
/// Consecutive failed unattended verifications before the host locks out
/// brute force (§18).
pub const UNATTENDED_MAX_FAILED_ATTEMPTS: u32 = 5;
/// How long the host refuses every unattended verification once the failure
/// limit is reached (§18).
pub const UNATTENDED_LOCKOUT_DURATION_SECS: u64 = 300;

/// Shortest device password the host will accept when setting one (§8).
///
/// §8 fixes the lockout but not a strength policy, and leaving one out would
/// be a policy decision made by omission: five attempts per
/// `UNATTENDED_LOCKOUT_DURATION_SECS` is only a meaningful defence if the
/// secret has enough room in it to be worth guessing at that rate. Eight bytes
/// is the floor, not a recommendation (ADR 0033).
pub const UNATTENDED_PASSWORD_MIN_BYTES: usize = 8;
/// Longest device password an unattended credential message may carry (§8;
/// §9.1 allocation-DoS check at the parse boundary). Generous enough for a
/// passphrase, far below `MAX_CONTROL_FRAME_BYTES`.
pub const UNATTENDED_PASSWORD_MAX_BYTES: usize = 1024;
/// Longest one-time code an unattended credential message may carry (§8).
///
/// Codes are six digits today (`unattended::Totp`), and verification insists
/// on exactly that. The wire limit leaves a little room on purpose, so a peer
/// sending a longer code fails verification with a coarse `BadCode` instead of
/// having its connection torn down as a malformed frame.
pub const UNATTENDED_CODE_MAX_BYTES: usize = 8;

/// How long the host keeps an audit record before deleting it (§15).
///
/// §15 fixes the retention, and that is the whole policy: records older than
/// this are removed unconditionally, protocol violations included. "Keep the
/// interesting ones longer" would be a retention decision §15 did not make,
/// and a log that quietly outlives its stated retention is worse than no log.
pub const AUDIT_RETENTION_DAYS: u64 = 30;

/// How often the host sweeps records past [`AUDIT_RETENTION_DAYS`] (§15).
///
/// Once a day, plus once at startup. Sweeping per append would turn every
/// consent decision into a table scan, and the cutoff moves by seconds — a
/// record can outlive its retention by up to this long, which is the price of
/// not paying for a scan the host has no reason to make.
pub const AUDIT_RETENTION_SWEEP_SECS: u64 = 24 * 60 * 60;

/// Maximum number of files in one clipboard file list, whether read off this
/// machine's own OS clipboard or announced in a `ClipboardFileOffer`
/// (docs/bugs/14-clipboard-files.md #1, #2).
///
/// Not in the design doc: §9.2 v1 was text-only. A clipboard file list is
/// untrusted input on both ends this crate has to protect — another local
/// application's clipboard on the reading side, an unauthenticated-until-
/// granted peer's message on the wire side — and both are checked against
/// this bound before anything is allocated per entry. Equal to
/// `MAX_PENDING_FILE_OFFERS`, the actual ceiling on how many of them the
/// transfer engine can ever queue at once: announcing more than that would
/// only ever be declined further down the same pipeline.
pub const CLIPBOARD_FILE_LIST_MAX_ENTRIES: usize = MAX_PENDING_FILE_OFFERS;

/// Maximum byte length of one path (or `file://` URI) read off this
/// machine's own OS clipboard, checked before it is decoded into a `PathBuf`
/// (docs/bugs/14-clipboard-files.md #1).
///
/// Not in the design doc, and not the same bound as `FILE_NAME_MAX_BYTES`:
/// that one bounds a basename that crosses the wire, this one bounds a raw
/// clipboard entry before it is even split into a basename on this machine.
/// 4096 covers `PATH_MAX` on Linux and macOS and is generous for Windows'
/// legacy `MAX_PATH`, while still refusing the pathological case a hostile
/// local clipboard owner could otherwise hand this process.
pub const CLIPBOARD_FILE_PATH_MAX_BYTES: usize = 4096;

/// Maximum number of display modes one host may report for its currently
/// captured monitor, whether enumerated locally or announced in
/// `DisplayModesList` (docs/bugs/16-host-display-mode.md #1; D7 point 2).
///
/// A hostile-peer bound, not a shortlist: the real backends already fold
/// their raw list down to one mode per resolution
/// ([`crate::constants`]' consumers call
/// `fold_display_modes_by_resolution`), so a real monitor lands well under
/// this — a 4K Windows driver reports about thirty distinct resolutions,
/// legacy VESA sizes included. What is left to bound is the count a peer
/// can claim before anything downstream allocates per mode.
pub const MAX_DISPLAY_MODES_PER_HOST: usize = 128;

/// How long the host waits, after switching its own physical display mode,
/// for something to confirm the new mode actually produces a picture before
/// reverting on its own (docs/bugs/16-host-display-mode.md #3).
///
/// Changing display mode is the riskiest control this project hands a guest:
/// unlike every other grant, it can strand the operator in front of a black
/// or garbled screen with no session left to fix it from. Confirmation is
/// the host's own capture backend successfully restarting on the new mode
/// and handing back a frame — nothing here waits on a human, attended or not,
/// because a mechanism that only saves someone who clicks in time is not a
/// safety net. 10 seconds is comfortably past how long a monitor takes to
/// resync to a new signal (typically 1-3 s) and short enough that a genuinely
/// broken mode does not strand the desktop for long.
pub const DISPLAY_MODE_CONFIRM_TIMEOUT_SECS: u64 = 10;

/// How long the host warns the person in front of it before a guest's
/// accepted `RebootRequest` actually takes the machine down (§4.1; ADR 0084).
///
/// Not a formality. Dropping somebody's machine without a word is the one
/// thing this grant must not be able to do, so the warning is unconditional —
/// it is shown even for a guest the host trusts completely, and the restart
/// is a separate act that happens when this window closes rather than when
/// the request arrived. The grant is re-read at that moment, so a revoke
/// landing inside the window stops the restart.
///
/// Ten seconds is the trade: long enough that somebody looking at the screen
/// can read the banner and reach the button, short enough that an operator
/// who is *not* there does not turn a remote restart into a minute of
/// waiting. It deliberately matches
/// [`DISPLAY_MODE_CONFIRM_TIMEOUT_SECS`] rather than inventing a second
/// number for "how long a host has to notice something drastic".
pub const REBOOT_WARNING_SECS: u64 = 10;

/// How long the guest waits between attempts to raise a **new** session with
/// a host that went away (§10; ADR 0084).
///
/// The wait that follows [`RECONNECT_WINDOW_SECS`], and a different thing
/// from it: that window is resume — same session, same grants — and it is
/// deliberately far shorter than a reboot, because grants must not survive
/// one. What happens after it elapses is an ordinary dial that ends in an
/// ordinary consent or an ordinary device password, and this is how often it
/// is tried.
///
/// A flat interval rather than a backoff. A machine coming back from a
/// restart is not a congested server: it is unreachable for a while and then
/// abruptly reachable, so what matters is how soon after that moment the next
/// attempt lands, and a doubling interval is at its worst exactly then. The
/// cost of a flat interval is bounded by [`REBOOT_WAIT_CEILING_SECS`], and
/// each attempt is one dial to one host the user asked for.
pub const REBOOT_WAIT_RETRY_SECS: u64 = 15;

/// How long the guest keeps waiting for a host to come back before giving up
/// and leaving the reconnect to the person (§10, §18; ADR 0084).
///
/// A machine that has not answered in ten minutes is not rebooting: it is off,
/// or its network is gone, or somebody cancelled the restart and walked away.
/// Trying forever would leave a dial loop running against a host nobody is
/// waiting for, so the wait ends and the ordinary "connect again" button is
/// what is left.
pub const REBOOT_WAIT_CEILING_SECS: u64 = 600;

/// The wait for a rebooting host has to outlast the resume window, or it would
/// only ever be tried inside it (§10; ADR 0084).
///
/// Stated as an assertion rather than left to the reader because the two
/// numbers mean opposite things: [`RECONNECT_WINDOW_SECS`] is how long a
/// session may be resumed *with its grants*, and stretching it to cover a
/// reboot is exactly what ADR 0084 refuses to do.
const _: () = assert!(
    REBOOT_WAIT_CEILING_SECS > RECONNECT_WINDOW_SECS,
    "the wait for a rebooting host must outlast the resume window"
);

/// How often a guest whose session dropped tries to resume it while
/// [`RECONNECT_WINDOW_SECS`] is still open (§10; ADR 0089).
///
/// Short, because a resume is what repairs an ordinary network blip and every
/// second of it is a second the picture is gone. Not shorter, because each
/// attempt is a full dial of every transport the plan has, and a link that is
/// down answers none of them quickly.
pub const RESUME_RETRY_SECS: u64 = 3;

/// Attempts one resume dial makes before the wait's next tick makes another
/// (§10; ADR 0091).
///
/// Two, not [`DIAL_ATTEMPTS`]'s five, because the retry loop of a resume is
/// the wait itself: [`RESUME_RETRY_SECS`] already asks again, and spending the
/// first-connection budget inside one attempt made [`RESUME_RETRY_SECS`]
/// meaningless — a single dial could take [`DIAL_TOTAL_BUDGET_SECS`], which
/// is longer than the whole window used to be, so a resume got exactly one
/// try and the window expired underneath it.
///
/// Two rather than one because the two attempts take different routes: the
/// odd one dials the addresses the ticket carries and the even one asks
/// discovery where the host is *now* (`by_lookup` in `dial_with_retries`).
/// After the kind of network change a resume exists for, the second is the
/// one that can work, so a one-attempt dial would never take it.
pub const RESUME_ATTEMPTS: u32 = 2;

/// Bound on one attempt of [`RESUME_ATTEMPTS`] — dial *and* handshake
/// together (§10; ADR 0091).
///
/// Much shorter than [`CONNECT_ATTEMPT_TIMEOUT_SECS`], and for a reason that
/// does not apply to a first connection: a resume is dialing a host it was
/// talking to seconds ago, over a path one side has just lost. Either that
/// path is back, in which case it answers quickly, or it is not, in which
/// case waiting is worse than asking again — the wait's own tick is the
/// retry, and there are a hundred of them in a window.
pub const RESUME_ATTEMPT_TIMEOUT_SECS: u64 = 5;

/// Worst case one resume dial may cost, derived the way
/// [`DIAL_TOTAL_BUDGET_SECS`] is.
pub const RESUME_DIAL_BUDGET_SECS: u64 = RESUME_ATTEMPTS as u64 * RESUME_ATTEMPT_TIMEOUT_SECS
    + ((RESUME_ATTEMPTS as u64 - 1) * (DIAL_RETRY_BACKOFF_MS + DIAL_RETRY_BACKOFF_JITTER_MS))
        / 1_000;

/// A resume has to get several *dials* inside its window, or it is not a retry
/// at all (§10; ADR 0089, ADR 0091).
///
/// Stated against the budget of a whole dial rather than against
/// [`RESUME_RETRY_SECS`], which is what ADR 0089 asserted and what made this
/// check pass while the behaviour it describes was false: the tick cannot ask
/// again while a dial is still in flight, so the cadence that matters is the
/// dial's own cost, not the timer's.
const _: () = assert!(
    RESUME_DIAL_BUDGET_SECS * 4 < RECONNECT_WINDOW_SECS,
    "a resume must get several dials inside the reconnect window"
);
