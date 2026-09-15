//! The whole channel between the privileged host and its session agent
//! (ADR 0085).
//!
//! ADR 0043's protocol is two bytes in, two bytes out because the process on
//! the far side runs as `LocalSystem` and the process on this side does not.
//! This channel is the other way round — the privileged side is the one
//! *sending* — and it needs its own argument, because "the privileged side is
//! talking, so it must be fine" is exactly the reasoning that puts a local
//! privilege escalation into a codebase.
//!
//! Three rules, and every message below exists only because it satisfies all
//! three:
//!
//! - **The agent is told what to do; it is never asked what is permitted.**
//!   No message an agent can send names a peer, a role, a grant or a session
//!   of the protocol's own. There is no reply that means "yes, allow it",
//!   because there is no question that asks. Authorization was made in
//!   `lumepeer-core`, in the host, before this channel was touched — the same
//!   rule ADR 0049 §4 and ADR 0057 §4 already set for the secure-desktop
//!   worker, restated here because this peer is long-lived where that one is
//!   not.
//! - **Direction is in the wire, not in the reader.** A command's kind byte
//!   and an event's kind byte come from disjoint ranges ([`is_command`],
//!   [`is_event`]), so an agent cannot send something the host will read as a
//!   command by getting the framing right, and vice versa. This is not
//!   authorization — the channel's DACL and the host's own pid check are —
//!   but it removes a whole class of confusion before it can be attempted.
//! - **Fixed shape, no length field.** Every message is exactly
//!   [`AGENT_MESSAGE_LEN`] bytes with a slot per field whether or not this
//!   kind uses it, exactly the way [`crate::protocol::encode_inject`] already
//!   does. Nothing on this wire is sized by the peer, so a short read is an
//!   error rather than a state to reassemble, and no peer can drive an
//!   allocation on the other side.
//!
//! Nothing here panics on any input. Every parse returns `None` for anything
//! it does not recognize, which is this crate's established answer to a
//! request it did not understand (`protocol.rs`: the caller learns it did not
//! work and nothing about why).

/// First byte of every message in both directions.
///
/// Not a security measure — anything that can open the channel can send it.
/// It is here so that something else connecting by accident is rejected as
/// garbage instead of being parsed as an operation, the same job
/// [`crate::protocol::MAGIC`] does on the helper's own pipe.
pub const AGENT_MAGIC: u8 = b'A';

/// Bytes in one message, in either direction.
///
/// Fixed at compile time and never named by either peer. The layout is
/// `magic:u8 | kind:u8 | flag:u8 | reason:u8 | word:u32 | x:u16 | y:u16`,
/// little-endian, read and written as a plain byte array the same way the
/// frame mapping's header is — so there is no struct layout to get wrong
/// across the two sides.
///
/// Local to `crates/service` rather than in `crates/core/src/constants.rs`
/// for the reason ADR 0049 §2 already recorded for this crate's other wire
/// constants: `lumepeer-core` is deliberately not on this binary's dependency
/// list, and that list is part of its security argument.
pub const AGENT_MESSAGE_LEN: usize = 12;

/// Byte 0 of the `word` slot within a message.
const WORD_AT: usize = 4;
/// Byte 0 of the `x` slot within a message.
const X_AT: usize = 8;
/// Byte 0 of the `y` slot within a message.
const Y_AT: usize = 10;

/// Kind byte of [`AgentCommand::ShowIndicator`].
const KIND_SHOW_INDICATOR: u8 = 0x01;
/// Kind byte of [`AgentCommand::StartCapture`].
const KIND_START_CAPTURE: u8 = 0x02;
/// Kind byte of [`AgentCommand::StopCapture`].
const KIND_STOP_CAPTURE: u8 = 0x03;
/// Kind byte of [`AgentCommand::PointerMove`].
const KIND_POINTER_MOVE: u8 = 0x04;
/// Kind byte of [`AgentCommand::Press`].
const KIND_PRESS: u8 = 0x05;
/// Kind byte of [`AgentCommand::Release`].
const KIND_RELEASE: u8 = 0x06;
/// Kind byte of [`AgentCommand::Wheel`].
const KIND_WHEEL: u8 = 0x07;
/// Kind byte of [`AgentCommand::Shutdown`].
const KIND_SHUTDOWN: u8 = 0x08;

