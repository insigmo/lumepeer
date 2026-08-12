//! `lumepeer-net` — Iroh endpoint, invite tickets, control streams, framing
//! and reconnect (design doc §4). Part of the application TCB: it authenticates
//! peers but never authorizes them; every authorization decision belongs to
//! `lumepeer-core` (§2.3).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod connection;
pub mod endpoint;
pub mod error;
pub mod framing;
pub mod keystore;
pub mod reconnect;
pub mod ticket;

pub use endpoint::{ALPN_CONTROL, ALPN_FILE, ALPN_MEDIA};
pub use error::{NetError, Result};
