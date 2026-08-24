//! Two-role connectivity probe: proves whether a real Lumepeer control session
//! can be established between two machines over the path the endpoint is
//! currently allowed to use.
//!
//! It is deliberately the *real* stack — `PeerEndpoint`, a signed
//! [`InviteTicket`], `guest_handshake`/`host_handshake`, a `ConsentGrant` — so
//! a pass here means the transport the desktop app uses works, not that some
//! simplified imitation of it does. What it leaves out is everything above the
//! control channel: no consent UI, no capture, no media.
//!
//! Set `LUMEPEER_RELAY_ONLY=1` (see
//! [`lumepeer_net::endpoint::relay_only_enabled`]) to clear the direct IP
//! transports, so the session can only be carried by the relay — out to the
//! internet and back. That is how this probe answers the WAN question: two
//! machines on one LAN otherwise reach each other without the internet being
//! involved at all, and the test would prove nothing.
//!
//! ```text
//! # on the host machine
//! cargo run -p lumepeer-net --example wan_probe -- host
//! # -> prints `INVITE lumepeer1:...`
//!
//! # on the other machine
//! cargo run -p lumepeer-net --example wan_probe -- guest lumepeer1:...
//! ```
//!
//! The identity is a fresh random key per run: the probe never reads or writes
//! the OS keystore, so it cannot disturb the app's long-term identity (§11.2).

use std::time::Duration;

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_core::protocol::MessageKind;
use lumepeer_net::endpoint::relay_only_enabled;
use lumepeer_net::{InviteTicket, PeerEndpoint};

/// How long to wait for a relay before giving up on the whole probe.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the host waits for a guest to show up.
const ACCEPT_TIMEOUT: Duration = Duration::from_mins(3);
/// How long the guest waits for the host's consent decision.
const GRANT_TIMEOUT: Duration = Duration::from_mins(1);
/// How long both sides stay connected before reporting the path a second time,
/// so an upgrade away from the relay becomes visible if one happens.
const SETTLE: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let role = args.next().unwrap_or_default();
    let code = args.next();

    let outcome = match role.as_str() {
        "host" => host().await,
        "guest" => match code {
            Some(code) => guest(&code).await,
            None => Err("usage: wan_probe guest <invite-code>".to_owned()),
        },
        _ => Err("usage: wan_probe host | wan_probe guest <invite-code>".to_owned()),
    };

    match outcome {
        Ok(()) => println!("RESULT ok"),
        Err(reason) => {
            println!("RESULT failed: {reason}");
            std::process::exit(1);
        }
    }
}

/// Binds an endpoint with a throwaway identity and waits for a relay.
async fn bind() -> Result<(PeerEndpoint, SigningKey), String> {
    println!(
        "MODE {}",
        if relay_only_enabled() {
            "relay-only (LUMEPEER_RELAY_ONLY is set: no direct IP transports)"
        } else {
            "direct+relay"
        }
    );
    let secret = iroh::SecretKey::generate();
    let identity = SigningKey::from_bytes(&secret.to_bytes());
    let endpoint = PeerEndpoint::bind(secret, None)
        .await
        .map_err(|e| format!("bind: {e}"))?;
    println!("NODE {}", endpoint.node_id());

    if tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online())
        .await
        .is_err()
    {
        return Err(format!(
            "no relay reached within {}s: this endpoint is not dialable from outside the LAN",
            ONLINE_TIMEOUT.as_secs()
        ));
    }
    println!("ONLINE addrs={:?}", endpoint.addr().addrs);
    Ok((endpoint, identity))
}

/// Describes how the traffic of `connection` is actually reaching the peer:
/// every open path, which kind it is, and which one is carrying the data.
fn report_path(connection: &iroh::endpoint::Connection, side: &str) {
    let paths = connection.paths();
    if paths.is_empty() {
        println!("{side} paths=none");
        return;
    }
    for path in paths.iter() {
        println!(
            "{side} path kind={} selected={} rtt={:?} remote={:?}",
            if path.is_relay() { "relay" } else { "direct" },
            path.is_selected(),
            path.rtt(),
            path.remote_addr()
        );
    }
}

async fn host() -> Result<(), String> {
    let (endpoint, identity) = bind().await?;
    let addr = endpoint.addr();
    if addr.addrs.is_empty() {
        return Err(
            "the endpoint has no address at all; any invite would be undialable".to_owned(),
        );
    }
    let ticket = InviteTicket::issue(&identity, &addr, Role::ViewOnly, unix_now())
        .map_err(|e| e.to_string())?;
    println!("TICKET addrs={:?}", addr.addrs);
    println!("INVITE {}", ticket.to_code().map_err(|e| e.to_string())?);

    let accepted = tokio::time::timeout(ACCEPT_TIMEOUT, endpoint.accept())
        .await
        .map_err(|_| format!("no guest connected within {}s", ACCEPT_TIMEOUT.as_secs()))?
        .ok_or_else(|| "the endpoint closed while accepting".to_owned())?
        .map_err(|e| format!("accept: {e}"))?;
    report_path(&accepted, "HOST");

    let (mut control, hello) = lumepeer_net::host_handshake(accepted)
        .await
        .map_err(|e| format!("host handshake: {e}"))?;
    println!(
        "HANDSHAKE peer={} role={:?}",
        control.peer(),
        hello.role_request
    );

    // The same three things the real host checks before it would even offer
    // the decision to its user: signature, TTL, and that the proof is the
    // ticket this run issued (§7).
    let proof: InviteTicket = postcard::from_bytes(&hello.invite_proof)
        .map_err(|_| "malformed invite proof".to_owned())?;
    proof
        .verify(&identity.verifying_key(), unix_now())
        .map_err(|e| format!("invite verify: {e}"))?;
    if proof.invite_id != ticket.invite_id {
        return Err("the guest presented a different invite".to_owned());
    }
    println!("TICKET verified");

    control
        .send(MessageKind::ConsentGrant(Role::ViewOnly))
        .await
        .map_err(|e| format!("grant: {e}"))?;
    println!("GRANT sent");

    tokio::time::sleep(SETTLE).await;
    report_path(control.connection(), "HOST-settled");
    Ok(())
}

async fn guest(code: &str) -> Result<(), String> {
    let (endpoint, _identity) = bind().await?;
    let ticket = InviteTicket::from_code(code).map_err(|e| format!("ticket: {e}"))?;
    let addr = ticket
        .endpoint_addr()
        .map_err(|e| format!("ticket address: {e}"))?;
    println!("DIALING id={} addrs={:?}", addr.id, addr.addrs);

    let connection = endpoint
        .connect_control(addr)
        .await
        .map_err(|e| format!("dial: {e}"))?;
    report_path(&connection, "GUEST");

    let proof = postcard::to_allocvec(&ticket).map_err(|_| "encode proof".to_owned())?;
    let mut control =
        lumepeer_net::guest_handshake(connection, ticket.allowed_request, proof, Vec::new())
            .await
            .map_err(|e| format!("guest handshake: {e}"))?;
    println!("HANDSHAKE host={}", control.peer());

    let envelope = tokio::time::timeout(GRANT_TIMEOUT, control.recv())
        .await
        .map_err(|_| format!("no decision within {}s", GRANT_TIMEOUT.as_secs()))?
        .map_err(|e| format!("recv: {e}"))?;
    match envelope.kind {
        MessageKind::ConsentGrant(role) => println!("GRANT role={role:?}"),
        other => return Err(format!("unexpected first message: {other:?}")),
    }

    tokio::time::sleep(SETTLE).await;
    report_path(control.connection(), "GUEST-settled");
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
