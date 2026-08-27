//! Export of an `.lmrc` recording into files a player can open (§9.2, §17;
//! ADR 0031).
//!
//! [`crate::record`] was written with "MKV export as a later, separate step"
//! in its header. This is that step, minus the MKV: no Matroska muxer exists
//! that passes the workspace's `cargo deny` licence and supply-chain policy,
//! so the export writes the two elementary streams the recording already
//! holds, each in the plainest container that makes it playable — H.264 as an
//! Annex-B elementary stream (`.h264`), Opus in Ogg (`.opus`). ADR 0031
//! records why, and what would change if a muxer ever becomes available.
//!
//! Streaming throughout: the source is read one record at a time through
//! [`crate::record::RecordReader`] and each payload is written out before the
//! next is read, so exporting an hour-long session costs one record of memory
//! rather than the whole file. Event records are dropped — they are the
//! session's action log (§15), not a media track, and a player has nowhere to
//! put them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use lumepeer_core::constants::AUDIO_SAMPLE_RATE_HZ;

use crate::record::{Record, RecordReader, RecordingError};

/// Extension of the exported video elementary stream.
pub const VIDEO_EXTENSION: &str = "h264";
/// Extension of the exported Ogg-encapsulated Opus stream.
pub const AUDIO_EXTENSION: &str = "opus";

/// What one export produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Video records written to the elementary stream.
    pub video_frames: u64,
    /// Opus packets written to the Ogg stream.
    pub audio_packets: u64,
    /// Event records skipped: they belong to the action log, not to a track.
    pub events_skipped: u64,
}

impl ExportSummary {
    /// Whether anything playable came out at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.video_frames == 0 && self.audio_packets == 0
    }
}

/// Files one export wrote. A track with no records leaves no file behind
/// rather than an empty one a player would refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutput {
    /// Annex-B H.264 elementary stream, if the recording carried video.
    pub video: Option<PathBuf>,
    /// Ogg Opus stream, if the recording carried audio.
    pub audio: Option<PathBuf>,
    /// Counts of what was written.
    pub summary: ExportSummary,
}

/// Exports the recording at `source` next to it, reusing its file stem.
///
/// Both tracks are written into `out_dir`; a track the recording does not
/// carry produces no file. A torn tail is not an error — the valid prefix is
/// exported, exactly as [`crate::record::RecordReader`] replays it.
///
/// # Errors
/// [`RecordingError::BadHeader`] when `source` is not a supported recording,
/// [`RecordingError::Corrupt`] for damage in the middle of the file, and
/// [`RecordingError::Io`] for any read or write failure.
pub fn export_file(source: &Path, out_dir: &Path) -> Result<ExportOutput, RecordingError> {
    let stem = source
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or(RecordingError::Corrupt("recording path has no file name"))?
        .to_owned();
    let file = std::fs::File::open(source).map_err(|e| RecordingError::Io(e.to_string()))?;
    std::fs::create_dir_all(out_dir).map_err(|e| RecordingError::Io(e.to_string()))?;

    let video_path = out_dir.join(format!("{stem}.{VIDEO_EXTENSION}"));
    let audio_path = out_dir.join(format!("{stem}.{AUDIO_EXTENSION}"));
    // Opened lazily: a video-only session must not leave an unplayable
    // zero-length `.opus` next to its picture, and vice versa.
    let mut video = LazyFile::new(video_path.clone());
    let mut audio = LazyFile::new(audio_path.clone());

    let summary = export_streams(
        std::io::BufReader::new(file),
        &mut video,
        &mut audio,
        AUDIO_CHANNELS_EXPORTED,
    )?;

    // A failed export must not leave half a file claiming to be a recording.
    let video_written = video.finish()?;
    let audio_written = audio.finish()?;
    Ok(ExportOutput {
        video: video_written.then_some(video_path),
        audio: audio_written.then_some(audio_path),
        summary,
    })
}

/// Channel count declared in the exported `OpusHead`.
///
/// The session encodes at [`lumepeer_core::constants::AUDIO_CHANNELS`]; the
/// export declares the same rather than guessing per packet, because nothing
/// in the container records a mid-session channel change and the encoder
/// cannot produce one.
const AUDIO_CHANNELS_EXPORTED: u8 = lumepeer_core::constants::AUDIO_CHANNELS;

