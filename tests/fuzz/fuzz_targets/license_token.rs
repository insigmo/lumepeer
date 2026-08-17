//! Fuzzes the license token parser (design doc §12.1, §17.2).
//!
//! A token is attacker-supplied until `verify_strict` says otherwise, so the
//! parser must reject anything unsigned without panicking and must never read
//! past the fixed layout.

#![no_main]

use libfuzzer_sys::fuzz_target;
use lumepeer_core::license::LicenseToken;

fuzz_target!(|data: &[u8]| {
    let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]).verifying_key();
    // Without the broker's private key, no fuzzer-generated input should ever
    // verify. If one does, the signature check is broken and the assert says so.
    assert!(
        LicenseToken::parse_and_verify(data, &key).is_err(),
        "an unsigned token must never verify"
    );
});
