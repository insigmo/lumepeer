//! Two-role connectivity probe for the obfuscated serverless transport (task
//! 17 increment 2, ADR 0053) — the `obfuscated_endpoint`/STUN-in-the-ticket
//! counterpart to `wan_probe.rs`, which exercises the existing iroh path.
//!
//! Proves, on a real NAT pair rather than inside `cargo test`, that a host's
//! STUN-discovered address survives long enough (held open by the keepalive
//! task) for a guest to dial it directly, entirely without iroh or a relay.
//!
//! ```text
//! # on the host machine
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- host
//! # -> prints `INVITE lumepeer1:...`
//!
//! # on the other machine
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- guest lumepeer1:...
//! ```

use std::time::Duration;

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_net::InviteTicket;
use lumepeer_net::obfuscated_endpoint::{bind_host, connect_guest};
use lumepeer_net::ticket::INVITE_ID_BYTES;
use rand::Rng as _;

/// How long the host waits for a guest to show up.
const ACCEPT_TIMEOUT: Duration = Duration::from_mins(3);

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
            None => Err("usage: obfuscated_wan_probe guest <invite-code>".to_owned()),
        },
        _ => Err(
            "usage: obfuscated_wan_probe host | obfuscated_wan_probe guest <invite-code>"
                .to_owned(),
        ),
    };

    match outcome {
        Ok(()) => println!("RESULT ok"),
        Err(reason) => {
            println!("RESULT failed: {reason}");
            std::process::exit(1);
        }
    }
}

async fn host() -> Result<(), String> {
    // The invite id has to exist before the endpoint binds: it is the key
    // material for every datagram the obfuscated socket seals, and the
    // ticket signs it alongside the address the endpoint discovers.
    let mut invite_id = [0u8; INVITE_ID_BYTES];
    rand::rng().fill_bytes(&mut invite_id);

    let bound = bind_host(&invite_id).await.map_err(|e| e.to_string())?;
    let Some(public_addr) = bound.public_addr else {
        return Err(
            "no STUN server answered (or the mapping looked unusable): this host cannot be \
             dialed on the obfuscated transport, only via the existing iroh fallback"
                .to_owned(),
        );
    };
    println!("STUN public_addr={public_addr}");

    // A throwaway identity, same as `wan_probe.rs`: this probe never touches
    // the OS keystore or the app's real identity (§11.2). `addr`/`node_addr`
    // still needs *some* iroh address to satisfy `InviteTicket::issue`, even
    // though this probe never dials it — a bare local endpoint gives one.
    let secret = iroh::SecretKey::generate();
    let identity = SigningKey::from_bytes(&secret.to_bytes());
    let iroh_stub = lumepeer_net::PeerEndpoint::bind_local(secret)
        .await
        .map_err(|e| format!("stub iroh bind: {e}"))?;

    let ticket = InviteTicket::issue(
        &identity,
        &iroh_stub.addr(),
        Role::ViewOnly,
        unix_now(),
        Some(public_addr),
        Some(bound.cert_fingerprint),
    )
    .map_err(|e| e.to_string())?;
    println!("INVITE {}", ticket.to_code().map_err(|e| e.to_string())?);

    println!("WAITING for a guest (up to {}s)", ACCEPT_TIMEOUT.as_secs());
    let incoming = tokio::time::timeout(ACCEPT_TIMEOUT, bound.endpoint.accept())
        .await
        .map_err(|_| format!("no guest connected within {}s", ACCEPT_TIMEOUT.as_secs()))?
        .ok_or_else(|| "the endpoint closed while accepting".to_owned())?;
    let connection = incoming.await.map_err(|e| format!("accept: {e}"))?;
    println!("ACCEPTED remote={:?}", remote_address(&connection));

    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| format!("accept_bi: {e}"))?;
    let request = recv
        .read_to_end(1024)
        .await
        .map_err(|e| format!("read: {e}"))?;
    println!("RECEIVED {:?}", String::from_utf8_lossy(&request));
    send.write_all(b"pong from host")
        .await
        .map_err(|e| format!("write: {e}"))?;
    send.finish().map_err(|e| format!("finish: {e}"))?;
    connection.closed().await;
    Ok(())
}

async fn guest(code: &str) -> Result<(), String> {
    let ticket = InviteTicket::from_code(code).map_err(|e| format!("ticket: {e}"))?;
    let (Some(target), Some(fingerprint)) = (ticket.obfuscated_addr, ticket.host_cert_fingerprint)
    else {
        return Err(
            "this invite carries no obfuscated-transport address; the host's STUN discovery \
             must have failed"
                .to_owned(),
        );
    };
    println!("DIALING target={target}");

    let connection = connect_guest(&ticket.invite_id, target, fingerprint)
        .await
        .map_err(|e| format!("dial: {e}"))?;
    println!("CONNECTED remote={:?}", remote_address(&connection));

    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| format!("open_bi: {e}"))?;
    send.write_all(b"ping from guest")
        .await
        .map_err(|e| format!("write: {e}"))?;
    send.finish().map_err(|e| format!("finish: {e}"))?;
    let reply = recv
        .read_to_end(1024)
        .await
        .map_err(|e| format!("read: {e}"))?;
    println!("REPLY {:?}", String::from_utf8_lossy(&reply));
    connection.close(0u32.into(), b"done");
    Ok(())
}

/// The peer address of `connection`'s one path, for a status line.
fn remote_address(connection: &noq::Connection) -> Option<std::net::SocketAddr> {
    connection
        .path(noq::PathId::ZERO)
        .and_then(|path| path.remote_address().ok())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