/// Kind byte of [`AgentEvent::Attached`].
const KIND_ATTACHED: u8 = 0x81;
/// Kind byte of [`AgentEvent::FramePublished`].
const KIND_FRAME_PUBLISHED: u8 = 0x82;
/// Kind byte of [`AgentEvent::CaptureUnavailable`].
const KIND_CAPTURE_UNAVAILABLE: u8 = 0x83;
/// Kind byte of [`AgentEvent::Detaching`].
const KIND_DETACHING: u8 = 0x84;

/// The bit that separates the two directions.
///
/// Commands are below it, events at or above it. See the module header: this
/// is confusion-avoidance, not authorization.
const EVENT_KIND_FLOOR: u8 = 0x80;

/// Name of the pipe the privileged host and its session agent talk over.
///
/// A local pipe, and its own: the helper's request endpoint
/// ([`crate::protocol::ENDPOINT`]) parses two-byte frames from interactive
/// users, and putting twelve-byte agent messages in front of that parser would
/// make one endpoint mean two things. `crates/service/src/agent_channel.rs`
/// carries the access list and the process check that bound who may connect.
#[cfg(target_os = "windows")]
pub const AGENT_ENDPOINT: &str = r"\\.\pipe\lumepeer-session-agent";

/// Name of the shared-memory mapping the session agent publishes encoded
/// frames into (ADR 0085).
///
/// `Global\` for the reason [`crate::protocol::SECURE_DESKTOP_MAPPING_NAME`]
/// already records: the host runs in session 0 and the agent runs in an
/// interactive session, and a name without the prefix would be created in the
/// caller's own session-private namespace — invisible across exactly the
/// boundary this mapping exists to cross.
///
/// Distinct from the secure-desktop mapping, and not reused: the two have
/// different writers, different lifetimes and different access lists, and a
/// single mapping serving both would have to carry the widest of each.
pub const AGENT_FRAME_MAPPING_NAME: &str = r"Global\lumepeer-session-agent-frame";

/// Capacity of the agent mapping's payload region: one **encoded** frame.
///
/// The agent captures *and encodes* (ADR 0085 §1), so what crosses this
/// boundary is a bitstream, not a screen's worth of pixels. That is the
/// reason the privileged process links no media pipeline at all, and it is
/// also why this bound is eight mebibytes rather than a picture size: it is
/// the same ceiling `lumepeer_core::constants::MAX_MEDIA_FRAME_BYTES` already
/// puts on one encoded frame everywhere else in this codebase.
///
/// Kept as a literal here rather than imported, for the reason ADR 0049 §2
/// recorded for this crate's other wire constants: `crates/service` does not
/// depend on `lumepeer-core`, so raising one bound without the other is
/// caught by review rather than by the compiler. A frame larger than this is
/// refused by [`crate::frame`] rather than published truncated — a partial
/// bitstream is a corrupt one, not a smaller one.
pub const AGENT_FRAME_CAPACITY_BYTES: usize = 8 * 1024 * 1024;

/// Whether `kind` names a message only the privileged host may send.
#[must_use]
pub const fn is_command(kind: u8) -> bool {
    kind < EVENT_KIND_FLOOR
}

/// Whether `kind` names a message only the agent may send.
#[must_use]
pub const fn is_event(kind: u8) -> bool {
    kind >= EVENT_KIND_FLOOR
}