/// Exports one recording into an already-open video sink and audio sink.
///
/// Split out from [`export_file`] so the whole export is testable against
/// in-memory buffers, and so a caller that wants the streams somewhere other
/// than two files does not have to go through the filesystem.
///
/// # Errors
/// Same as [`export_file`].
pub fn export_streams<R: Read, V: Write, A: Write>(
    source: R,
    video: &mut V,
    audio: &mut A,
    channels: u8,
) -> Result<ExportSummary, RecordingError> {
    let mut reader = RecordReader::new(source)?;
    let mut summary = ExportSummary::default();
    let mut ogg: Option<OggOpusWriter> = None;
    while let Some(record) = reader.next_record()? {
        match record {
            Record::Video { data, .. } => {
                // Annex-B chunks concatenated in capture order *are* the
                // elementary stream: every chunk already carries its own
                // start codes, so no framing has to be invented here.
                video.write_all(&data)?;
                summary.video_frames += 1;
            }
            Record::Audio { data, .. } => {
                let writer = match ogg.as_mut() {
                    Some(writer) => writer,
                    None => ogg.insert(OggOpusWriter::begin(audio, channels)?),
                };
                writer.write_packet(audio, &data)?;
                summary.audio_packets += 1;
            }
            Record::Event { .. } => summary.events_skipped += 1,
        }
    }
    if let Some(writer) = ogg {
        writer.finish(audio)?;
    }
    Ok(summary)
}

/// A file created on first write. Keeps [`export_file`] from leaving an empty
/// track behind for a recording that never carried it.
#[derive(Debug)]
struct LazyFile {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl LazyFile {
    const fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }

