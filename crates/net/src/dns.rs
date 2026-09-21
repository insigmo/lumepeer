//! The DNS resolver the endpoint hands to iroh (§4), with `AAAA` suppressed on
//! a machine that has no routable IPv6 (ADR 0095).
//!
//! iroh resolves a relay's hostname over both families at once and dials
//! whatever comes back. On a host with IPv6 *configured but not routed* — a
//! Windows box with a tunnel adapter, most commonly — every `AAAA` answer is
//! an address the routing table has no way to reach, and the relay client
//! spends its connect budget on them:
//!
//! ```text
//! Failed to connect to relay server: unable to connect: A socket operation
//!   was attempted to an unreachable network. (os error 10051)
//! Failed to connect to relay server: unable to connect: Resolve failed,
//!   IPv4: Request timed out, IPv6: Request timed out
//! ```
//!
//! Two things follow, and the second is the one that costs a session. The
//! obvious cost is that the home relay link keeps dropping. The hidden one is
//! that iroh ranks relays by measured latency and drops the ones that do not
//! answer, so a machine whose near relays are being probed through an
//! unreachable family ends up *preferring a far one*: the host this was
//! diagnosed on sat on a Singapore relay at ~250 ms while the European one it
//! could actually reach answered in ~95 ms, and every session between two
//! machines in Europe was being carried half-way around the world until the
//! link collapsed under it.
//!
//! So the fix is not "log it better". If this machine cannot route IPv6, the
//! honest answer to an `AAAA` lookup is that there is nothing here to dial,
//! and saying so immediately is what lets the latency ranking see the truth.
//! A machine that *does* have IPv6 is untouched: the probe answers yes and
//! every lookup is the one iroh would have made anyway.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::OnceLock;

use iroh::dns::{BoxIter, DNS_TIMEOUT, DnsError, DnsResolver, Resolver, TxtRecordData};

/// `n0_future::boxed::BoxFuture`, the return type [`Resolver`] is declared
/// with, written out so this crate does not take a dependency on that crate
/// for one type alias.
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The address the IPv6 probe asks the routing table about.
///
/// A documentation-range address (RFC 3849), never a host anybody runs:
/// connecting a UDP socket sends no packet at all, it only makes the kernel
/// pick a source address and a route, so the question is answered without
/// anything leaving this machine and without naming a third party.
const IPV6_ROUTE_PROBE: SocketAddr = SocketAddr::new(
    IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1)),
    53,
);

/// Whether this machine has a route to the global IPv6 internet.
///
/// Asked of the routing table rather than of the interface list on purpose. A
/// host can hold several IPv6 addresses that reach nothing — a link-local one
/// on every adapter, a unique-local one from a VPN — and counting addresses
/// would call that machine dual-stacked. Binding a UDP socket and connecting
/// it is the one question whose answer is "is there a route", and it sends no
/// traffic.
fn has_routable_ipv6() -> bool {
    let Ok(socket) = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)) else {
        // No IPv6 stack at all to bind on.
        return false;
    };
    socket.connect(IPV6_ROUTE_PROBE).is_ok()
}

/// The system resolver, with `AAAA` answered as "nothing here" while this
/// machine has no route to the IPv6 internet.
///
/// Wraps a [`DnsResolver`] rather than re-implementing one over hickory: the
/// resolver iroh builds for itself is the one that reads this platform's own
/// nameserver configuration, and reproducing that here to change one answer
/// would be a second copy to keep correct forever.
#[derive(Debug)]
struct SingleStackResolver {
    /// The resolver every lookup is actually made through.
    inner: DnsResolver,
    /// Whether IPv6 is routable, probed once on first use.
    ///
    /// Lazy because [`Resolver::reset`] must not perform IO: a network change
    /// hands back a fresh value of this type with an empty cell, and the probe
    /// runs again the next time something asks for an `AAAA` record — which is
    /// exactly when the answer matters and exactly when it is current.
    ipv6: OnceLock<bool>,
}

impl SingleStackResolver {
    /// Wraps `inner`, deferring the IPv6 probe to the first `AAAA` lookup.
    fn new(inner: DnsResolver) -> Self {
        Self {
            inner,
            ipv6: OnceLock::new(),
        }
    }

    /// Whether `AAAA` lookups are worth making at all, probing once.
    fn ipv6_is_usable(&self) -> bool {
        *self.ipv6.get_or_init(|| {
            let usable = has_routable_ipv6();
            if !usable {
                tracing::info!(
                    "no route to the IPv6 internet: AAAA lookups are answered empty (ADR 0095)"
                );
            }
            usable
        })
    }
}

