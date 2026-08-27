//! Session recording: the custom `LMREC` container (design doc §9.2, §17;
//! questions.md item 7; ADR 0023).
//!
//! Decision recorded in questions.md: a minimal self-describing container —
//! a header, then a sequence of timestamped records (video frames, audio
//! chunks, session events) — no external dependency, with MKV export as a
//! later, separate step. The format is append-only by construction: a writer
//! never seeks, so an interrupted session leaves a valid prefix that this
//! module's reader still replays up to the truncation point.
//!
//! Wire shape (all integers big-endian):
//!
//! ```text
//! magic   "LMRC" (4 bytes)
//! version u16  = RECORD_FORMAT_VERSION
//! flags   u16  = bit 0: contains audio
//! header  8 bytes total
//! record  kind:u8 | reserved:u8[3] | t_us:u64 | len:u32 | payload[len]
//!   kind 1 = video frame (H.264 bitstream chunk)
//!       2 = audio chunk (Opus packet)
//!       3 = event (JSON line of the action log)
//! ```
//!
//! Bounds mirror the media path: every length is checked before allocation,
//! and any violation is an error, never a panic. Nothing here encrypts —
//! a recording is as sensitive as the session itself (§15), so files land
//! only where the host user chose to put them.

use std::io::{self, Read, Write};

use lumepeer_core::CoreError;
use lumepeer_core::constants::MAX_MEDIA_FRAME_BYTES;

/// Magic bytes at offset 0 of every recording.
pub const MAGIC: [u8; 4] = *b"LMRC";
/// Container version this module writes. Readers refuse newer majors.
pub const RECORD_FORMAT_VERSION: u16 = 1;
/// Header flag bits. V1 writers always emit 0 here: the file is append-only,
/// so a late-discovered audio track cannot retroactively edit the header.
/// Readers derive audio presence from the records themselves.
pub const FLAG_RESERVED: u16 = 0;

/// Record kind: video frame (H.264 bitstream chunk).
pub const KIND_VIDEO: u8 = 1;
/// Record kind: audio chunk (Opus packet).
pub const KIND_AUDIO: u8 = 2;
/// Record kind: event-log JSON line.
pub const KIND_EVENT: u8 = 3;

/// Header size in bytes: magic(4) + version(2) + flags(2).
pub const HEADER_BYTES: usize = 8;
/// Record preamble in bytes: kind(1) + reserved(3) + timestamp(8) + len(4).
pub const RECORD_PREAMBLE_BYTES: usize = 16;

/// Hard ceiling for one record payload. Video rides the media bound; events
/// are chat-sized JSON lines. A larger announcement is corrupt or hostile.
fn max_payload_for(kind: u8) -> usize {
    match kind {
        KIND_EVENT => lumepeer_core::constants::CHAT_MAX_BYTES,
        // Audio frames are far smaller, but one shared bound keeps the
        // parser simple; the length check happens before any allocation
        // either way.
        _ => MAX_MEDIA_FRAME_BYTES,
    }
}

/// Errors of writing or reading a recording.
#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    /// The file does not start with [`MAGIC`], or its version is newer than
    /// [`RECORD_FORMAT_VERSION`].
    #[error("not an LMRC recording of a supported version")]
    BadHeader,
    /// A record kind, length or ordering violated the format.
    #[error("corrupt record: {0}")]
    Corrupt(&'static str),
    /// Underlying I/O failed.
    #[error("i/o error: {0}")]
    Io(String),
    /// The caller asked to write a payload over the per-kind bound.
    #[error("payload too large for record kind {0}")]
    TooLarge(u8),
}

impl From<io::Error> for RecordingError {
    fn from(e: io::Error) -> Self {
        RecordingError::Io(e.to_string())
    }
}

impl From<RecordingError> for CoreError {
    fn from(_: RecordingError) -> Self {
        // Recordings never decide anything; their failures are surfaced as
        // ordinary malformed-input errors where the TCB meets them.
        CoreError::Malformed
    }
}

/// Streaming writer of an `.lmrc` file. Append-only: nothing ever seeks.
#[derive(Debug)]
pub struct RecordingWriter<W: Write> {
    /// Sink every record is appended onto.
    inner: W,
    started: bool,
    /// First record timestamp, so stored times can be relative to session
    /// start rather than absolute wall clock (§15: less metadata on disk).
    base_us: Option<u64>,
}

