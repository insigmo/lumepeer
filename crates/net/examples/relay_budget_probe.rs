//! Measures how much a relay-carried flow delivers before it freezes, and how
//! long it stays frozen: the question behind beta's `Ping timeout` storms.
//!
//! Both sides are relay-only and pinned to one relay, so every byte crosses
//! the relay's TCP/TLS link. The sender writes a fixed-size chunk at a fixed
//! interval; the receiver prints the byte count at every gap and resume. Two
//! rates that stall at the same byte count point at a byte budget; two that
//! stall at the same time point at a timer.
//!
//! ```text
//! relay_budget_probe host [relay-url]
//! # -> prints `ID <hex>`
//! relay_budget_probe guest <id> <down|up> <chunk-bytes> <interval-ms> <secs> [relay-url]
//! ```
//! `down`: the guest sends and the host receives. `up`: the host sends.

use std::time::{Duration, Instant};

use iroh::endpoint::{QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl};

const ALPN: &[u8] = b"lumepeer/relay-budget-probe/0";
const DEFAULT_RELAY: &str = "https://euc1-1.relay.n0.iroh.link./";
/// A gap this long in the byte stream is reported as a stall.
const STALL: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("warn,iroh::socket::transports::relay=info")
            }),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("host") => host(args.get(1).map_or(DEFAULT_RELAY, String::as_str)).await,
        Some("guest") if args.len() >= 6 => guest(&args).await,
        _ => Err("usage: see the module docs".to_owned()),
    };
    if let Err(e) = result {
        println!("RESULT failed: {e}");
        std::process::exit(1);
    }
}

async fn bind(relay: &str) -> Result<Endpoint, String> {
    let url: RelayUrl = relay.parse().map_err(|e| format!("relay url: {e}"))?;
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            Duration::from_mins(2)
                .try_into()
                .map_err(|e| format!("{e}"))?,
        ))
        .keep_alive_interval(Duration::from_secs(5))
        .build();
    let endpoint = Endpoint::builder(presets::N0)
        .clear_ip_transports()
        .relay_mode(RelayMode::custom([url]))
        .transport_config(transport)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| format!("bind: {e}"))?;
    tokio::time::timeout(Duration::from_secs(30), endpoint.online())
        .await
        .map_err(|_| "no relay within 30s".to_owned())?;
    Ok(endpoint)
}

async fn host(relay: &str) -> Result<(), String> {
    let endpoint = bind(relay).await?;
    println!("ID {}", endpoint.id());
    loop {
        let Some(incoming) = endpoint.accept().await else {
            return Ok(());
        };
        let connection = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                println!("accept failed: {e}");
                continue;
            }
        };
        let (mut send, mut recv) = match connection.accept_bi().await {
            Ok(s) => s,
            Err(e) => {
                println!("accept_bi failed: {e}");
                continue;
            }
        };
        let mut header = [0u8; 13];
        if let Err(e) = recv.read_exact(&mut header).await {
            println!("header: {e}");
            continue;
        }
        let (mode, chunk, interval, secs) = parse_header(&header);
        println!(
            "TEST mode={} chunk={chunk} interval_ms={interval} secs={secs}",
            mode as char
        );
        if mode == b'd' {
            receive(&mut recv, secs).await;
        } else {
            transmit(&mut send, chunk, interval, secs).await;
        }
        connection.close(0u32.into(), b"done");
    }
}

async fn guest(args: &[String]) -> Result<(), String> {
    let id: iroh::EndpointId = args[1].parse().map_err(|e| format!("id: {e}"))?;
    let mode = match args[2].as_str() {
        "down" => b'd',
        "up" => b'u',
        other => return Err(format!("mode {other}")),
    };
    let chunk: u32 = args[3].parse().map_err(|e| format!("chunk: {e}"))?;
    let interval: u32 = args[4].parse().map_err(|e| format!("interval: {e}"))?;
    let secs: u32 = args[5].parse().map_err(|e| format!("secs: {e}"))?;
    let relay = args.get(6).map_or(DEFAULT_RELAY, String::as_str);
    let endpoint = bind(relay).await?;
    let url: RelayUrl = relay.parse().map_err(|e| format!("relay url: {e}"))?;
    let started = Instant::now();
    let connection = endpoint
        .connect(EndpointAddr::new(id).with_relay_url(url), ALPN)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    println!("CONNECTED in {:.1}s", started.elapsed().as_secs_f32());
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| format!("open_bi: {e}"))?;
    let mut header = [0u8; 13];
    header[0] = mode;
    header[1..5].copy_from_slice(&chunk.to_be_bytes());
    header[5..9].copy_from_slice(&interval.to_be_bytes());
    header[9..13].copy_from_slice(&secs.to_be_bytes());
    send.write_all(&header)
        .await
        .map_err(|e| format!("header: {e}"))?;
    if mode == b'd' {
        transmit(&mut send, chunk, interval, secs).await;
    } else {
        receive(&mut recv, secs).await;
    }
    connection.close(0u32.into(), b"done");
    endpoint.close().await;
    println!("RESULT ok");
    Ok(())
}

fn parse_header(h: &[u8; 13]) -> (u8, u32, u32, u32) {
    let word = |i: usize| u32::from_be_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
    (h[0], word(1), word(5), word(9))
}

async fn transmit(send: &mut iroh::endpoint::SendStream, chunk: u32, interval: u32, secs: u32) {
    let started = Instant::now();
    let data = vec![0x5a_u8; chunk as usize];
    let mut written: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(u64::from(interval)));
    while started.elapsed() < Duration::from_secs(u64::from(secs)) {
        tick.tick().await;
        match tokio::time::timeout(Duration::from_secs(30), send.write_all(&data)).await {
            Ok(Ok(())) => written += u64::from(chunk),
            Ok(Err(e)) => {
                println!("SEND error after {written} bytes: {e}");
                return;
            }
            Err(_) => println!("SEND blocked 30s at {written} bytes"),
        }
    }
    println!(
        "SENT {written} bytes in {:.1}s",
        started.elapsed().as_secs_f32()
    );
    let _ = send.finish();
}

async fn receive(recv: &mut iroh::endpoint::RecvStream, secs: u32) {
    let started = Instant::now();
    let deadline = Duration::from_secs(u64::from(secs) + 60);
    let mut total: u64 = 0;
    let mut last = Instant::now();
    let mut stalled = false;
    let mut buf = vec![0u8; 64 * 1024];
    let mut report = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            read = recv.read(&mut buf) => match read {
                Ok(Some(n)) => {
                    if stalled {
                        println!(
                            "RESUME t={:.1}s after {:.1}s gap, at {total} bytes",
                            started.elapsed().as_secs_f32(),
                            last.elapsed().as_secs_f32()
                        );
                        stalled = false;
                    }
                    total += n as u64;
                    last = Instant::now();
                }
                Ok(None) => break,
                Err(e) => {
                    println!("RECV error t={:.1}s at {total} bytes: {e}", started.elapsed().as_secs_f32());
                    break;
                }
            },
            _ = report.tick() => {
                if !stalled && last.elapsed() >= STALL && total > 0 {
                    stalled = true;
                    println!(
                        "STALL t={:.1}s at {total} bytes (last byte {:.1}s ago)",
                        started.elapsed().as_secs_f32(),
                        last.elapsed().as_secs_f32()
                    );
                }
                if started.elapsed() > deadline {
                    break;
                }
            }
        }
    }
    println!(
        "RECEIVED {total} bytes in {:.1}s",
        started.elapsed().as_secs_f32()
    );
}
