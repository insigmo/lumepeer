//! Prints this machine's public reflexive address as learned by a single
//! stateless STUN Binding query (task 17; ADR 0052).
//!
//! This is the serverless address-discovery step of the obfuscated-QUIC
//! transport, on its own: it proves a host behind NAT can learn the public
//! `ip:port` a peer would dial, without n0's relay and without any server
//! carrying data — only a stateless reflector answering one request. Run it on
//! both machines of the rig; the address it prints is what would go into the
//! invite.
//!
//! ```text
//! cargo run -p lumepeer-net --example stun_probe
//! cargo run -p lumepeer-net --example stun_probe -- stun.cloudflare.com:3478
//! ```

use std::net::{ToSocketAddrs as _, UdpSocket};

use lumepeer_net::stun;

/// STUN servers tried in order until one answers. Overridable by CLI argument.
/// These are public reflectors, not relays: they see one request and reply with
/// the source address, nothing more.
const DEFAULT_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
];

fn main() {
    let servers: Vec<String> = match std::env::args().nth(1) {
        Some(server) => vec![server],
        None => DEFAULT_SERVERS.iter().map(|s| (*s).to_owned()).collect(),
    };

    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("bind failed: {e}");
            std::process::exit(1);
        }
    };
    if let Ok(local) = socket.local_addr() {
        println!("LOCAL {local}");
    }

    for server in &servers {
        let Some(addr) = server.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            println!("SKIP {server}: does not resolve");
            continue;
        };
        match stun::reflexive_addr(&socket, addr) {
            Ok(reflexive) => {
                println!("REFLEXIVE {reflexive} (via {server})");
                return;
            }
            Err(e) => println!("SKIP {server}: {e}"),
        }
    }
    eprintln!("no STUN server answered");
    std::process::exit(1);
}