impl<W: Write> RecordingWriter<W> {
    /// Writes the container header immediately.
    ///
    /// # Errors
    /// [`RecordingError::Io`] when the sink fails.
    pub fn new(mut inner: W) -> Result<Self, RecordingError> {
        let mut header = Vec::with_capacity(HEADER_BYTES);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&RECORD_FORMAT_VERSION.to_be_bytes());
        header.extend_from_slice(&FLAG_RESERVED.to_be_bytes());
        inner.write_all(&header)?;
        Ok(Self {
            inner,
            started: false,
            base_us: None,
        })
    }

    fn write_record(
        &mut self,
        kind: u8,
        timestamp_us: u64,
        payload: &[u8],
    ) -> Result<(), RecordingError> {
        if payload.len() > max_payload_for(kind) {
            return Err(RecordingError::TooLarge(kind));
        }
        let base = *self.base_us.get_or_insert(timestamp_us);
        let relative = timestamp_us.saturating_sub(base);
        // The bound check above caps payload far under u32::MAX, so the
        // narrowing cannot truncate (§3.2).
        let len = u32::try_from(payload.len()).map_err(|_| RecordingError::TooLarge(kind))?;
        self.inner.write_all(&[kind, 0, 0, 0])?;
        self.inner.write_all(&relative.to_be_bytes())?;
        self.inner.write_all(&len.to_be_bytes())?;
        self.inner.write_all(payload)?;
        Ok(())
    }

    /// Appends one video frame (H.264 bitstream chunk).
    ///
    /// # Errors
    /// [`RecordingError::TooLarge`] over the media bound; I/O failures.
    pub fn write_video(
        &mut self,
        timestamp_us: u64,
        bitstream: &[u8],
    ) -> Result<(), RecordingError> {
        if bitstream.is_empty() {
            return Err(RecordingError::Corrupt("empty video record"));
        }
        self.started = true;
        self.write_record(KIND_VIDEO, timestamp_us, bitstream)
    }

    /// Appends one Opus packet; flips the header's audio flag lazily.
    ///
    /// # Errors
    /// Same as [`Self::write_video`].
    pub fn write_audio(&mut self, timestamp_us: u64, packet: &[u8]) -> Result<(), RecordingError> {
        if packet.is_empty() {
            return Err(RecordingError::Corrupt("empty audio record"));
        }
        self.started = true;
        self.write_record(KIND_AUDIO, timestamp_us, packet)
    }

    /// Appends one event-log line (JSON produced by the caller).
    ///
    /// # Errors
    /// Same as [`Self::write_video`], plus [`RecordingError::Corrupt`] for
    /// non-UTF-8 lines — the event log must stay greppable.
    pub fn write_event(
        &mut self,
        timestamp_us: u64,
        json_line: &str,
    ) -> Result<(), RecordingError> {
        self.started = true;
        self.write_record(KIND_EVENT, timestamp_us, json_line.as_bytes())
            .map_err(|e| match e {
                RecordingError::TooLarge(KIND_EVENT) => {
                    RecordingError::Corrupt("event line over the limit")
                }
                other => other,
            })
    }

    /// Flushes the sink. The format needs no trailer; an interrupted file is
    /// a valid prefix.
    ///
    /// # Errors
    /// [`RecordingError::Io`] when the sink fails.
    pub fn finish(mut self) -> Result<(), RecordingError> {
        self.inner.flush()?;
        Ok(())
    }

    /// Flushes and returns the underlying sink (tests, in-memory callers).
    ///
    /// # Errors
    /// [`RecordingError::Io`] when the flush fails.
    pub fn into_inner(mut self) -> Result<W, RecordingError> {
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// One decoded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// Video bitstream chunk.
    Video {
        /// Microseconds since the first record.
        t_us: u64,
        /// H.264 bytes.
        data: Vec<u8>,
    },
    /// Opus packet.
    Audio {
        /// Microseconds since the first record.
        t_us: u64,
        /// Codec bytes.
        data: Vec<u8>,
    },
    /// Event-log JSON line.
    Event {
        /// Microseconds since the first record.
        t_us: u64,
        /// UTF-8 JSON.
        line: String,
    },
}

/// What a finished read reports about the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Whether at least one audio record was present (derived from the
    /// records, not the reserved header flags).
    pub has_audio: bool,
    /// Format version read from the header.
    pub version: u16,
}

/// Streaming reader over the records of one `.lmrc` file.
///
/// The whole-file [`read_recording`] is written on top of this, and so is the
/// exporter (`crate::export`): an export must not have to hold a session's
/// worth of video in memory to write it back out, so the one parser everything
/// shares hands back one record at a time.
///
/// A truncated tail ends the stream instead of failing it — an interrupted
/// session must still be replayable, which is the reason the format is
/// append-only in the first place.
#[derive(Debug)]
pub struct RecordReader<R: Read> {
    inner: R,
    version: u16,
    /// Set once the stream has ended (cleanly or at a torn tail), so a caller
    /// that keeps polling gets `None` rather than a second read attempt.
    ended: bool,
}

