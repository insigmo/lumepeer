//! `lumepeer-net` — Iroh endpoint, invite tickets, control streams, framing
//! and reconnect (design doc §4). Part of the application TCB: it authenticates
//! peers but never authorizes them; every authorization decision belongs to
//! `lumepeer-core` (§2.3).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod connection;
pub mod endpoint;
pub mod error;
pub mod file_transfer;
pub mod framing;
pub mod keystore;
pub mod media;
pub mod reconnect;
pub mod ticket;

pub use connection::{
    Channel, ControlConnection, ControlReader, ControlWriter, HelloInfo, guest_handshake,
    host_handshake,
};
pub use endpoint::{ALPN_CONTROL, ALPN_FILE, ALPN_MEDIA, PeerEndpoint};
pub use error::{NetError, Result};
pub use media::{
    MediaFrameReader, MediaFrameWriter, STREAM_AUDIO, STREAM_MIC, STREAM_VIDEO,
    accept_audio_media_stream, accept_media_stream, accept_tagged_media_stream,
    check_media_frame_length, decode_audio_payload, encode_audio_payload, open_media_stream,
    open_tagged_media_stream,
};
pub use ticket::InviteTicket;