    /// Flushes and reports whether anything was ever written.
    fn finish(mut self) -> Result<bool, RecordingError> {
        match self.file.as_mut() {
            Some(file) => {
                file.flush()
                    .map_err(|e| RecordingError::Io(e.to_string()))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl Write for LazyFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let file = match self.file.as_mut() {
            Some(file) => file,
            None => self.file.insert(std::fs::File::create(&self.path)?),
        };
        file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Ogg encapsulation of the Opus track (RFC 3533 pages, RFC 7845 headers).
// ---------------------------------------------------------------------------

/// Stream serial number of every exported Opus stream.
///
/// Fixed rather than random: an export of the same recording must produce the
/// same bytes twice (it is a conversion, not a new stream), and one logical
/// stream per file means a serial has nothing to disambiguate against.
const OGG_SERIAL: u32 = u32::from_be_bytes(*b"LMRC");

/// `header_type` bit: first page of the logical stream.
const OGG_FLAG_BOS: u8 = 0x02;
/// `header_type` bit: last page of the logical stream.
const OGG_FLAG_EOS: u8 = 0x04;

/// Largest lacing value; anything longer is split across segments (RFC 3533).
const OGG_MAX_SEGMENT: usize = 255;
/// Segments one page can carry.
const OGG_MAX_SEGMENTS_PER_PAGE: usize = 255;

/// Version byte of the `OpusHead` packet (RFC 7845 §5.1).
const OPUS_HEAD_VERSION: u8 = 1;
/// Channel mapping family 0: mono or stereo, no mapping table.
const OPUS_MAPPING_FAMILY_RTP: u8 = 0;
/// Vendor string of the `OpusTags` packet: the exporter, not the encoder.
const OPUS_TAGS_VENDOR: &str = "lumepeer";

/// Writes an Opus track into Ogg pages as packets arrive.
///
/// One packet per page. That is not the densest possible packing, but it
/// keeps the writer streaming — a page can be emitted the moment its packet
/// is read, without buffering ahead to fill it — which is the property the
/// export was asked for.
#[derive(Debug)]
struct OggOpusWriter {
    sequence: u32,
    /// Samples at 48 kHz decoded so far: the granule position of the newest
    /// page (RFC 7845 §4).
    granule: u64,
    /// Newest packet, held back so the last one can carry the EOS flag.
    pending: Option<Vec<u8>>,
}

impl OggOpusWriter {
    /// Emits the two mandatory header pages.
    ///
    /// The sink is passed per call rather than held: the audio track is
    /// discovered mid-stream, and a writer that borrowed the sink for the rest
    /// of the export would keep the video path from using it.
    fn begin<W: Write>(sink: &mut W, channels: u8) -> Result<Self, RecordingError> {
        let mut writer = Self {
            sequence: 0,
            granule: 0,
            pending: None,
        };
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(OPUS_HEAD_VERSION);
        head.push(channels);
        // Pre-skip 0: the container records no encoder delay, and inventing
        // one would trim audio the recording actually contains (§24.5:
        // degrade honestly rather than silently).
        head.extend_from_slice(&0u16.to_le_bytes());
        head.extend_from_slice(&AUDIO_SAMPLE_RATE_HZ.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(OPUS_MAPPING_FAMILY_RTP);
        writer.write_page(sink, &head, OGG_FLAG_BOS, 0)?;

        let vendor = OPUS_TAGS_VENDOR.as_bytes();
        let mut tags = Vec::with_capacity(16 + vendor.len());
        tags.extend_from_slice(b"OpusTags");
        // Bounded by the constant above, so the narrowing cannot truncate.
        let vendor_len = u32::try_from(vendor.len())
            .map_err(|_| RecordingError::Corrupt("vendor string too long"))?;
        tags.extend_from_slice(&vendor_len.to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments
        writer.write_page(sink, &tags, 0, 0)?;
        Ok(writer)
    }

    /// Queues one Opus packet, emitting the one queued before it.
    fn write_packet<W: Write>(
        &mut self,
        sink: &mut W,
        packet: &[u8],
    ) -> Result<(), RecordingError> {
        if let Some(previous) = self.pending.take() {
            self.emit(sink, &previous, false)?;
        }
        self.pending = Some(packet.to_vec());
        Ok(())
    }

    /// Emits the last queued packet with the end-of-stream flag set.
    fn finish<W: Write>(mut self, sink: &mut W) -> Result<(), RecordingError> {
        if let Some(last) = self.pending.take() {
            self.emit(sink, &last, true)?;
        }
        sink.flush()?;
        Ok(())
    }

    fn emit<W: Write>(
        &mut self,
        sink: &mut W,
        packet: &[u8],
        last: bool,
    ) -> Result<(), RecordingError> {
        // An unreadable TOC would desynchronize every later granule position,
        // so the packet is refused rather than guessed at.
        self.granule = self
            .granule
            .saturating_add(u64::from(packet_samples_48k(packet)?));
        let granule = self.granule;
        self.write_page(sink, packet, if last { OGG_FLAG_EOS } else { 0 }, granule)
    }

    fn write_page<W: Write>(
        &mut self,
        sink: &mut W,
        packet: &[u8],
        flags: u8,
        granule: u64,
    ) -> Result<(), RecordingError> {
        let mut lacing = Vec::new();
        let mut left = packet.len();
        loop {
            let value = left.min(OGG_MAX_SEGMENT);
            lacing.push(u8::try_from(value).unwrap_or(u8::MAX));
            left -= value;
            if value < OGG_MAX_SEGMENT {
                break;
            }
        }
        if lacing.len() > OGG_MAX_SEGMENTS_PER_PAGE {
            // Only reachable for a packet over ~64 KiB, which the media path's
            // own bound already rules out; refused rather than mis-framed.
            return Err(RecordingError::Corrupt("packet too large for one page"));
        }
        let mut page = Vec::with_capacity(27 + lacing.len() + packet.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // stream structure version
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&OGG_SERIAL.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // checksum, filled below
        page.push(u8::try_from(lacing.len()).unwrap_or(u8::MAX));
        page.extend_from_slice(&lacing);
        page.extend_from_slice(packet);
        let checksum = ogg_crc32(&page).to_le_bytes();
        // Offset 22..26 is the checksum field, always present: the header is
        // 27 bytes and `page` already holds all of it.
        page[22..26].copy_from_slice(&checksum);
        sink.write_all(&page)?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }
}

/// CRC-32 of an Ogg page: polynomial 0x04c11db7, no reflection, zero init and
/// no final inversion (RFC 3533 §6). Not the CRC-32 of zlib, and not
/// interchangeable with it.
fn ogg_crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x04c1_1db7
            };
        }
    }
    crc
}

/// Samples at 48 kHz one Opus packet decodes to, from its TOC byte
/// (RFC 6716 §3.1).
///
/// Returns an error rather than a guess for a packet that violates the TOC
/// rules: this runs over the contents of a file that may have been truncated
/// or tampered with, and a wrong duration would silently skew every later
/// timestamp in the exported stream.
fn packet_samples_48k(packet: &[u8]) -> Result<u32, RecordingError> {
    let toc = *packet
        .first()
        .ok_or(RecordingError::Corrupt("empty opus packet"))?;
    let config = usize::from(toc >> 3);
    // Frame length per configuration, in 48 kHz samples.
    let frame_samples: u32 = match config {
        // SILK-only: 10, 20, 40, 60 ms, repeated per bandwidth.
        0..=11 => match config % 4 {
            0 => 480,
            1 => 960,
            2 => 1920,
            _ => 2880,
        },
        // Hybrid: 10 and 20 ms.
        12..=15 => {
            if config % 2 == 0 {
                480
            } else {
                960
            }
        }
        // CELT-only: 2.5, 5, 10, 20 ms.
        16..=31 => match config % 4 {
            0 => 120,
            1 => 240,
            2 => 480,
            _ => 960,
        },
        // `toc >> 3` of a u8 cannot exceed 31.
        _ => return Err(RecordingError::Corrupt("impossible opus configuration")),
    };
    let frames: u32 = match toc & 0b11 {
        0 => 1,
        1 | 2 => 2,
        _ => {
            let count = packet
                .get(1)
                .ok_or(RecordingError::Corrupt("truncated opus frame count"))?
                & 0b0011_1111;
            if count == 0 {
                return Err(RecordingError::Corrupt("opus packet with zero frames"));
            }
            u32::from(count)
        }
    };
    let samples = frame_samples.saturating_mul(frames);
    // RFC 6716 §3.1: a packet may not exceed 120 ms of audio.
    if samples > AUDIO_SAMPLE_RATE_HZ * 120 / 1000 {
        return Err(RecordingError::Corrupt("opus packet longer than 120 ms"));
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::record::RecordingWriter;

    /// One 20 ms stereo CELT packet: config 31 (`0b11111`), one frame.
    fn opus_packet(payload: u8) -> Vec<u8> {
        vec![(31 << 3) | 0b100, payload, payload]
    }

    fn recording_with(video: &[&[u8]], audio: &[Vec<u8>], events: usize) -> Vec<u8> {
        let mut writer = RecordingWriter::new(Vec::new()).unwrap();
        let mut t = 0;
        for chunk in video {
            writer.write_video(t, chunk).unwrap();
            t += 16_000;
        }
        for packet in audio {
            writer.write_audio(t, packet).unwrap();
            t += 20_000;
        }
        for i in 0..events {
            writer
                .write_event(t, &format!(r#"{{"event":"e{i}"}}"#))
                .unwrap();
        }
        writer.into_inner().unwrap()
    }

    #[test]
    fn video_records_concatenate_into_one_annex_b_stream() {
        let source = recording_with(
            &[b"\x00\x00\x00\x01\x67sps", b"\x00\x00\x00\x01\x65idr"],
            &[],
            1,
        );
        let mut video = Vec::new();
        let mut audio = Vec::new();
        let summary = export_streams(source.as_slice(), &mut video, &mut audio, 2).unwrap();

        assert_eq!(summary.video_frames, 2);
        assert_eq!(summary.audio_packets, 0);
        assert_eq!(summary.events_skipped, 1);
        assert_eq!(video, b"\x00\x00\x00\x01\x67sps\x00\x00\x00\x01\x65idr");
        // No audio records means no Ogg headers: an empty stream is not a
        // valid Opus file, so nothing is written at all.
        assert!(audio.is_empty());
    }

    #[test]
    fn audio_records_become_a_playable_ogg_opus_stream() {
        let packets = vec![opus_packet(1), opus_packet(2), opus_packet(3)];
        let source = recording_with(&[], &packets, 0);
        let mut video = Vec::new();
        let mut audio = Vec::new();
        let summary = export_streams(source.as_slice(), &mut video, &mut audio, 2).unwrap();

        assert_eq!(summary.audio_packets, 3);
        assert!(video.is_empty());
        // Two header pages plus one page per packet, each starting "OggS".
        let pages: Vec<usize> = audio
            .windows(4)
            .enumerate()
            .filter_map(|(i, w)| (w == b"OggS").then_some(i))
            .collect();
        assert_eq!(pages.len(), 5, "two headers plus three audio pages");
        assert_eq!(&audio[28..36], b"OpusHead");
        assert_eq!(audio[5], OGG_FLAG_BOS, "first page opens the stream");
        // The last page ends the stream, and its granule position is the whole
        // duration: three 20 ms packets at 48 kHz.
        let last = *pages.last().unwrap();
        assert_eq!(audio[last + 5], OGG_FLAG_EOS);
        let mut granule = [0u8; 8];
        granule.copy_from_slice(&audio[last + 6..last + 14]);
        assert_eq!(u64::from_le_bytes(granule), 3 * 960);
    }

    #[test]
    fn every_page_carries_the_checksum_a_demuxer_recomputes() {
        let source = recording_with(&[], &[opus_packet(7)], 0);
        let mut audio = Vec::new();
        export_streams(source.as_slice(), &mut Vec::new(), &mut audio, 2).unwrap();

        let mut offset = 0;
        let mut pages = 0;
        while offset < audio.len() {
            let segments = usize::from(audio[offset + 26]);
            let body: usize = audio[offset + 27..offset + 27 + segments]
                .iter()
                .map(|&v| usize::from(v))
                .sum();
            let end = offset + 27 + segments + body;
            let mut page = audio[offset..end].to_vec();
            let stored = u32::from_le_bytes([page[22], page[23], page[24], page[25]]);
            page[22..26].copy_from_slice(&0u32.to_le_bytes());
            assert_eq!(ogg_crc32(&page), stored, "page {pages} checksum");
            offset = end;
            pages += 1;
        }
        assert_eq!(pages, 3);
    }

    #[test]
    fn a_truncated_recording_exports_its_valid_prefix() {
        let mut source = recording_with(&[b"first-frame", b"second-frame"], &[], 0);
        source.truncate(source.len() - 4);
        let mut video = Vec::new();
        let summary = export_streams(source.as_slice(), &mut video, &mut Vec::new(), 2).unwrap();

        assert_eq!(summary.video_frames, 1);
        assert_eq!(video, b"first-frame");
    }

    #[test]
    fn a_packet_with_an_unreadable_toc_stops_the_export() {
        // Code 3 (`toc & 0b11 == 3`) needs a frame-count byte; without one the
        // duration is unknowable and every later granule would be wrong.
        let source = recording_with(&[], &[vec![(31 << 3) | 0b11]], 0);
        let error = export_streams(source.as_slice(), &mut Vec::new(), &mut Vec::new(), 2);
        assert!(matches!(
            error,
            Err(RecordingError::Corrupt("truncated opus frame count"))
        ));
    }

    #[test]
    fn packet_durations_follow_the_toc_configuration() {
        // SILK 60 ms, hybrid 20 ms, CELT 2.5 ms, and two-frame code 1: a
        // wideband CELT packet of two 2.5 ms frames (config 20).
        assert_eq!(packet_samples_48k(&[3 << 3, 0]).unwrap(), 2880);
        assert_eq!(packet_samples_48k(&[13 << 3, 0]).unwrap(), 960);
        assert_eq!(packet_samples_48k(&[16 << 3, 0]).unwrap(), 120);
        assert_eq!(packet_samples_48k(&[(20 << 3) | 1, 0]).unwrap(), 2 * 120);
        assert!(packet_samples_48k(&[]).is_err());
    }

    #[test]
    fn exporting_a_file_writes_only_the_tracks_it_carries() {
        let dir = std::env::temp_dir().join(format!("lumepeer-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("session.lmrc");
        std::fs::write(&source, recording_with(&[b"only-video"], &[], 2)).unwrap();

        let out = export_file(&source, &dir).unwrap();
        assert_eq!(out.summary.video_frames, 1);
        assert_eq!(out.summary.events_skipped, 2);
        assert_eq!(out.video, Some(dir.join("session.h264")));
        assert_eq!(out.audio, None);
        assert!(!dir.join("session.opus").exists());
        assert_eq!(
            std::fs::read(dir.join("session.h264")).unwrap(),
            b"only-video"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
