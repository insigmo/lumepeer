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
//! Both `host` and `guest` take an optional count, so one invite can carry a
//! series of punches and the share that landed is one number at the end
//! (gap-tasks/22 definition of done: ten attempts).
//!
//! ```text
//! # on each machine, before anything else: what is this network?
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- nat
//!
//! # on the host machine (serve up to 10 guests on one invite)
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- host 10
//! # -> prints `INVITE lumepeer1:...`
//!
//! # on the other machine (10 independent punches)
//! cargo run -p lumepeer-net --example obfuscated_wan_probe -- guest lumepeer1:... 10
//! ```

use std::collections::BTreeSet;
use std::net::{SocketAddr, ToSocketAddrs as _, UdpSocket};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_net::obfuscated_endpoint::{GuestObfuscatedEndpoint, STUN_SERVERS, bind_host};
use lumepeer_net::rendezvous::Rendezvous;
use lumepeer_net::stun;
use lumepeer_net::ticket::INVITE_ID_BYTES;
use lumepeer_net::{InviteTicket, PeerConnection};
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

    let outcome = match role.as_str() {
        "host" => match count(args.next()) {
            Ok(guests) => host(guests).await,
            Err(reason) => Err(reason),
        },
        "guest" => match (args.next(), count(args.next())) {
            (Some(code), Ok(attempts)) => guest(&code, attempts).await,
            (None, _) => {
                Err("usage: obfuscated_wan_probe guest <invite-code> [attempts]".to_owned())
            }
            (_, Err(reason)) => Err(reason),
        },
        "hold" => match (args.next(), count(args.next())) {
            (Some(code), Ok(minutes)) => hold(&code, minutes).await,
            (None, _) => Err("usage: obfuscated_wan_probe hold <invite-code> [minutes]".to_owned()),
            (_, Err(reason)) => Err(reason),
        },
        "nat" => nat().await,
        _ => Err(
            "usage: obfuscated_wan_probe nat | obfuscated_wan_probe host [guests] | \
                  obfuscated_wan_probe guest <invite-code> [attempts] | \
                  obfuscated_wan_probe hold <invite-code> [minutes]"
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

/// The count argument of `host` and `guest`: one when absent, and never zero —
/// a series of nothing would report a share of nothing.
fn count(arg: Option<String>) -> Result<u32, String> {
    match arg {
        None => Ok(1),
        Some(arg) => match arg.parse::<u32>() {
            Ok(0) | Err(_) => Err(format!("expected a positive count, got {arg:?}")),
            Ok(count) => Ok(count),
        },
    }
}

async fn host(guests: u32) -> Result<(), String> {
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

    // The real product path since ADR 0113: the host publishes where it is
    // and punches back towards every guest that knocks.
    let rendezvous = Rendezvous::start().map_err(|e| e.to_string())?;
    let bound = bind_host(&invite_id, &identity, Some(rendezvous))
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

    // Under the id the endpoint was bound with: `issue` would mint another,
    // and a guest holding it would seal with keys this endpoint cannot open.
    let ticket = InviteTicket::issue_with_id(
        &identity,
        &iroh_stub.addr(),
        Role::ViewOnly,
        unix_now(),
        invite_id,
        Some(public_addr),
        Some(bound.cert_fingerprint),
    )
    .map_err(|e| e.to_string())?;
    println!("INVITE {}", ticket.to_code().map_err(|e| e.to_string())?);

    // Each guest is waited for on its own clock: a guest whose punch never
    // lands never arrives, so the series ends at the first quiet
    // `ACCEPT_TIMEOUT` rather than at the count, and the count served is what
    // is reported. The clock only runs while nobody is being served: a `hold`
    // guest that drops dials again, however long its session was.
    println!(
        "WAITING for {guests} guest(s), each within {}s",
        ACCEPT_TIMEOUT.as_secs()
    );
    let mut serving = tokio::task::JoinSet::new();
    let mut served = 0u32;
    while served < guests {
        while let Some(outcome) = serving.try_join_next() {
            if let Ok(Err(reason)) = outcome {
                println!("SERVE failed: {reason}");
            }
        }
        let accepting = async {
            if serving.is_empty() {
                tokio::time::timeout(ACCEPT_TIMEOUT, bound.accept()).await
            } else {
                Ok(bound.accept().await)
            }
        };
        let accepted = match accepting.await {
            Ok(Some(accepted)) => accepted,
            Ok(None) => return Err("the endpoint closed while accepting".to_owned()),
            Err(_) => break,
        };
        // A refused handshake is one guest's failure, not the series'.
        let connection = match accepted {
            Ok(connection) => connection,
            Err(e) => {
                println!("REFUSED {e}");
                continue;
            }
        };
        served += 1;
        println!(
            "ACCEPTED {served}/{guests} peer={} alpn={:?}",
            connection.peer(),
            String::from_utf8_lossy(connection.alpn())
        );
        serving.spawn(serve(connection));
    }
    while let Some(outcome) = serving.join_next().await {
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => println!("SERVE failed: {reason}"),
            Err(e) => println!("SERVE failed: {e}"),
        }
    }
    println!("SERVED {served}/{guests}");
    if served == 0 {
        return Err(format!(
            "no guest connected within {}s",
            ACCEPT_TIMEOUT.as_secs()
        ));
    }
    Ok(())
}

/// Echoes whatever one guest sends — a `guest` ping or a `hold` session's
/// ticks — and waits for it to hang up.
async fn serve(connection: PeerConnection) -> Result<(), String> {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| format!("accept_bi: {e}"))?;
    let mut buf = [0u8; 1024];
    let mut echoed = 0usize;
    while let Some(n) = recv
        .read(&mut buf)
        .await
        .map_err(|e| format!("read after {echoed} bytes: {e}"))?
    {
        send.write_all(&buf[..n])
            .await
            .map_err(|e| format!("write after {echoed} bytes: {e}"))?;
        echoed += n;
    }
    println!("ECHOED {echoed} bytes");
    send.finish().map_err(|e| format!("finish: {e}"))?;
    connection.closed().await;
    Ok(())
}