impl<R: Read> RecordReader<R> {
    /// Reads and validates the container header.
    ///
    /// # Errors
    /// [`RecordingError::BadHeader`] for wrong magic or a version newer than
    /// [`RECORD_FORMAT_VERSION`]; [`RecordingError::Io`] otherwise.
    pub fn new(mut inner: R) -> Result<Self, RecordingError> {
        let mut header = [0u8; HEADER_BYTES];
        inner.read_exact(&mut header).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                RecordingError::BadHeader
            } else {
                RecordingError::Io(e.to_string())
            }
        })?;
        if header[0..4] != MAGIC {
            return Err(RecordingError::BadHeader);
        }
        let version = u16::from_be_bytes([header[4], header[5]]);
        if version > RECORD_FORMAT_VERSION {
            return Err(RecordingError::BadHeader);
        }
        Ok(Self {
            inner,
            version,
            ended: false,
        })
    }

    /// Format version read from the header.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Next record, or `None` at the end of the stream — including a tail torn
    /// off by an interrupted session.
    ///
    /// # Errors
    /// [`RecordingError::Corrupt`] for a bad record *in the middle* of the
    /// file (not at the trailing edge); [`RecordingError::Io`] for a read
    /// failure that is not the end of the file.
    pub fn next_record(&mut self) -> Result<Option<Record>, RecordingError> {
        if self.ended {
            return Ok(None);
        }
        let mut preamble = [0u8; RECORD_PREAMBLE_BYTES];
        match self.inner.read_exact(&mut preamble) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                // Clean end exactly on a boundary, or a torn tail after a
                // partial write: both end the stream here.
                self.ended = true;
                return Ok(None);
            }
            Err(e) => {
                self.ended = true;
                return Err(RecordingError::Io(e.to_string()));
            }
        }
        let kind = preamble[0];
        let mut ts_bytes = [0u8; 8];
        ts_bytes.copy_from_slice(&preamble[4..12]);
        let t_us = u64::from_be_bytes(ts_bytes);
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&preamble[12..16]);
        let len = u32::from_be_bytes(len_bytes) as usize;
        match kind {
            KIND_VIDEO | KIND_AUDIO | KIND_EVENT => {}
            _ => {
                self.ended = true;
                return Err(RecordingError::Corrupt("unknown record kind"));
            }
        }
        if len == 0 || len > max_payload_for(kind) {
            self.ended = true;
            return Err(RecordingError::Corrupt("record length out of bounds"));
        }
        let mut payload = vec![0u8; len];
        if let Err(e) = self.inner.read_exact(&mut payload) {
            self.ended = true;
            if e.kind() == io::ErrorKind::UnexpectedEof {
                // Torn tail mid-record: everything before it stays valid.
                return Ok(None);
            }
            return Err(RecordingError::Io(e.to_string()));
        }
        Ok(Some(match kind {
            KIND_VIDEO => Record::Video {
                t_us,
                data: payload,
            },
            KIND_AUDIO => Record::Audio {
                t_us,
                data: payload,
            },
            _ => {
                let Ok(line) = String::from_utf8(payload) else {
                    self.ended = true;
                    return Err(RecordingError::Corrupt("event line is not UTF-8"));
                };
                Record::Event { t_us, line }
            }
        }))
    }
}

