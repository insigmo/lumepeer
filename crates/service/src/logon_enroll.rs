//! The one message a Linux client sends the sign-in screen's supervisor
//! (ADR 0151).
//!
//! On Linux the account's identity and device password live in the Secret
//! Service, which stays locked until that account signs in — exactly the
//! moment the sign-in screen is gone. So the client hands a copy of those
//! entries to the supervisor while it can read them, and the supervisor keeps
//! the copy where only `root` can read it ([`STATE_DIR`]). The logon host it
//! later starts reads that copy instead of the keyring.
//!
//! The format is deliberately small and fixed: a header, an operation, and for
//! an enrolment a bounded list of `(name, bytes)` pairs. The supervisor never
//! interprets the bytes; it checks the shape, the bounds and who is asking
//! (`SO_PEERCRED`), and writes the entries out as they came. Only the logon
//! host, which links the keystore, decides what each entry means.
//!
//! Pure Rust and platform-independent so the codec is tested everywhere; the
//! socket and the files are Linux-only and live with the supervisor.

use std::io::{self, Read};

/// Where the supervisor listens. Under the runtime directory systemd creates
/// for the unit (`RuntimeDirectory=lumepeer`).
pub const SOCKET_PATH: &str = "/run/lumepeer/logon-host.sock";

/// Where the supervisor keeps the owner and the entries, `root`-only.
pub const STATE_DIR: &str = "/var/lib/lumepeer/logon-host";

/// The owner's uid, decimal, one line.
pub const OWNER_FILE: &str = "owner";

/// The owner's entries, in the [`encode_entries`] format.
pub const SECRETS_FILE: &str = "secrets";

const MAGIC: [u8; 4] = *b"LPLH";
const VERSION: u8 = 1;
const HEADER_BYTES: usize = 10;

/// Largest payload the supervisor reads. The five entries a host keeps come
/// to a few hundred bytes; anything near this is not one of them.
pub const MAX_PAYLOAD: usize = 16 * 1024;

/// Most entries one enrolment may carry.
pub const MAX_ENTRIES: usize = 16;

/// Longest entry name.
pub const MAX_NAME: usize = 64;

/// Longest entry value.
pub const MAX_VALUE: usize = 4096;

const OP_ENROLL: u8 = 1;
const OP_WITHDRAW: u8 = 2;
const OP_STATUS: u8 = 3;

/// What a client asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Host the sign-in screen with these entries, as the asking account.
    Enroll(Vec<(String, Vec<u8>)>),
    /// Stop hosting it with the asking account's entries.
    Withdraw,
    /// Whether the asking account is the one hosting it.
    Status,
}

/// What the supervisor answers, one byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Done, or for [`Request::Status`]: the asker's entries are the ones in use.
    Yours = 0,
    /// For [`Request::Status`] and [`Request::Withdraw`]: nobody's entries are
    /// kept.
    NotEnrolled = 1,
    /// Another account hosts the sign-in screen; only it can change that.
    OtherOwner = 2,
    /// The request did not parse.
    Refused = 3,
    /// The supervisor could not write what it was given.
    Failed = 4,
}

impl Reply {
    /// The reply a byte stands for; anything unknown reads as [`Self::Refused`].
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Yours,
            1 => Self::NotEnrolled,
            2 => Self::OtherOwner,
            4 => Self::Failed,
            _ => Self::Refused,
        }
    }
}

/// Whether `name` is a name an entry may have: `lumepeer.` and then lowercase
/// letters, digits and dots, at most [`MAX_NAME`] long.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME
        && name.strip_prefix("lumepeer.").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.')
        })
}