async fn guest(code: &str, attempts: u32) -> Result<(), String> {
    let ticket = InviteTicket::from_code(code).map_err(|e| format!("ticket: {e}"))?;
    let (Some(target), Some(fingerprint)) = (ticket.obfuscated_addr, ticket.host_cert_fingerprint)
    else {
        return Err(
            "this invite carries no obfuscated-transport address; the host's STUN discovery \
             must have failed"
                .to_owned(),
        );
    };
    let host = ticket
        .endpoint_addr()
        .map_err(|e| format!("ticket address: {e}"))?
        .id;
    println!("DIALING target={target}, {attempts} punch(es)");
    // One DHT client for every punch, as the app has one per process
    // (ADR 0113): each punch still knocks from its own fresh socket.
    let rendezvous = Rendezvous::start().map_err(|e| e.to_string())?;

    // The guest's own throwaway identity: its certificate names it to the
    // host exactly as the iroh path's endpoint key would (ADR 0080).
    let identity = SigningKey::from_bytes(&iroh::SecretKey::generate().to_bytes());
    let mut landed = 0u32;
    for attempt in 1..=attempts {
        let started = Instant::now();
        match punch(
            &ticket.invite_id,
            &identity,
            host,
            target,
            fingerprint,
            &rendezvous,
        )
        .await
        {
            Ok(()) => {
                landed += 1;
                println!(
                    "PUNCH {attempt}/{attempts} landed in {}ms",
                    started.elapsed().as_millis()
                );
            }
            Err(reason) => println!(
                "PUNCH {attempt}/{attempts} failed after {}ms: {reason}",
                started.elapsed().as_millis()
            ),
        }
    }
    println!("PUNCH landed={landed}/{attempts}");
    if landed == 0 {
        return Err("no punch landed".to_owned());
    }
    Ok(())
}

/// One punch of a series, from a socket of its own.
///
/// A fresh socket is a fresh mapping on this side's NAT: reusing one would
/// leave the first attempt's mapping open towards the host, and every later
/// attempt would count a punch the first one had already made.
async fn punch(
    invite_id: &[u8; INVITE_ID_BYTES],
    identity: &SigningKey,
    host: lumepeer_core::NodeId,
    target: SocketAddr,
    fingerprint: [u8; 32],
    rendezvous: &Rendezvous,
) -> Result<(), String> {
    let endpoint = GuestObfuscatedEndpoint::bind(
        invite_id,
        identity,
        host,
        target,
        fingerprint,
        Some(rendezvous.clone()),
    )
    .map_err(|e| format!("bind: {e}"))?;
    let outcome = ping(&endpoint).await;
    endpoint.close().await;
    outcome
}

