//! Fuzzes the invite ticket decoder (design doc §7, §17.2).
//!
//! A ticket arrives from a QR code or a short link, both of which an attacker
//! controls. Decoding must never panic; it also authorizes nothing on its own,
//! the host still verifies the signature and the TTL.

#![no_main]

use libfuzzer_sys::fuzz_target;
use lumepeer_net::ticket::InviteTicket;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(ticket) = InviteTicket::from_qr_string(text) {
        // Re-encoding an accepted ticket must produce something that decodes
        // back to the same ticket.
        if let Ok(encoded) = ticket.to_qr_string() {
            let again = InviteTicket::from_qr_string(&encoded).expect("a re-encoded ticket must parse");
            assert_eq!(ticket, again);
        }
    }
});