/// Reads a whole recording into records. A truncated tail yields the valid
/// prefix and is reported rather than treated as corruption — an interrupted
/// session must still be replayable (the reason the format is append-only).
///
/// Buffers the whole file: callers that only stream through the records once
/// — the exporter above all — want [`RecordReader`] instead.
///
/// # Errors
/// [`RecordingError::BadHeader`] for wrong magic/version;
/// [`RecordingError::Corrupt`] for a bad record *in the middle* of the file
/// (not at the trailing edge).
pub fn read_recording<R: Read>(reader: R) -> Result<(ContainerInfo, Vec<Record>), RecordingError> {
    let mut reader = RecordReader::new(reader)?;
    let version = reader.version();
    let mut has_audio = false;
    let mut records = Vec::new();
    while let Some(record) = reader.next_record()? {
        has_audio |= matches!(record, Record::Audio { .. });
        records.push(record);
    }
    Ok((ContainerInfo { has_audio, version }, records))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::io::Cursor;

    fn t_us(r: &Record) -> u64 {
        match *r {
            Record::Video { t_us, .. }
            | Record::Audio { t_us, .. }
            | Record::Event { t_us, .. } => t_us,
        }
    }

    fn payload(r: &Record) -> &[u8] {
        match r {
            Record::Video { data, .. } | Record::Audio { data, .. } => data,
            Record::Event { line, .. } => line.as_bytes(),
        }
    }

    #[test]
    fn roundtrip_preserves_kinds_order_and_timestamps() {
        let mut w = RecordingWriter::new(Vec::new()).unwrap();
        w.write_event(1_000, r#"{"event":"session_start"}"#)
            .unwrap();
        w.write_video(5_000, b"\x00\x00\x01\x67keyframe").unwrap();
        w.write_video(45_000, b"delta").unwrap();
        w.write_audio(6_000, b"opus-packet").unwrap();
        let buffer = w.into_inner().unwrap();
        let (info, records) = read_recording(Cursor::new(buffer)).unwrap();
        assert!(info.has_audio);
        assert_eq!(info.version, RECORD_FORMAT_VERSION);
        assert_eq!(
            records,
            vec![
                Record::Event {
                    t_us: 0,
                    line: r#"{"event":"session_start"}"#.to_owned(),
                },
                Record::Video {
                    t_us: 4_000,
                    data: b"\x00\x00\x01\x67keyframe".to_vec(),
                },
                Record::Video {
                    t_us: 44_000,
                    data: b"delta".to_vec(),
                },
                Record::Audio {
                    t_us: 5_000,
                    data: b"opus-packet".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn timestamps_are_relative_to_the_first_record() {
        let mut w = RecordingWriter::new(Vec::new()).unwrap();
        w.write_video(100_000, b"a").unwrap();
        w.write_video(150_000, b"b").unwrap();
        let (_, records) = read_recording(Cursor::new(w.into_inner().unwrap())).unwrap();
        assert_eq!(t_us(&records[0]), 0);
        assert_eq!(t_us(&records[1]), 50_000);
    }

    #[test]
    fn a_truncated_tail_still_yields_the_valid_prefix() {
        let mut w = RecordingWriter::new(Vec::new()).unwrap();
        w.write_video(0, b"first").unwrap();
        w.write_video(16_000, b"second").unwrap();
        let mut buffer = w.into_inner().unwrap();
        // Simulate a crash mid-second-record: cut 3 bytes off the end.
        buffer.truncate(buffer.len() - 3);

        let (_, records) = read_recording(Cursor::new(buffer)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(payload(&records[0]), b"first");
    }

    #[test]
    fn wrong_magic_and_future_versions_are_refused() {
        let mut w = RecordingWriter::new(Vec::new()).unwrap();
        w.write_video(0, b"x").unwrap();
        let mut buffer = w.into_inner().unwrap();
        buffer[0] = b'X';
        assert!(matches!(
            read_recording(Cursor::new(buffer.clone())),
            Err(RecordingError::BadHeader)
        ));
        // A version above ours may use record shapes we would misread.
        buffer[0] = b'L';
        buffer[4] = 0x7f;
        buffer[5] = 0xff;
        assert!(matches!(
            read_recording(Cursor::new(buffer)),
            Err(RecordingError::BadHeader)
        ));
    }

    #[test]
    fn oversized_and_empty_payloads_are_refused_at_write_time() {
        let mut w = RecordingWriter::new(Vec::new()).unwrap();
        assert!(matches!(
            w.write_video(0, &[]),
            Err(RecordingError::Corrupt("empty video record"))
        ));
        let huge = vec![0u8; MAX_MEDIA_FRAME_BYTES + 1];
        assert!(matches!(
            w.write_video(0, &huge),
            Err(RecordingError::TooLarge(KIND_VIDEO))
        ));
        let long_event = "x".repeat(lumepeer_core::constants::CHAT_MAX_BYTES + 1);
        assert!(w.write_event(0, &long_event).is_err());
    }

    #[test]
    fn unknown_record_kind_in_the_middle_is_corruption_not_a_prefix() {
        let mut w = RecordingWriter::new(Vec::new()).unwrap();
        w.write_video(0, b"good").unwrap();
        w.write_video(1, b"bad-next").unwrap();
        let mut buffer = w.into_inner().unwrap();
        // Flip the second record's kind byte to something undefined.
        let second_preamble = HEADER_BYTES + RECORD_PREAMBLE_BYTES + 4;
        buffer[second_preamble] = 0x7f;
        assert!(matches!(
            read_recording(Cursor::new(buffer)),
            Err(RecordingError::Corrupt("unknown record kind"))
        ));
    }
}