/// Everything the privileged host can ask its session agent to do.
///
/// The whole list, and it is the list on purpose: ADR 0043's "each capability
/// of the privileged side is enumerated and justified" applies to what the
/// privileged side *causes* as much as to what it is asked for. There is no
/// general-purpose member here — no "run this", no "open that", no path and
/// no string anywhere in the encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCommand {
    /// Raise or lower the session indicator the person at this machine cannot
    /// dismiss (§2.2; ADR 0033).
    ///
    /// First, always: the host sends this when an agent attaches, and only
    /// starts capture afterwards. An agent that captured first would have a
    /// window — however short — in which pixels left a machine showing no
    /// sign of it, which is the one thing ADR 0085 §3 is built to make
    /// unreachable.
    ShowIndicator {
        /// Whether the indicator is up.
        on: bool,
    },
    /// Begin publishing frames of `monitor` into the frame mapping.
    ///
    /// The monitor is an index into what the agent's own session can see. The
    /// host cannot name a desktop, a window or a process — there is nothing
    /// in the encoding that could.
    StartCapture {
        /// Zero-based index of the monitor in the agent's own enumeration.
        monitor: u32,
    },
    /// Stop publishing frames.
    StopCapture,
    /// Move the pointer to an absolute, normalized point on the agent's
    /// session.
    ///
    /// Already authorized: `SessionManager::authorize_input` ran in the host
    /// before this was sent, exactly as it does for the in-process injector
    /// today. The agent performs it; it never decides it.
    PointerMove {
        /// Horizontal position, `0..=65535`.
        x: u16,
        /// Vertical position, `0..=65535`.
        y: u16,
    },
    /// Press the key or pointer button named by `logical`.
    ///
    /// `logical` is the guest's own logical key/button code — the same
    /// encoding `lumepeer_core::protocol::InputEventPayload::logical` carries
    /// and [`crate::protocol::InjectAction`] already forwards to the
    /// secure-desktop worker.
    Press {
        /// The guest's logical key/button code.
        logical: u32,
    },
    /// Release the key or pointer button named by `logical`.
    Release {
        /// The guest's logical key/button code.
        logical: u32,
    },
    /// Scroll by a signed delta on the agent's session.
    ///
    /// Here rather than folded into [`Press`](Self::Press) because a wheel is
    /// the one input event with no press and no release — a host that could
    /// forward keys and buttons but not scrolling would leave a guest on a
    /// service host silently unable to read a page, which is exactly the quiet
    /// degradation §18 forbids.
    ///
    /// The deltas travel in the `x` and `y` slots, reinterpreted as signed:
    /// the layout does not grow, because the slots are the right width and a
    /// second pair would be two bytes every message carries and six of the
    /// seven never use.
    Wheel {
        /// Horizontal delta, as the guest sent it.
        dx: i16,
        /// Vertical delta, as the guest sent it.
        dy: i16,
    },
    /// Stop capturing, drop the indicator and exit.
    ///
    /// The ordinary way an agent ends: the host stopped hosting, or is handing
    /// the role over (ADR 0085 §4). An agent that loses the channel instead —
    /// the host died, the session ended — exits on its own, so this is a
    /// courtesy rather than the only path.
    Shutdown,
}

/// Everything the session agent can tell the privileged host.
///
/// Facts about a desktop, and nothing else. There is deliberately no variant
/// that carries a peer, a role, a grant or a verdict: the host asks the agent
/// nothing it would have to trust the answer to, so a compromised agent
/// cannot widen anything — it can only lie about a screen it is already
/// showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEvent {
    /// The agent is up in `session` and the indicator is showing.
    ///
    /// The host treats this as "there is a screen now", never as "this agent
    /// may do X": it re-reads its own grants for every event it forwards,
    /// exactly as the actor already does before every injected key.
    Attached {
        /// Windows session id the agent is running in.
        session: u32,
    },
    /// A frame numbered `sequence` is in the mapping.
    ///
    /// The counter exists so a host can tell a fresh frame from the one it
    /// already read; it is the agent's own, monotonic within one attachment,
    /// and the host treats a repeat or a jump as "no new frame" rather than
    /// as an error worth ending an attachment over.
    FramePublished {
        /// The agent's own frame counter.
        sequence: u32,
    },
    /// This session cannot produce a picture, and why — so the host can send
    /// the guest the honest `MediaUnavailable` it already sends rather than a
    /// frozen frame (§18; ADR 0024).
    CaptureUnavailable {
        /// Which of the closed set of reasons applies.
        reason: CaptureUnavailableReason,
    },
    /// The agent is going away: the session is ending, or it was told to stop.
    ///
    /// Advisory. The host must not depend on seeing it — a session that ends
    /// abruptly kills the agent without a word — which is why the host also
    /// watches the process itself (`agent_launch`).
    Detaching,
}

/// Why an agent has no picture to publish.
///
/// A closed set, deliberately coarse. It says enough for the host to pick the
/// right `MediaUnavailableReason` for the guest and nothing that describes
/// this machine's configuration in more detail than the guest already learns
/// from the absence of a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureUnavailableReason {
    /// This build or this platform has no capture backend at all.
    NoBackend,
    /// There is a backend and it refused or failed.
    Failed,
    /// The session has no display attached to capture.
    NoDisplay,
}

/// Wire byte of [`CaptureUnavailableReason::NoBackend`].
const REASON_NO_BACKEND: u8 = 0x01;
/// Wire byte of [`CaptureUnavailableReason::Failed`].
const REASON_FAILED: u8 = 0x02;
/// Wire byte of [`CaptureUnavailableReason::NoDisplay`].
const REASON_NO_DISPLAY: u8 = 0x03;

