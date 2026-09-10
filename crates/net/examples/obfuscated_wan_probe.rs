//! Three-role probe for the obfuscated serverless transport (task 17
//! increment 2, ADR 0053; gap-tasks/22) — the `obfuscated_endpoint`/
//! STUN-in-the-ticket counterpart to `wan_probe.rs`, which exercises the
//! existing iroh path.
//!
//! `host`/`guest` prove, on a real NAT pair rather than inside `cargo test`,
//! that a host's STUN-discovered address survives long enough (held open by
//! the keepalive task) for a guest to dial it directly, entirely without iroh
//! or a relay. `nat` answers the question that decides whether that can work
//! at all on a given machine: what kind of NAT it is behind, and how long its
//! UDP mappings live without traffic (gap-tasks/22 task 2).
//!
//! ```text
//! # on each machine, before anything else: what is this network?
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- nat
//!
//! # on the host machine
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- host
//! # -> prints `INVITE lumepeer1:...`
//!
//! # on the other machine
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- guest lumepeer1:...
//! ```

use std::collections::BTreeSet;
use std::net::{SocketAddr, ToSocketAddrs as _, UdpSocket};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_net::InviteTicket;
use lumepeer_net::obfuscated_endpoint::{GuestObfuscatedEndpoint, STUN_SERVERS, bind_host};
use lumepeer_net::stun;
use lumepeer_net::ticket::INVITE_ID_BYTES;
use rand::Rng as _;

/// How long the host waits for a guest to show up.
const ACCEPT_TIMEOUT: Duration = Duration::from_mins(3);

