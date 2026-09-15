//! What a failed dial is called, without saying why it failed (§18).
//!
//! One classification, two readers. The IPC layer turns it into the error a
//! command returns, and the actor keeps the code on a dial that failed off the
//! actor loop (ADR 0027) for `connect_status` to hand to the webview, which
//! owns the wording in the user's language. They have to agree, and the way to
//! make them agree is for there to be one of them.
//!
//! It lives in the runtime crate rather than next to the IPC surface because
//! the actor is the side that produces these: a front end that is not a
//! webview at all still needs to know what to call a dial that did not land.
//!
//! The rule the table follows is §18's: say what *this* side observed, never
//! what the far end decided. A refused connection and a flapping link have to
//! read differently, or somebody goes hunting on the wrong machine (ADR 0026).

/// The §18 code of a transport failure, without its message.
///
/// The dial now runs off the actor loop, so a failure can no longer be the
/// IPC call's own `Err`: the actor keeps this code instead and `connect_status`
/// hands it to the webview, which owns the wording in the user's language
/// (ADR 0027). Same classification the IPC error channel uses, so nothing is
/// disclosed here that it would not have disclosed anyway.
#[must_use]
pub fn net_error_code(error: &lumepeer_net::NetError) -> &'static str {
    classify_net(error).0
}

/// Maps a transport failure onto the (code, message) pair of §18.
#[must_use]
pub fn classify_net(error: &lumepeer_net::NetError) -> (&'static str, &'static str) {
    use lumepeer_core::CoreError;
    use lumepeer_net::NetError;

    match *error {
        NetError::MalformedTicket | NetError::InvalidTicket => {
            ("BAD_TICKET", "the invite is not valid or has expired")
        }
        NetError::AlreadyConnected => (
            "ALREADY_CONNECTED",
            "you are already connected to this device",
        ),
        NetError::Dial(_) | NetError::Endpoint(_) => {
            ("DIAL_FAILED", "the host could not be reached")
        }
        // This device, not the peer: nothing is wrong with the invite or
        // the far side, so it must not read like a rejection.
        NetError::Offline => (
            "OFFLINE",
            "this device is not reachable from the internet yet — wait for the status to turn ready, then try again",
        ),
        NetError::Framing(CoreError::IncompatibleVersion { .. }) => (
            "INCOMPATIBLE_VERSION",
            "the host speaks an incompatible protocol version",
        ),
        // Transport, not verdict. This is what *this* side observed — a
        // stream that stopped — so saying so leaks nothing about why the
        // far end did anything, and it keeps a flapping link from being
        // reported as a rejection, which sends the user hunting on the
        // wrong machine (ADR 0026).
        NetError::Io(_) => (
            "TRANSPORT_LOST",
            "the connection dropped before the session was set up — check the network and try again",
        ),
        _ => ("REJECTED", "the host refused the connection"),
    }
}