/// Dials the host and exchanges one ping. Success is the reply, not the dial:
/// a connection that carries nothing proves less than the session needs.
async fn ping(endpoint: &GuestObfuscatedEndpoint) -> Result<(), String> {
    // The dial alone: what a person waits for. `landed` also counts the ping
    // and the endpoint draining on close, which nobody waits for.
    let dialing = Instant::now();
    let connection = endpoint
        .connect_control()
        .await
        .map_err(|e| format!("dial: {e}"))?;
    println!(
        "CONNECTED in {}ms peer={}",
        dialing.elapsed().as_millis(),
        connection.peer()
    );

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

/// Spacing of a `hold` session's ticks.
const HOLD_TICK: Duration = Duration::from_millis(100);
/// A `hold` echo later than this is reported as a gap: well above a WAN round
/// trip, well below anything a person would not notice.
const HOLD_GAP: Duration = Duration::from_secs(1);

/// Holds one session for `minutes`, sending a tick every [`HOLD_TICK`] and
/// timing its echo, and reports every gap and every drop (ADR 0134).
///
/// A drop is dialed again at once, as the app's resume would, so one run
/// counts how often a real path breaks rather than stopping at the first.
async fn hold(code: &str, minutes: u32) -> Result<(), String> {
    let ticket = InviteTicket::from_code(code).map_err(|e| format!("ticket: {e}"))?;
    let (Some(target), Some(fingerprint)) = (ticket.obfuscated_addr, ticket.host_cert_fingerprint)
    else {
        return Err("this invite carries no obfuscated-transport address".to_owned());
    };
    let host = ticket
        .endpoint_addr()
        .map_err(|e| format!("ticket address: {e}"))?
        .id;
    let rendezvous = Rendezvous::start().map_err(|e| e.to_string())?;
    let identity = SigningKey::from_bytes(&iroh::SecretKey::generate().to_bytes());
    let started = Instant::now();
    let deadline = started + Duration::from_secs(u64::from(minutes) * 60);
    let (mut drops, mut gaps) = (0u32, 0u32);
    let mut worst = Duration::ZERO;

    while Instant::now() < deadline {
        let endpoint = GuestObfuscatedEndpoint::bind(
            &ticket.invite_id,
            &identity,
            host,
            target,
            fingerprint,
            Some(rendezvous.clone()),
        )
        .map_err(|e| format!("bind: {e}"))?;
        let dialing = Instant::now();
        match endpoint.connect_control().await {
            Ok(connection) => {
                println!(
                    "CONNECTED at +{}s in {}ms",
                    started.elapsed().as_secs(),
                    dialing.elapsed().as_millis()
                );
                let held = hold_session(&connection, started, deadline).await?;
                if held.dropped {
                    drops += 1;
                }
                gaps += held.gaps;
                worst = worst.max(held.worst);
                connection.close(0u32.into(), b"done");
            }
            Err(e) => {
                println!("DIAL failed after {}ms: {e}", dialing.elapsed().as_millis());
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
        endpoint.close().await;
    }
    println!(
        "HOLD {minutes} min: drops={drops} gaps={gaps} worst={}ms",
        worst.as_millis()
    );
    Ok(())
}

/// What one connection of a `hold` run went through.
struct Held {
    dropped: bool,
    gaps: u32,
    worst: Duration,
}

/// Ticks on `connection` until `deadline` or until it drops.
async fn hold_session(
    connection: &PeerConnection,
    started: Instant,
    deadline: Instant,
) -> Result<Held, String> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| format!("open_bi: {e}"))?;
    // The echoes are read on a task of their own, so a stalled path never
    // stalls the ticks and a read is never cut off halfway through one.
    let last_echo = std::sync::Arc::new(std::sync::Mutex::new(Instant::now()));
    let reader = tokio::spawn({
        let last_echo = std::sync::Arc::clone(&last_echo);
        async move {
            let (mut gaps, mut worst) = (0u32, Duration::ZERO);
            let mut echo = [0u8; 8];
            while recv.read_exact(&mut echo).await.is_ok() {
                let mut last = last_echo
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let gap = last.elapsed();
                if gap > HOLD_GAP {
                    gaps += 1;
                    worst = worst.max(gap);
                    println!(
                        "GAP {}ms ending at +{}s",
                        gap.as_millis(),
                        started.elapsed().as_secs()
                    );
                }
                *last = Instant::now();
            }
            (gaps, worst)
        }
    });
    let mut tick = tokio::time::interval(HOLD_TICK);
    let mut seq = 0u64;
    let ended = loop {
        if Instant::now() >= deadline {
            break None;
        }
        tick.tick().await;
        seq += 1;
        if let Err(e) = send.write_all(&seq.to_le_bytes()).await {
            break Some(format!("write: {e}"));
        }
        if let Some(reason) = connection.close_reason() {
            break Some(reason.to_string());
        }
    };
    let since_echo = last_echo
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .elapsed();
    if let Some(reason) = &ended {
        println!(
            "DROPPED at +{}s, {}ms after the last echo: {reason}",
            started.elapsed().as_secs(),
            since_echo.as_millis()
        );
    } else {
        let _ = send.finish();
    }
    connection.close(0u32.into(), b"done");
    let (gaps, worst) = reader.await.unwrap_or((0, Duration::ZERO));
    Ok(Held {
        dropped: ended.is_some(),
        gaps,
        worst: if ended.is_some() {
            worst.max(since_echo)
        } else {
            worst
        },
    })
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