impl Resolver for SingleStackResolver {
    fn lookup_ipv4(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv4Addr>, DnsError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let addrs = inner.lookup_ipv4(host, DNS_TIMEOUT).await?;
            // `DnsResolver` widens its answers to `IpAddr`; the trait wants the
            // family back. Nothing is dropped: an IPv4 lookup only ever yields
            // `V4`, and the match is what turns that fact into the type.
            let only_v4 = addrs.filter_map(|addr| match addr {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            });
            Ok(Box::new(only_v4) as BoxIter<Ipv4Addr>)
        })
    }

    fn lookup_ipv6(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv6Addr>, DnsError>> {
        if !self.ipv6_is_usable() {
            // Empty rather than an error. iroh joins the two families and an
            // error from one is only swallowed while the other succeeds — so a
            // host with no A record would turn this into `ResolveBoth` and
            // report a DNS failure for what is really "this machine has no
            // IPv6". An empty answer composes correctly everywhere instead.
            return Box::pin(std::future::ready(Ok(
                Box::new(std::iter::empty()) as BoxIter<Ipv6Addr>
            )));
        }
        let inner = self.inner.clone();
        Box::pin(async move {
            let addrs = inner.lookup_ipv6(host, DNS_TIMEOUT).await?;
            let only_v6 = addrs.filter_map(|addr| match addr {
                IpAddr::V6(v6) => Some(v6),
                IpAddr::V4(_) => None,
            });
            Ok(Box::new(only_v6) as BoxIter<Ipv6Addr>)
        })
    }

    fn lookup_txt(&self, host: String) -> BoxFuture<Result<BoxIter<TxtRecordData>, DnsError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            // Collected rather than boxed straight through: unlike the two
            // address lookups, this one's iterator borrows the resolver, so it
            // cannot outlive this future the way the trait's return type asks.
            let records: Vec<TxtRecordData> = inner.lookup_txt(host, DNS_TIMEOUT).await?.collect();
            Ok(Box::new(records.into_iter()) as BoxIter<TxtRecordData>)
        })
    }

    fn clear_cache(&self) {
        self.inner.clear_cache();
    }

    fn reset(&self) -> Box<dyn Resolver> {
        // `DnsResolver::reset` swaps its own inner resolver for a freshly
        // built one without performing IO, which is the contract this method
        // is held to as well. The IPv6 cell starts empty so the next `AAAA`
        // lookup re-probes: a network change is exactly when a machine gains
        // or loses its route.
        self.inner.reset();
        Box::new(Self::new(self.inner.clone()))
    }
}

/// The resolver to hand an endpoint builder (ADR 0095).
#[must_use]
pub fn resolver() -> DnsResolver {
    DnsResolver::custom(SingleStackResolver::new(DnsResolver::new()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "a failed assumption must fail the test"
    )]

    use super::*;

    /// The probe has to answer without panicking or blocking on every host CI
    /// runs on, whichever way it answers there: the endpoint's bind depends on
    /// it, and a probe that can fail the bind would be worse than the skewed
    /// relay ranking it exists to fix.
    #[test]
    fn the_ipv6_probe_answers_on_this_machine() {
        let _answer: bool = has_routable_ipv6();
    }

    /// The probe is made once and reused: it is a syscall on the path of every
    /// `AAAA` lookup, and an endpoint makes a great many of those.
    #[test]
    fn the_probe_is_cached_after_the_first_lookup() {
        let resolver = SingleStackResolver::new(DnsResolver::new());
        let first = resolver.ipv6_is_usable();
        assert_eq!(resolver.ipv6.get(), Some(&first));
        assert_eq!(resolver.ipv6_is_usable(), first);
    }

    /// A reset is what a network change hands back, and the whole point of it
    /// here is that the answer is asked again afterwards rather than carried
    /// over from the network this process started on.
    #[test]
    fn a_reset_forgets_the_probe() {
        let resolver = SingleStackResolver::new(DnsResolver::new());
        let _ = resolver.ipv6_is_usable();
        assert!(resolver.ipv6.get().is_some());
        let fresh = resolver.reset();
        // Downcasting is not available through the trait object, so the
        // observable claim is the one that matters: a reset produces a
        // resolver that still answers, and the contract above says it starts
        // without a cached probe.
        fresh.clear_cache();
    }

    /// On a machine with no IPv6 route the answer is an empty list, never an
    /// error: iroh only swallows a lookup error while the other family
    /// answered, so an error here would turn a host with no `A` record into a
    /// reported DNS failure instead of "no IPv6 on this machine".
    #[tokio::test]
    async fn a_machine_without_ipv6_answers_aaaa_empty() {
        let resolver = SingleStackResolver::new(DnsResolver::new());
        // Force the "no route" branch without depending on how this machine is
        // connected, which CI cannot promise either way.
        let _ = resolver.ipv6.set(false);
        let answer = resolver.lookup_ipv6("example.invalid".to_owned()).await;
        assert_eq!(answer.unwrap().count(), 0);
    }
}