/// Encodes entries: a count, then each name and value with its length.
///
/// # Errors
/// [`io::ErrorKind::InvalidInput`] when the entries break a bound or a name is
/// not [`valid_name`]: nothing is encoded that [`decode_entries`] would refuse.
pub fn encode_entries(entries: &[(String, Vec<u8>)]) -> io::Result<Vec<u8>> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_owned());
    if entries.len() > MAX_ENTRIES {
        return Err(invalid("too many entries"));
    }
    let mut out = vec![u8::try_from(entries.len()).map_err(|_| invalid("too many entries"))?];
    for (name, value) in entries {
        if !valid_name(name) {
            return Err(invalid("entry name"));
        }
        if value.len() > MAX_VALUE {
            return Err(invalid("entry value too long"));
        }
        out.push(u8::try_from(name.len()).map_err(|_| invalid("entry name"))?);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(
            &u16::try_from(value.len())
                .map_err(|_| invalid("entry value too long"))?
                .to_be_bytes(),
        );
        out.extend_from_slice(value);
    }
    if out.len() > MAX_PAYLOAD {
        return Err(invalid("entries too long"));
    }
    Ok(out)
}

/// Decodes what [`encode_entries`] wrote, refusing anything out of bounds,
/// any name twice, and trailing bytes.
///
/// # Errors
/// [`io::ErrorKind::InvalidData`] for every malformed input.
pub fn decode_entries(bytes: &[u8]) -> io::Result<Vec<(String, Vec<u8>)>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed entries");
    if bytes.len() > MAX_PAYLOAD {
        return Err(bad());
    }
    let (&count, mut rest) = bytes.split_first().ok_or_else(bad)?;
    let count = usize::from(count);
    if count > MAX_ENTRIES {
        return Err(bad());
    }
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(count);
    for _ in 0..count {
        let (&name_len, tail) = rest.split_first().ok_or_else(bad)?;
        let name_len = usize::from(name_len);
        if tail.len() < name_len + 2 {
            return Err(bad());
        }
        let (name, tail) = tail.split_at(name_len);
        let name = std::str::from_utf8(name).map_err(|_| bad())?;
        if !valid_name(name) || entries.iter().any(|(seen, _)| seen == name) {
            return Err(bad());
        }
        let (len, tail) = tail.split_at(2);
        let value_len = usize::from(u16::from_be_bytes([len[0], len[1]]));
        if value_len > MAX_VALUE || tail.len() < value_len {
            return Err(bad());
        }
        let (value, tail) = tail.split_at(value_len);
        entries.push((name.to_owned(), value.to_vec()));
        rest = tail;
    }
    if !rest.is_empty() {
        return Err(bad());
    }
    Ok(entries)
}

/// Encodes a request with its header.
///
/// # Errors
/// As [`encode_entries`], for an enrolment.
pub fn encode_request(request: &Request) -> io::Result<Vec<u8>> {
    let (op, payload) = match request {
        Request::Enroll(entries) => (OP_ENROLL, encode_entries(entries)?),
        Request::Withdraw => (OP_WITHDRAW, Vec::new()),
        Request::Status => (OP_STATUS, Vec::new()),
    };
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(op);
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload too long"))?
            .to_be_bytes(),
    );
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Reads one request from `reader`, reading no more than its header says and
/// never more than [`MAX_PAYLOAD`].
///
/// # Errors
/// [`io::ErrorKind::InvalidData`] for a wrong magic, version, operation or
/// length, or malformed entries; whatever the reader returns otherwise.
pub fn read_request(reader: &mut impl Read) -> io::Result<Request> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let mut header = [0u8; HEADER_BYTES];
    reader.read_exact(&mut header)?;
    if header[..4] != MAGIC {
        return Err(bad("not a lumepeer request"));
    }
    if header[4] != VERSION {
        return Err(bad("unknown version"));
    }
    let len = usize::try_from(u32::from_be_bytes([
        header[6], header[7], header[8], header[9],
    ]))
    .map_err(|_| bad("length"))?;
    if len > MAX_PAYLOAD {
        return Err(bad("payload too long"));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    match header[5] {
        OP_ENROLL => Ok(Request::Enroll(decode_entries(&payload)?)),
        OP_WITHDRAW if payload.is_empty() => Ok(Request::Withdraw),
        OP_STATUS if payload.is_empty() => Ok(Request::Status),
        _ => Err(bad("unknown operation")),
    }
}

