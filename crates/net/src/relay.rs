//! Which of the public relays this node keeps as its fallback fleet (ADR
//! 0097).
//!
//! The relay is not the path — direct paths are preferred and a session that
//! gets one stops using it (ADR 0026). But it is the path a session *starts*
//! on, and it is where the two sides exchange what they need to hole-punch a
//! direct one, so a far relay is not merely a slower fallback: it is a slower
//! and less reliable way to reach the fast path.
//!
//! iroh ranks the fleet by measured latency, and the full ranking it produces
//! is right. The trouble is what happens between full rankings. A net report
//! that re-measured only one relay hands the home slot to *that* relay, and
//! the hysteresis meant to stop a switch is skipped exactly then, because it
//! is guarded on the previous home relay having a latency in the same report —
//! which, not having been probed, it does not. Measured on a host in Europe:
//!
//! ```text
//! relay_latency ipv4: {aps1-1: 224ms, euc1-1: 81ms, use1-1: 157ms}
//! preferred_relay: euc1-1          <- the full report, and it is correct
//!
//! relay_latency ipv4: {aps1-1: 207ms}
//! preferred_relay: aps1-1          <- the next partial one, and it sticks
//! ```
//!
//! Every five minutes a full report moved that host back to Frankfurt and the
//! next partial one moved it to Singapore again, which is what its log had
//! been doing for days.
//!
//! So this does not try to out-rank iroh. It removes the relays that could
//! only ever be the wrong answer for this machine, and lets iroh rank what is
//! left. Two are kept rather than one, because a fleet of one is a single
//! point of failure and the point here is reliability; the cost of the drift
//! is then bounded at "the second-nearest relay" instead of "the other side of
//! the world".

use std::time::Duration;

use iroh::RelayUrl;
use iroh::defaults::prod;

/// How many of the fleet to keep.
///
/// Two, not one: iroh's own choice between them is a real fallback if one
/// relay is down or unreachable from a particular network, and both being
/// near is what this is for. Not three, because on a four-relay fleet that
/// only drops the single worst entry and leaves the far ones this exists to
/// remove.
const KEEP: usize = 2;

/// How long one relay has to answer before it is treated as unreachable.
///
/// Spent once, at bind, with every relay probed at the same time, so this is
/// the whole cost and not a cost per relay. Generous enough that a relay on
/// another continent still answers — the far ones have to be *measured*, not
/// timed out, or this would keep whichever two happened to be quick today.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Port the relays serve on. Their `RelayUrl`s are `https`, with no port.
const RELAY_PORT: u16 = 443;

/// The public fleet, exactly as `RelayMode::Default` builds it: four
/// hostnames, each turned into a bare `RelayConfig` with nothing else on it,
/// which is why keeping a subset loses no configuration at all.
const FLEET: [&str; 4] = [
    prod::EU_RELAY_HOSTNAME,
    prod::NA_EAST_RELAY_HOSTNAME,
    prod::NA_WEST_RELAY_HOSTNAME,
    prod::AP_RELAY_HOSTNAME,
];

/// Keeping fewer than two would make the fleet a single point of failure, and
/// keeping every relay would narrow nothing — the two ends this choice has to
/// stay between, checked where they are written rather than in a test that
/// could be deleted with them.
const _: () = assert!(
    KEEP >= 2 && KEEP < FLEET.len(),
    "the kept relays must leave a fallback and still narrow the fleet"
);

