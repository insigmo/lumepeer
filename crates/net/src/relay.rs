//! Which of the public relays this node keeps as its fallback fleet (ADR
//! 0097), kept narrow for as long as that is safe and widened the moment it
//! is not (ADR 0098).
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
//! So this does not try to out-rank iroh. **It takes the far relays out of the
//! map**, and a relay that is not in the map cannot be drifted onto. ADR 0097
//! left two in, so that the fleet was never a single point of failure; the
//! cost was that the drift still had somewhere to go, and for a machine in
//! Europe that somewhere was Virginia. ADR 0098 keeps *one* — the nearest —
//! and holds the rest in reserve off the map:
//!
//! * the endpoint proves the narrow fleet works ([`RELAY_GRACE`]) and keeps
//!   proving it ([`HEALTH_INTERVAL`]);
//! * the moment no relay of the map is connected, the next-nearest is put back
//!   ([`Fleet::widen`]), and this run never narrows again;
//! * every [`MEASURE_INTERVAL`] the fleet is measured afresh, so a laptop that
//!   moved country moves relay without being restarted.
//!
//! The measurement is also written to disk and read back at the next bind, so
//! a machine that starts before its network is up gets the relays it had last
//! time rather than the whole global fleet — which is the one case where "no
//! evidence" used to mean "Singapore is as good as Frankfurt".

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::defaults::prod;
use iroh::{Endpoint, RelayConfig, RelayUrl, Watcher as _};

/// How long one relay has to answer before it is treated as unreachable.
///
/// Spent once, at bind, with every relay probed at the same time, so this is
/// the whole cost and not a cost per relay. Generous enough that a relay on
/// another continent still answers — the far ones have to be *measured*, not
/// timed out, or this would keep whichever one happened to be quick today.
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

/// How long a measurement on disk is still taken as this machine's answer.
///
/// The same period as [`MEASURE_INTERVAL`], and for the same reason: a running
/// app re-measures on that cadence, so a file older than one cadence is a file
/// the running app would already have replaced.
const CACHE_TTL: Duration = Duration::from_mins(30);

/// How often the running app measures the fleet again (ADR 0098).
///
/// Half an hour, because what it detects is a machine that moved — a laptop
/// carried to another country, a VPN switched on, an uplink that failed over.
/// None of those happen on a scale where a shorter period would find them
/// sooner in any way the user could notice, and each measurement costs four
/// TCP connects.
const MEASURE_INTERVAL: Duration = Duration::from_mins(30);

/// How long the narrowed fleet has to reach its one relay before the reserve
/// is put back (ADR 0098).
///
/// A TCP connect to port 443 is what [`round_trip`] measures, and a network
/// that answers one is not necessarily a network the relay protocol survives:
/// a transparent proxy accepts the connection and then fails the WebSocket
/// upgrade. That is exactly the case a fleet of one would strand, so the
/// narrowing is not trusted until the endpoint says it actually reached a
/// relay.
const RELAY_GRACE: Duration = Duration::from_secs(10);

/// How often the fleet's health is re-checked once it is up.
///
/// Much shorter than [`MEASURE_INTERVAL`]: measuring answers "is there
/// somewhere better", which changes on the scale of a journey, while this
/// answers "can this node be reached at all", which changes on the scale of a
/// relay outage and must not wait half an hour.
const HEALTH_INTERVAL: Duration = Duration::from_secs(30);

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

/// The whole public fleet as URLs, in the order [`FLEET`] names it.
///
/// What the endpoint's map holds when nothing has been measured — which is
/// `RelayMode::Default`, stated here so a later narrowing knows what it is
/// narrowing *from*.
fn fleet_urls() -> Vec<RelayUrl> {
    FLEET.iter().filter_map(|host| url_of(host)).collect()
}

/// One fleet hostname as a `RelayUrl`.
fn url_of(hostname: &str) -> Option<RelayUrl> {
    format!("https://{hostname}").parse().ok()
}