impl CaptureUnavailableReason {
    /// This reason's wire byte.
    #[must_use]
    const fn to_byte(self) -> u8 {
        match self {
            Self::NoBackend => REASON_NO_BACKEND,
            Self::Failed => REASON_FAILED,
            Self::NoDisplay => REASON_NO_DISPLAY,
        }
    }

    /// The reason a wire byte names, or `None` for one this protocol does not
    /// know. Never a default: a reason nobody wrote is not "failed", it is a
    /// message that did not parse.
    #[must_use]
    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            REASON_NO_BACKEND => Some(Self::NoBackend),
            REASON_FAILED => Some(Self::Failed),
            REASON_NO_DISPLAY => Some(Self::NoDisplay),
            _ => None,
        }
    }
}

/// Writes the fixed header shared by both directions.
fn frame_of(kind: u8) -> [u8; AGENT_MESSAGE_LEN] {
    let mut out = [0u8; AGENT_MESSAGE_LEN];
    out[0] = AGENT_MAGIC;
    out[1] = kind;
    out
}

/// Reads the `word` slot.
const fn word_of(message: &[u8; AGENT_MESSAGE_LEN]) -> u32 {
    u32::from_le_bytes([
        message[WORD_AT],
        message[WORD_AT + 1],
        message[WORD_AT + 2],
        message[WORD_AT + 3],
    ])
}

/// Reads the `x` slot.
const fn x_of(message: &[u8; AGENT_MESSAGE_LEN]) -> u16 {
    u16::from_le_bytes([message[X_AT], message[X_AT + 1]])
}

/// Reads the `y` slot.
const fn y_of(message: &[u8; AGENT_MESSAGE_LEN]) -> u16 {
    u16::from_le_bytes([message[Y_AT], message[Y_AT + 1]])
}

/// Serializes a command into the fixed message the agent reads.
#[must_use]
pub fn encode_command(command: AgentCommand) -> [u8; AGENT_MESSAGE_LEN] {
    let mut out;
    match command {
        AgentCommand::ShowIndicator { on } => {
            out = frame_of(KIND_SHOW_INDICATOR);
            out[2] = u8::from(on);
        }
        AgentCommand::StartCapture { monitor } => {
            out = frame_of(KIND_START_CAPTURE);
            out[WORD_AT..WORD_AT + 4].copy_from_slice(&monitor.to_le_bytes());
        }
        AgentCommand::StopCapture => out = frame_of(KIND_STOP_CAPTURE),
        AgentCommand::PointerMove { x, y } => {
            out = frame_of(KIND_POINTER_MOVE);
            out[X_AT..X_AT + 2].copy_from_slice(&x.to_le_bytes());
            out[Y_AT..Y_AT + 2].copy_from_slice(&y.to_le_bytes());
        }
        AgentCommand::Press { logical } => {
            out = frame_of(KIND_PRESS);
            out[WORD_AT..WORD_AT + 4].copy_from_slice(&logical.to_le_bytes());
        }
        AgentCommand::Release { logical } => {
            out = frame_of(KIND_RELEASE);
            out[WORD_AT..WORD_AT + 4].copy_from_slice(&logical.to_le_bytes());
        }
        AgentCommand::Wheel { dx, dy } => {
            out = frame_of(KIND_WHEEL);
            out[X_AT..X_AT + 2].copy_from_slice(&dx.to_le_bytes());
            out[Y_AT..Y_AT + 2].copy_from_slice(&dy.to_le_bytes());
        }
        AgentCommand::Shutdown => out = frame_of(KIND_SHUTDOWN),
    }
    out
}