/// Idle periods, in seconds and ascending, that the mapping-lifetime probe
/// measures (gap-tasks/22 task 2 item 2).
///
/// Each gets a socket of its own, all opened at the start, each re-queried
/// once its own period is up — so the re-query is the first packet that socket
/// has sent since its baseline and nothing in between refreshed the mapping
/// being measured. The longest reaches past the 120 s a UDP:443 mapping was
/// already known to hold on `beta`
/// (project-lumepeer-quic-vs-relay-transport), so "still alive" and "expired
/// somewhere below here" can be told apart.
const MAPPING_IDLE_PROBES_SECS: &[u64] = &[30, 60, 120, 180, 240];

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
        "nat" => nat().await,
        _ => Err(
            "usage: obfuscated_wan_probe nat | obfuscated_wan_probe host | \
                  obfuscated_wan_probe guest <invite-code>"
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

    // A throwaway identity, same as `wan_probe.rs`: this probe never touches
    // the OS keystore or the app's real identity (§11.2). The endpoint's
    // certificate is generated from it, so the `NodeId` a guest sees here is
    // this key (ADR 0080).
    let secret = iroh::SecretKey::generate();
    let identity = SigningKey::from_bytes(&secret.to_bytes());

    let bound = bind_host(&invite_id, &identity)
        .await
        .map_err(|e| e.to_string())?;
    let Some(public_addr) = bound.public_addr else {
        return Err(
            "no STUN server answered (or the mapping looked unusable): this host cannot be \
             dialed on the obfuscated transport, only via the existing iroh fallback"
                .to_owned(),
        );
    };
    println!("STUN public_addr={public_addr}");

    // `addr`/`node_addr` still needs *some* iroh address to satisfy
    // `InviteTicket::issue`, even though this probe never dials it — a bare
    // local endpoint gives one.
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
    let connection = tokio::time::timeout(ACCEPT_TIMEOUT, bound.accept())
        .await
        .map_err(|_| format!("no guest connected within {}s", ACCEPT_TIMEOUT.as_secs()))?
        .ok_or_else(|| "the endpoint closed while accepting".to_owned())?
        .map_err(|e| format!("accept: {e}"))?;
    println!(
        "ACCEPTED peer={} alpn={:?}",
        connection.peer(),
        String::from_utf8_lossy(connection.alpn())
    );

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

    // The guest's own throwaway identity: its certificate names it to the
    // host exactly as the iroh path's endpoint key would (ADR 0080).
    let identity = SigningKey::from_bytes(&iroh::SecretKey::generate().to_bytes());
    let endpoint = GuestObfuscatedEndpoint::bind(&ticket.invite_id, &identity, target, fingerprint)
        .map_err(|e| format!("bind: {e}"))?;
    let connection = endpoint
        .connect(lumepeer_net::ALPN_CONTROL)
        .await
        .map_err(|e| format!("dial: {e}"))?;
    println!("CONNECTED peer={}", connection.peer());

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

/// Measures the NAT this machine sits behind (gap-tasks/22 task 2).
///
/// Runs on a blocking thread because every step of it is a blocking STUN query
/// or a wait of minutes, and neither belongs on a runtime worker.
async fn nat() -> Result<(), String> {
    tokio::task::spawn_blocking(measure_nat)
        .await
        .map_err(|e| format!("probe thread: {e}"))?
}

/// The reflectors of [`STUN_SERVERS`] that resolve, at most one entry per
/// resolved address.
///
/// Two names on one address are one destination as far as a NAT is concerned,
/// and querying both would compare a mapping with itself — which always agrees
/// and would read as "cone" on a symmetric NAT.
fn resolved_reflectors() -> Vec<(&'static str, SocketAddr)> {
    let mut seen = BTreeSet::new();
    STUN_SERVERS
        .iter()
        .filter_map(|name| {
            let addr = name.to_socket_addrs().ok()?.next()?;
            seen.insert(addr).then_some((*name, addr))
        })
        .collect()
}

/// Asks every reflector for this socket's reflexive address, then measures how
/// long a mapping survives with nothing sent through it.
///
/// The mapping question is the one that decides whether a punch is even
/// possible: an endpoint-independent ("cone") NAT keeps one mapping whatever
/// the destination, so the address a reflector reports is the address a guest
/// can dial; a symmetric NAT allocates a fresh port per destination, so that
/// address is worth nothing to anybody but the reflector, and no amount of
/// sending fixes it without a channel to tell the other side which port was
/// picked.
fn measure_nat() -> Result<(), String> {
    let reflectors = resolved_reflectors();
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("local addr: {e}"))?;
    println!("NAT local={local}");

    let mut answered: Vec<(&str, SocketAddr)> = Vec::new();
    for (name, addr) in &reflectors {
        match stun::reflexive_addr(&socket, *addr) {
            Ok(reflexive) => {
                println!("NAT reflexive via={name} ({addr}) addr={reflexive}");
                answered.push((name, reflexive));
            }
            Err(e) => println!("NAT unreachable via={name} ({addr}): {e}"),
        }
    }

    let Some((_, first)) = answered.first().copied() else {
        return Err(
            "no reflector answered: this machine cannot learn its own public address, \
                    so it has nothing to advertise on the obfuscated transport"
                .to_owned(),
        );
    };
    if answered.len() < 2 {
        println!(
            "NAT mapping=unknown: only one reflector answered, and telling the two apart \
                  takes two"
        );
    } else if answered.iter().all(|(_, addr)| *addr == first) {
        println!("NAT mapping=endpoint-independent (cone): every reflector saw {first}");
    } else {
        println!(
            "NAT mapping=address-dependent (symmetric): the reflectors disagree, so the \
                  address one of them reports is not the one a guest would reach"
        );
    }
    // Filtering behaviour — whether the mapping admits a packet from a source
    // it has never sent to — is the other half of "is this punchable", and it
    // cannot be answered from here: it takes an unsolicited packet from a
    // machine outside this NAT. `crate::stun` sends a plain Binding request
    // with no RFC 5780 CHANGE-REQUEST, so no reflector here will answer from a
    // second address either.
    println!("NAT filtering=unmeasured: it takes a second machine outside this NAT");

    let (_, probe_server) = *reflectors
        .first()
        .ok_or_else(|| "no reflector resolved".to_owned())?;
    let mut probes = Vec::with_capacity(MAPPING_IDLE_PROBES_SECS.len());
    for idle in MAPPING_IDLE_PROBES_SECS {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
        let baseline = stun::reflexive_addr(&socket, probe_server)
            .map_err(|e| format!("baseline query for the {idle}s probe: {e}"))?;
        probes.push((*idle, socket, baseline));
    }

    println!(
        "MAPPING measuring {} idle periods, up to {}s",
        probes.len(),
        MAPPING_IDLE_PROBES_SECS.last().copied().unwrap_or(0)
    );
    let opened = Instant::now();
    for (idle, socket, baseline) in &probes {
        if let Some(remaining) = Duration::from_secs(*idle).checked_sub(opened.elapsed()) {
            std::thread::sleep(remaining);
        }
        match stun::reflexive_addr(socket, probe_server) {
            Ok(after) if after == *baseline => println!("MAPPING idle={idle}s addr={after} alive"),
            Ok(after) => println!("MAPPING idle={idle}s expired: was {baseline}, now {after}"),
            Err(e) => println!("MAPPING idle={idle}s unknown: {e}"),
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