/// The public fleet, ranked nearest first, or `None` when the measurement
/// cannot support a ranking at all.
///
/// `None` whenever nothing answered, which is what an offline machine and a
/// network that blocks TLS to every relay both look like. Ranking a fleet on
/// no evidence would turn a temporary failure into a permanently wrong choice,
/// and the default fleet is the honest answer to "this node has nothing to go
/// on" (§18).
async fn measure() -> Option<Vec<(&'static str, Duration)>> {
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
    let mut answered: Vec<(&'static str, Duration)> = [
        (eu, measured.0),
        (na_east, measured.1),
        (na_west, measured.2),
        (ap, measured.3),
    ]
    .into_iter()
    .filter_map(|(hostname, rtt)| rtt.map(|rtt| (hostname, rtt)))
    .collect();
    answered.sort_by_key(|(_, rtt)| *rtt);
    if answered.is_empty() {
        tracing::info!("no relay answered; the fleet is left as it is");
        return None;
    }
    tracing::info!(
        relays = ?answered
            .iter()
            .map(|(hostname, rtt)| format!("{hostname} {}ms", rtt.as_millis()))
            .collect::<Vec<_>>(),
        "measured the public relay fleet (ADR 0098)"
    );
    Some(answered)
}

/// A measurement as it is written to disk, nearest first.
///
/// Whole URLs rather than the bare hostnames of [`FLEET`], so a file written
/// by a build whose fleet has since changed still parses: an entry this build
/// does not recognise is simply a relay it will measure and rank like any
/// other.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Cached {
    /// Unix seconds the measurement was taken.
    measured_at: u64,
    /// Every relay that answered, nearest first.
    ranked: Vec<String>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Reads the stored measurement, or `None` when there is none to read.
///
/// Every failure is a `None`: a missing file is the first run, and an
/// unreadable one is a file this build will overwrite with its own
/// measurement anyway.
fn read_cache(path: &Path) -> Option<Cached> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Best-effort: a measurement that cannot be written only costs the *next*
/// bind its head start, never this one its relays.
fn write_cache(path: &Path, ranked: &[RelayUrl]) {
    let cached = Cached {
        measured_at: unix_now(),
        ranked: ranked.iter().map(ToString::to_string).collect(),
    };
    let Ok(bytes) = serde_json::to_vec(&cached) else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::debug!(%error, "cannot create the directory for the relay measurement");
        return;
    }
    if let Err(error) = std::fs::write(path, bytes) {
        tracing::debug!(%error, "cannot store the relay measurement");
    }
}

/// What a bind settled on: the relays the endpoint's map holds, and the ones
/// held back for [`Fleet::widen`] to reach for.
#[derive(Debug)]
pub struct Fleet {
    /// Relays the endpoint's map holds, nearest first.
    live: Vec<RelayUrl>,
    /// Measured relays deliberately left *out* of the map, nearest first.
    reserve: Vec<RelayUrl>,
    /// Whether this run has already had to put a reserve relay back.
    ///
    /// It is set at most once, and it is never cleared, so a run's map holds
    /// either one relay or two and nothing else can happen to it.
    ///
    /// Never cleared, because a network that needed a second relay once will
    /// need it again: narrowing back to the same unreachable relay every half
    /// hour would take this node off the air on a schedule.
    ///
    /// Set at most once, because "no relay is connected" is not evidence about
    /// a *relay*. A machine whose network is down reports exactly that, and a
    /// fleet that widened on every health check would walk itself back to the
    /// whole global map — the state this file exists to leave — over a few
    /// minutes of no uplink. Two is where it stops, which is the fleet ADR
    /// 0097 shipped and knew to be safe.
    widened: bool,
    /// Where the measurement is kept for the next bind to start from.
    cache: Option<PathBuf>,
}

impl Fleet {
    /// The relays a bind should hand `RelayMode::custom`, or `None` to leave
    /// the default fleet whole.
    ///
    /// `None` is what "nothing measured and nothing remembered" must mean: the
    /// whole public fleet is the honest answer for a node with no evidence
    /// (§18), and the watcher this fleet spawns narrows it as soon as there
    /// *is* evidence.
    #[must_use]
    pub fn chosen(&self) -> Option<Vec<RelayUrl>> {
        (!self.reserve.is_empty()).then(|| self.live.clone())
    }