/// The command a message names, or `None` if it is not a well-formed command.
///
/// `None` covers all four ways a message can fail to be one — the wrong magic,
/// a kind from the event range, a kind nothing defines, and a payload byte
/// outside its field's range — because the agent's answer to every one of them
/// is the same: do nothing, and do not guess. An agent that guessed at a
/// message it did not understand would be a privileged instruction carried out
/// approximately.
#[must_use]
pub fn parse_command(message: &[u8; AGENT_MESSAGE_LEN]) -> Option<AgentCommand> {
    if message[0] != AGENT_MAGIC || !is_command(message[1]) {
        return None;
    }
    match message[1] {
        KIND_SHOW_INDICATOR => match message[2] {
            0 => Some(AgentCommand::ShowIndicator { on: false }),
            1 => Some(AgentCommand::ShowIndicator { on: true }),
            // A flag that is neither is not "probably on". Refusing is what
            // keeps the indicator from ever being raised or lowered by a byte
            // nobody meant to write.
            _ => None,
        },
        KIND_START_CAPTURE => Some(AgentCommand::StartCapture {
            monitor: word_of(message),
        }),
        KIND_STOP_CAPTURE => Some(AgentCommand::StopCapture),
        KIND_POINTER_MOVE => Some(AgentCommand::PointerMove {
            x: x_of(message),
            y: y_of(message),
        }),
        KIND_PRESS => Some(AgentCommand::Press {
            logical: word_of(message),
        }),
        KIND_RELEASE => Some(AgentCommand::Release {
            logical: word_of(message),
        }),
        // Every bit pattern is a delta: unlike the indicator's flag there is
        // no reserved value here, so there is nothing to refuse.
        KIND_WHEEL => Some(AgentCommand::Wheel {
            dx: x_of(message).cast_signed(),
            dy: y_of(message).cast_signed(),
        }),
        KIND_SHUTDOWN => Some(AgentCommand::Shutdown),
        _ => None,
    }
}

/// Serializes an event into the fixed message the host reads.
#[must_use]
pub fn encode_event(event: AgentEvent) -> [u8; AGENT_MESSAGE_LEN] {
    let mut out;
    match event {
        AgentEvent::Attached { session } => {
            out = frame_of(KIND_ATTACHED);
            out[WORD_AT..WORD_AT + 4].copy_from_slice(&session.to_le_bytes());
        }
        AgentEvent::FramePublished { sequence } => {
            out = frame_of(KIND_FRAME_PUBLISHED);
            out[WORD_AT..WORD_AT + 4].copy_from_slice(&sequence.to_le_bytes());
        }
        AgentEvent::CaptureUnavailable { reason } => {
            out = frame_of(KIND_CAPTURE_UNAVAILABLE);
            out[3] = reason.to_byte();
        }
        AgentEvent::Detaching => out = frame_of(KIND_DETACHING),
    }
    out
}