/// Round-trip to one relay's TLS port, or `None` if it did not answer in time.
///
/// A TCP connect rather than a protocol exchange: this only has to *rank* the
/// fleet, and the ordering a connect produces is the ordering iroh's own QAD
/// probes produce — on the host this was measured against, both put Frankfurt
/// first, Virginia second and Singapore last. Nothing is sent and the socket
/// is dropped immediately.
async fn round_trip(hostname: &str) -> Option<Duration> {
    // `RelayUrl` hostnames are fully qualified and end in a dot, which is
    // correct in DNS and not what a socket address parser expects.
    let host = hostname.trim_end_matches('.');
    let started = std::time::Instant::now();
    let connect = tokio::net::TcpStream::connect((host, RELAY_PORT));
    match tokio::time::timeout(PROBE_TIMEOUT, connect).await {
        Ok(Ok(_stream)) => Some(started.elapsed()),
        Ok(Err(error)) => {
            tracing::debug!(%hostname, %error, "a relay did not accept a connection");
            None
        }
        Err(_) => {
            tracing::debug!(%hostname, "a relay did not answer in time");
            None
        }
    }
}

/// The nearest [`KEEP`] relays of the public fleet, or `None` to leave the
/// fleet alone (ADR 0097).
///
/// `None` whenever the measurement cannot support a choice — fewer than
/// [`KEEP`] relays answered, or none did, which is what an offline machine and
/// a network that blocks TLS to all of them both look like. Narrowing a fleet
/// on no evidence would turn a temporary failure into a permanently smaller
/// set of ways to be reached, and the default fleet is the honest answer to
/// "this node has nothing to go on" (§18).
pub async fn nearest() -> Option<Vec<RelayUrl>> {
    // All four at once, so the whole measurement costs one `PROBE_TIMEOUT` and
    // not four. `join!` rather than a task per relay: nothing here outlives
    // this call, and the bind that awaits it is already on the runtime.
    let [eu, na_east, na_west, ap] = FLEET;
    let measured = tokio::join!(
        round_trip(eu),
        round_trip(na_east),
        round_trip(na_west),
        round_trip(ap),
    );
    let mut answered: Vec<(&str, Duration)> = [
        (eu, measured.0),
        (na_east, measured.1),
        (na_west, measured.2),
        (ap, measured.3),
    ]
    .into_iter()
    .filter_map(|(hostname, rtt)| rtt.map(|rtt| (hostname, rtt)))
    .collect();
    answered.sort_by_key(|(_, rtt)| *rtt);
    if answered.len() < KEEP {
        tracing::info!(
            answered = answered.len(),
            "not enough relays answered to choose between them; keeping the whole fleet"
        );
        return None;
    }
    answered.truncate(KEEP);
    let kept: Vec<RelayUrl> = answered
        .iter()
        .filter_map(|(hostname, _)| format!("https://{hostname}").parse().ok())
        .collect();
    if kept.len() < KEEP {
        return None;
    }
    tracing::info!(
        relays = ?answered
            .iter()
            .map(|(hostname, rtt)| format!("{hostname} {}ms", rtt.as_millis()))
            .collect::<Vec<_>>(),
        "keeping the nearest relays of the public fleet (ADR 0097)"
    );
    Some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fleet this narrows is the one iroh would otherwise use in full. If
    /// a release adds a relay, this array has to gain it too — the constants
    /// are what makes that a compile-time question rather than a silent
    /// omission, and this is what notices the count changing.
    #[test]
    fn the_fleet_is_the_whole_public_one() {
        assert_eq!(FLEET.len(), 4);
        assert!(FLEET.contains(&prod::EU_RELAY_HOSTNAME));
        assert!(FLEET.contains(&prod::NA_EAST_RELAY_HOSTNAME));
        assert!(FLEET.contains(&prod::NA_WEST_RELAY_HOSTNAME));
        assert!(FLEET.contains(&prod::AP_RELAY_HOSTNAME));
    }

    /// Every hostname has to survive the trip to a `RelayUrl`, or the pick
    /// would silently hand back fewer relays than it chose.
    #[test]
    fn every_fleet_hostname_parses_as_a_relay_url() {
        for hostname in FLEET {
            let url: RelayUrl = format!("https://{hostname}")
                .parse()
                .unwrap_or_else(|error| panic!("{hostname} must parse as a relay url: {error}"));
            assert!(url.to_string().contains(hostname.trim_end_matches('.')));
        }
    }
}