    /// Measures the fleet — or reads back the last measurement, if it is still
    /// fresh — and keeps the nearest relay, holding the rest in reserve.
    ///
    /// The cache is what makes a cold start cheap *and* correct. Cheap,
    /// because a bind that would have spent up to [`PROBE_TIMEOUT`] waiting
    /// for a relay on another continent spends nothing; correct, because a
    /// machine whose network is not up yet — a service starting at boot, a
    /// laptop resuming — would otherwise measure nothing and fall back to the
    /// whole global fleet for the rest of its run.
    pub async fn measured(cache: Option<PathBuf>) -> Self {
        let stored = cache.as_deref().and_then(read_cache);
        if let Some(stored) = &stored
            && unix_now().saturating_sub(stored.measured_at) < CACHE_TTL.as_secs()
        {
            let ranked: Vec<RelayUrl> = stored
                .ranked
                .iter()
                .filter_map(|url| url.parse().ok())
                .collect();
            if let Some(fleet) = Self::ranked(ranked, cache.clone()) {
                tracing::info!(
                    relay = %fleet.live.first().map_or_else(String::new, ToString::to_string),
                    "relay fleet: reusing the last measurement (ADR 0098)"
                );
                return fleet;
            }
        }
        if let Some(answered) = measure().await {
            let ranked: Vec<RelayUrl> = answered
                .iter()
                .filter_map(|(hostname, _)| url_of(hostname))
                .collect();
            if let Some(fleet) = Self::ranked(ranked, cache.clone()) {
                if let Some(path) = cache.as_deref() {
                    write_cache(path, &fleet.ordered());
                }
                tracing::info!(
                    relay = %fleet.live.first().map_or_else(String::new, ToString::to_string),
                    reserve = fleet.reserve.len(),
                    "relay fleet: narrowed to the nearest relay (ADR 0098)"
                );
                return fleet;
            }
        }
        // Nothing answered and nothing fresh was remembered. A stale
        // measurement is still the only evidence about this machine there is,
        // and it beats a global fleet whose far half this exists to remove.
        if let Some(stored) = stored {
            let ranked: Vec<RelayUrl> = stored
                .ranked
                .iter()
                .filter_map(|url| url.parse().ok())
                .collect();
            if let Some(fleet) = Self::ranked(ranked, cache.clone()) {
                tracing::info!(
                    relay = %fleet.live.first().map_or_else(String::new, ToString::to_string),
                    "relay fleet: nothing answered; reusing a stale measurement rather than the whole fleet"
                );
                return fleet;
            }
        }
        tracing::info!("relay fleet: nothing to go on; keeping the whole public fleet");
        Self {
            live: fleet_urls(),
            reserve: Vec::new(),
            widened: false,
            cache,
        }
    }

    /// A fleet from a ranking, nearest first, or `None` when the ranking is
    /// too short to narrow anything — a single relay with nothing behind it is
    /// a fleet of one with no way back, which is the one shape this must never
    /// produce.
    fn ranked(ranked: Vec<RelayUrl>, cache: Option<PathBuf>) -> Option<Self> {
        if ranked.len() < 2 {
            return None;
        }
        let mut reserve = ranked;
        let live = vec![reserve.remove(0)];
        Some(Self {
            live,
            reserve,
            widened: false,
            cache,
        })
    }

    /// Everything measured, nearest first — what goes to the cache.
    fn ordered(&self) -> Vec<RelayUrl> {
        let mut all = self.live.clone();
        all.extend(self.reserve.iter().cloned());
        all
    }

    /// Keeps the fleet honest for as long as `endpoint` lives (ADR 0098).
    ///
    /// Three jobs, one task: prove the narrowing works, put a relay back the
    /// moment it stops working, and re-measure often enough that a machine
    /// which moved stops talking to the country it left.
    pub fn watch(self, endpoint: Endpoint) {
        tokio::spawn(self.run(endpoint));
    }