/// The event a message names, or `None` if it is not a well-formed event.
///
/// Same four failures as [`parse_command`], and the same answer. A host that
/// cannot parse what its agent said treats the attachment as having told it
/// nothing, which leaves the guest on the honest "no picture" state rather
/// than on a guess about a screen.
#[must_use]
pub fn parse_event(message: &[u8; AGENT_MESSAGE_LEN]) -> Option<AgentEvent> {
    if message[0] != AGENT_MAGIC || !is_event(message[1]) {
        return None;
    }
    match message[1] {
        KIND_ATTACHED => Some(AgentEvent::Attached {
            session: word_of(message),
        }),
        KIND_FRAME_PUBLISHED => Some(AgentEvent::FramePublished {
            sequence: word_of(message),
        }),
        KIND_CAPTURE_UNAVAILABLE => CaptureUnavailableReason::from_byte(message[3])
            .map(|reason| AgentEvent::CaptureUnavailable { reason }),
        KIND_DETACHING => Some(AgentEvent::Detaching),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command survives the fixed message unchanged, including the
    /// coordinate and code extremes the encoding has to carry.
    #[test]
    fn every_command_round_trips() {
        for command in [
            AgentCommand::ShowIndicator { on: true },
            AgentCommand::ShowIndicator { on: false },
            AgentCommand::StartCapture { monitor: 0 },
            AgentCommand::StartCapture { monitor: u32::MAX },
            AgentCommand::StopCapture,
            AgentCommand::PointerMove { x: 0, y: 0 },
            AgentCommand::PointerMove {
                x: u16::MAX,
                y: u16::MAX,
            },
            AgentCommand::PointerMove {
                x: 12_345,
                y: 54_321,
            },
            AgentCommand::Press { logical: 0x0d },
            AgentCommand::Release { logical: u32::MAX },
            // Both signs and both extremes: a delta that came back unsigned
            // would scroll a page the wrong way, which is the kind of bug the
            // round trip is here to catch.
            AgentCommand::Wheel { dx: 0, dy: 0 },
            AgentCommand::Wheel { dx: -1, dy: 1 },
            AgentCommand::Wheel {
                dx: i16::MIN,
                dy: i16::MAX,
            },
            AgentCommand::Shutdown,
        ] {
            assert_eq!(parse_command(&encode_command(command)), Some(command));
        }
    }

    /// Every event survives the fixed message unchanged, including each of
    /// the closed reasons.
    #[test]
    fn every_event_round_trips() {
        for event in [
            AgentEvent::Attached { session: 0 },
            AgentEvent::Attached { session: u32::MAX },
            AgentEvent::FramePublished { sequence: 0 },
            AgentEvent::FramePublished { sequence: u32::MAX },
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::NoBackend,
            },
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::Failed,
            },
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::NoDisplay,
            },
            AgentEvent::Detaching,
        ] {
            assert_eq!(parse_event(&encode_event(event)), Some(event));
        }
    }

    /// The property the whole channel rests on: an agent cannot say anything
    /// the host will carry out. Every command the host can send is refused by
    /// the event reader, and every event the agent can send is refused by the
    /// command reader — by the kind byte's range, before any field is read.
    #[test]
    fn neither_direction_parses_as_the_other() {
        for command in [
            AgentCommand::ShowIndicator { on: true },
            AgentCommand::StartCapture { monitor: 3 },
            AgentCommand::StopCapture,
            AgentCommand::PointerMove { x: 7, y: 9 },
            AgentCommand::Press { logical: 1 },
            AgentCommand::Release { logical: 1 },
            AgentCommand::Shutdown,
        ] {
            assert_eq!(parse_event(&encode_command(command)), None);
        }
        for event in [
            AgentEvent::Attached { session: 1 },
            AgentEvent::FramePublished { sequence: 1 },
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::Failed,
            },
            AgentEvent::Detaching,
        ] {
            assert_eq!(parse_command(&encode_event(event)), None);
        }
    }

    /// A message without the magic is not a message, in either direction —
    /// the same reading `protocol.rs` gives a frame on the helper's pipe.
    #[test]
    fn a_message_without_the_magic_is_refused() {
        let mut command = encode_command(AgentCommand::StopCapture);
        command[0] = 0;
        assert_eq!(parse_command(&command), None);
        let mut event = encode_event(AgentEvent::Detaching);
        event[0] = b'L';
        assert_eq!(parse_event(&event), None);
    }

    /// Exhaustive over the whole byte: no kind outside the two defined sets
    /// parses as anything, and no combination of payload bytes can make one.
    /// This is the "no panic on untrusted input" property stated as a test —
    /// the channel's far end is a process the host launched, but a long-lived
    /// one in an interactive session, which is exactly the place a hostile
    /// local process would try to stand.
    #[test]
    fn no_byte_sequence_panics_or_invents_a_message() {
        for kind in 0..=u8::MAX {
            for payload in [0u8, 1, 2, 0xff] {
                let mut message = [payload; AGENT_MESSAGE_LEN];
                message[0] = AGENT_MAGIC;
                message[1] = kind;
                let command = parse_command(&message);
                let event = parse_event(&message);
                assert!(
                    command.is_none() || event.is_none(),
                    "a message must never read as both a command and an event"
                );
                if command.is_some() {
                    assert!(is_command(kind));
                }
                if event.is_some() {
                    assert!(is_event(kind));
                }
            }
        }
    }

    /// An indicator flag that is neither on nor off is refused rather than
    /// rounded towards either: the one message whose whole job is §2.2's
    /// visibility must not be movable by a byte nobody meant to write.
    #[test]
    fn an_out_of_range_indicator_flag_is_refused() {
        let mut message = encode_command(AgentCommand::ShowIndicator { on: true });
        message[2] = 2;
        assert_eq!(parse_command(&message), None);
    }

    /// A reason byte nothing defines is refused, never defaulted — a host
    /// that read an unknown reason as `Failed` would be inventing a fact
    /// about a machine it cannot see.
    #[test]
    fn an_unknown_capture_reason_is_refused() {
        let mut message = encode_event(AgentEvent::CaptureUnavailable {
            reason: CaptureUnavailableReason::Failed,
        });
        message[3] = 0x7f;
        assert_eq!(parse_event(&message), None);
    }

    /// The two ranges partition the byte, so there is no kind that is neither
    /// and none that is both.
    #[test]
    fn the_two_directions_partition_the_kind_byte() {
        for kind in 0..=u8::MAX {
            assert_ne!(is_command(kind), is_event(kind));
        }
    }
}