/// Asks the supervisor `request` and returns its answer.
///
/// # Errors
/// Whatever connecting, writing or reading returns — above all
/// [`io::ErrorKind::NotFound`] / [`io::ErrorKind::ConnectionRefused`] when no
/// supervisor runs (an `AppImage`, a development build, the unit disabled).
#[cfg(target_os = "linux")]
pub fn ask(request: &Request) -> io::Result<Reply> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.write_all(&encode_request(request)?)?;
    let mut reply = [0u8; 1];
    stream.read_exact(&mut reply)?;
    Ok(Reply::from_byte(reply[0]))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn entries() -> Vec<(String, Vec<u8>)> {
        vec![
            ("lumepeer.endpoint.identity".to_owned(), vec![7; 64]),
            (
                "lumepeer.unattended.password".to_owned(),
                b"$argon2id$v=19$...".to_vec(),
            ),
            ("lumepeer.unattended.role".to_owned(), Vec::new()),
        ]
    }

    #[test]
    fn every_request_survives_the_wire() {
        for request in [
            Request::Enroll(entries()),
            Request::Enroll(Vec::new()),
            Request::Withdraw,
            Request::Status,
        ] {
            let bytes = encode_request(&request).unwrap();
            assert_eq!(read_request(&mut bytes.as_slice()).unwrap(), request);
        }
    }

    #[test]
    fn entries_round_trip_and_nothing_trails() {
        let bytes = encode_entries(&entries()).unwrap();
        assert_eq!(decode_entries(&bytes).unwrap(), entries());
        let mut longer = bytes;
        longer.push(0);
        assert!(decode_entries(&longer).is_err());
    }

    #[test]
    fn names_outside_the_pattern_are_refused_both_ways() {
        for name in [
            "",
            "lumepeer.",
            "other.entry",
            "lumepeer.Upper",
            "lumepeer.a/b",
        ] {
            assert!(!valid_name(name), "{name}");
            assert!(encode_entries(&[(name.to_owned(), Vec::new())]).is_err());
        }
        // Hand-built, so the decoder is checked without the encoder's help.
        let mut bytes = vec![1, 10];
        bytes.extend_from_slice(b"lumepeer.A");
        bytes.extend_from_slice(&[0, 0]);
        assert!(decode_entries(&bytes).is_err());
    }

    #[test]
    fn a_name_twice_is_refused() {
        let mut bytes = encode_entries(&entries()[..1]).unwrap();
        let one = bytes[1..].to_vec();
        bytes[0] = 2;
        bytes.extend_from_slice(&one);
        assert!(decode_entries(&bytes).is_err());
    }

    #[test]
    fn lengths_past_the_bounds_are_refused_before_reading_them() {
        let mut header = Vec::from(MAGIC);
        header.push(VERSION);
        header.push(OP_ENROLL);
        header.extend_from_slice(&u32::try_from(MAX_PAYLOAD + 1).unwrap().to_be_bytes());
        // No payload follows: a reader that trusted the length would block or
        // allocate; this one refuses on the header.
        assert_eq!(
            read_request(&mut header.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(encode_entries(&[("lumepeer.x".to_owned(), vec![0; MAX_VALUE + 1])]).is_err());
    }

    #[test]
    fn a_truncated_entry_is_refused() {
        let bytes = encode_entries(&entries()).unwrap();
        for cut in 1..bytes.len() {
            assert!(decode_entries(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn a_status_or_withdraw_with_a_payload_is_refused() {
        let mut bytes = encode_request(&Request::Status).unwrap();
        bytes[9] = 1;
        bytes.push(0);
        assert!(read_request(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn unknown_reply_bytes_read_as_refused() {
        assert_eq!(Reply::from_byte(0), Reply::Yours);
        assert_eq!(Reply::from_byte(2), Reply::OtherOwner);
        assert_eq!(Reply::from_byte(200), Reply::Refused);
    }
}