    async fn run(mut self, endpoint: Endpoint) {
        // A TCP connect proved the relay's port answers, not that the relay
        // protocol survives this network. Until the endpoint says it actually
        // reached one, the narrowing is a guess.
        if tokio::time::timeout(RELAY_GRACE, endpoint.online())
            .await
            .is_err()
        {
            self.widen(&endpoint, "the nearest relay did not answer in time")
                .await;
        }
        // A bind that narrowed nothing is on the whole global fleet, which is
        // the state this file exists to leave. It must not sit there for half
        // an hour: by now the endpoint has either come online or given up, so
        // the network is as ready as it is going to get, and a machine that
        // started before its uplink did gets its measurement here.
        if self.reserve.is_empty() && self.live.len() > 1 {
            self.remeasure(&endpoint).await;
        }
        let mut ticker = tokio::time::interval(HEALTH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // fires immediately; the grace above just ran
        let mut since_measured = Duration::ZERO;
        loop {
            ticker.tick().await;
            if endpoint.is_closed() {
                return;
            }
            if !connected(&endpoint) {
                self.widen(&endpoint, "no relay of the fleet is connected")
                    .await;
            }
            since_measured = since_measured.saturating_add(HEALTH_INTERVAL);
            if since_measured >= MEASURE_INTERVAL {
                since_measured = Duration::ZERO;
                self.remeasure(&endpoint).await;
            }
        }
    }

    /// The relay a widening puts back, taken out of the reserve — `None` when
    /// this run has already widened, or has nothing held back.
    ///
    /// Split out of [`Fleet::widen`] because it is the whole of the rule and
    /// none of the I/O: see [`Fleet::widened`] for why it fires once and why
    /// it never resets.
    fn take_reserve(&mut self) -> Option<RelayUrl> {
        if self.widened || self.reserve.is_empty() {
            return None;
        }
        self.widened = true;
        Some(self.reserve.remove(0))
    }

    /// Puts the next-nearest reserve relay back into the map — once.
    async fn widen(&mut self, endpoint: &Endpoint, why: &str) {
        let Some(url) = self.take_reserve() else {
            return;
        };
        tracing::warn!(
            relay = %url,
            reason = why,
            "relay fleet: putting a reserve relay back (ADR 0098)"
        );
        insert(endpoint, &url).await;
        self.live.push(url);
    }

    /// Measures the fleet again and moves the map to the new nearest relay.
    ///
    /// Insert before remove, always: for the length of one `await` the map
    /// holds both, and at no point does it hold neither. A run that has had to
    /// widen keeps everything it has — see [`Fleet::widened`].
    async fn remeasure(&mut self, endpoint: &Endpoint) {
        let Some(answered) = measure().await else {
            return;
        };
        let ranked: Vec<RelayUrl> = answered
            .iter()
            .filter_map(|(hostname, _)| url_of(hostname))
            .collect();
        let Some(nearest) = ranked.first().cloned() else {
            return;
        };
        if let Some(path) = self.cache.as_deref() {
            write_cache(path, &ranked);
        }
        if !self.live.contains(&nearest) {
            tracing::info!(
                relay = %nearest,
                "relay fleet: a nearer relay than the one in use; adding it (ADR 0098)"
            );
            insert(endpoint, &nearest).await;
            self.live.push(nearest.clone());
        }
        if !self.widened && self.live.len() > 1 {
            // Nothing has gone wrong on this network, so the map goes back to
            // one relay: whatever else is in it is somewhere iroh's own
            // ranking can drift to, and drifting is the whole thing this file
            // exists to stop.
            let far: Vec<RelayUrl> = self
                .live
                .iter()
                .filter(|url| **url != nearest)
                .cloned()
                .collect();
            for url in far {
                tracing::info!(
                    relay = %url,
                    "relay fleet: dropping a relay that is no longer the nearest"
                );
                endpoint.remove_relay(&url).await;
                self.live.retain(|live| *live != url);
            }
        }
        // Everything measured that is not in the map, in today's order, so a
        // later `widen` reaches for the nearest of them and not for whichever
        // one happened to be dropped first.
        self.reserve = ranked
            .into_iter()
            .filter(|url| !self.live.contains(url))
            .collect();
    }
}

/// Adds one relay to a live endpoint's map, with exactly the configuration
/// `RelayMode::Default` would have given it (`RelayConfig::from`).
async fn insert(endpoint: &Endpoint, url: &RelayUrl) {
    endpoint
        .insert_relay(url.clone(), Arc::new(RelayConfig::from(url.clone())))
        .await;
}

/// Whether the endpoint currently holds a relay connection at all.
///
/// An empty status is "no home relay chosen yet", which on this cadence means
/// the same thing as a disconnected one: nothing of the map is carrying this
/// node right now.
fn connected(endpoint: &Endpoint) -> bool {
    endpoint
        .home_relay_status()
        .get()
        .iter()
        .any(iroh::endpoint::RelayStatus::is_connected)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "a failed assumption must fail the test")]

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
            let url =
                url_of(hostname).unwrap_or_else(|| panic!("{hostname} must parse as a relay url"));
            assert!(url.to_string().contains(hostname.trim_end_matches('.')));
        }
        assert_eq!(fleet_urls().len(), FLEET.len());
    }

    /// ADR 0098's whole claim: one relay in the map, the rest off it. A relay
    /// that is not in the map is a relay iroh's ranking cannot drift onto.
    #[test]
    fn a_ranking_leaves_one_relay_in_the_map_and_the_rest_in_reserve() {
        let ranked = fleet_urls();
        let fleet = Fleet::ranked(ranked.clone(), None).expect("four relays rank");
        assert_eq!(fleet.live, vec![ranked[0].clone()]);
        assert_eq!(fleet.reserve, ranked[1..].to_vec());
        assert_eq!(fleet.ordered(), ranked, "the cache keeps the whole ranking");
        assert_eq!(fleet.chosen(), Some(vec![ranked[0].clone()]));
    }

    /// The widening rule of ADR 0098: the nearest reserve relay goes back,
    /// exactly once, and a run that has widened stays widened.
    ///
    /// Once, because "no relay is connected" is what a machine with no network
    /// at all reports, and a fleet that widened on every health check would
    /// walk itself back to the whole global map over a few minutes of no
    /// uplink.
    #[test]
    fn a_fleet_widens_once_to_the_nearest_reserve_and_then_never_again() {
        let ranked = fleet_urls();
        let mut fleet = Fleet::ranked(ranked.clone(), None).expect("four relays rank");

        assert_eq!(fleet.take_reserve(), Some(ranked[1].clone()));
        assert!(fleet.widened);
        assert_eq!(
            fleet.take_reserve(),
            None,
            "a second widening would walk the map back to the whole fleet"
        );
        assert_eq!(
            fleet.reserve,
            ranked[2..].to_vec(),
            "what was not put back is still held back"
        );
    }

    /// A fleet of one with nothing behind it is the one shape this must never
    /// produce: there would be no relay left to put back when it failed.
    #[test]
    fn a_ranking_of_one_is_refused() {
        assert!(Fleet::ranked(fleet_urls()[..1].to_vec(), None).is_none());
        assert!(Fleet::ranked(Vec::new(), None).is_none());
    }

    /// Nothing measured and nothing remembered leaves the default fleet whole
    /// (§18), which `chosen` says by handing back `None`.
    #[tokio::test]
    async fn nothing_to_go_on_keeps_the_whole_fleet() {
        let fleet = Fleet {
            live: fleet_urls(),
            reserve: Vec::new(),
            widened: false,
            cache: None,
        };
        assert!(fleet.chosen().is_none());
    }

    /// The measurement survives the trip to disk and back, which is what
    /// makes a cold start cheap rather than a guess.
    #[test]
    fn a_measurement_round_trips_through_the_cache_file() {
        let dir = std::env::temp_dir().join(format!("lumepeer-relay-{}", unix_now()));
        let path = dir.join("relays.json");
        let ranked = fleet_urls();
        write_cache(&path, &ranked);
        let read = read_cache(&path).expect("what was just written must read back");
        assert_eq!(read.ranked.len(), ranked.len());
        assert_eq!(read.ranked[0], ranked[0].to_string());
        assert!(unix_now().saturating_sub(read.measured_at) < CACHE_TTL.as_secs());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache that cannot be read is a first run, never a failure: the bind
    /// that reads it has to measure instead, not stop.
    #[test]
    fn an_unreadable_cache_reads_as_nothing() {
        assert!(read_cache(Path::new("no-such-file-anywhere.json")).is_none());
    }
}
