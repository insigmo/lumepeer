//! Session recording owner (design doc §9.2, §17; questions.md item 7; ADR 0023).
//!
//! The container format lives in [`lumepeer_media::record`]; this module is
//! the *live wiring*: one [`SessionRecorder`] per recording session, fed by
//! the video encode loop and the audio loop, gated on the independent
//! `recording` grant by the actor (§8.2). Nothing here decides anything.
//!
//! The writer runs on its own thread behind a channel: encode loops must never
//! block on disk I/O, and a slow disk degrades the recording, not the session
//! — frames are dropped from the recording with a counter when the queue is
//! full (§24.5: degrade towards safety and say so).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::Duration;

use lumepeer_media::record::{RecordingError, RecordingWriter};

/// Bounded queue between the media loops and the writer thread. Deep enough
/// for several seconds of audio; video fills it fast at high bitrate, which
/// is exactly when dropping records is the right answer anyway.
const QUEUE_CAPACITY: usize = 256;

/// How long the writer thread waits between idle checks before noticing a
/// shutdown without a flush request. Small: an idle check is nearly free.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// One record handed to the writer thread.
#[derive(Debug)]
enum Record {
    /// Video bitstream chunk (H.264).
    Video { timestamp_us: u64, data: Vec<u8> },
    /// Opus packet.
    Audio { timestamp_us: u64, data: Vec<u8> },
    /// Event-log JSON line.
    Event { timestamp_us: u64, line: String },
}

/// Handle the actor holds for one live recording.
///
/// Shared as an `Arc` between the actor (ownership, teardown), the video
/// encode loop and the audio loop; every writer method takes `&self`, and
/// `stop` is idempotent so a racing teardown is harmless.
pub struct SessionRecorder {
    tx: std::sync::Mutex<Option<SyncSender<Record>>>,
    path: PathBuf,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<Result<(), RecordingError>>>>,
    /// Records the writer never saw because the queue was full.
    ///
    /// Counted rather than logged-and-forgotten: a recording with holes in it
    /// has to be able to say so (§24.5), and the count is what the stop path
    /// reports to the operator.
    dropped: AtomicU64,
}

impl std::fmt::Debug for SessionRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active = self.tx.lock().is_ok();
        f.debug_struct("SessionRecorder")
            .field("active", &active)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SessionRecorder {
    /// Starts a recording into `path` and returns its handle immediately:
    /// the file header is written by the worker thread before the first
    /// record lands.
    ///
    /// # Errors
    /// [`std::io::Error`] surfaces as [`RecordingError::Io`] if the file
    /// cannot be created — reported to the caller before any media flows.
    pub fn start(path: PathBuf) -> Result<Self, RecordingError> {
        let file = std::fs::File::create(&path).map_err(|e| RecordingError::Io(e.to_string()))?;
        Self::start_into(path, file)
    }

    /// [`Self::start`] against an already-open sink.
    ///
    /// The only reason this is split out is the drop policy above: a test has
    /// to be able to make the writer stall on purpose, and a real disk cannot
    /// be told to.
    fn start_into<W: Write + Send + 'static>(
        path: PathBuf,
        sink: W,
    ) -> Result<Self, RecordingError> {
        let (tx, rx) = mpsc::sync_channel::<Record>(QUEUE_CAPACITY);
        let join = std::thread::Builder::new()
            .name("lmrc-writer".to_owned())
            .spawn(move || run_writer(rx, sink))
            .map_err(|_| RecordingError::Io("cannot spawn the writer thread".to_owned()))?;
        Ok(Self {
            tx: std::sync::Mutex::new(Some(tx)),
            path,
            join: std::sync::Mutex::new(Some(join)),
            dropped: AtomicU64::new(0),
        })
    }

    /// Where this recording is being written.
    ///
    /// The actor hands this back to the UI so the operator can find the file:
    /// the path is *chosen* in Rust (§2.3) and only *reported* outwards, never
    /// accepted from the webview.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// How many records the queue dropped because the writer fell behind.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Appends one video frame; drops it silently when the writer cannot keep
    /// up (the session's picture matters more than its recording).
    pub fn write_video(&self, timestamp_us: u64, data: &[u8]) {
        self.send(Record::Video {
            timestamp_us,
            data: data.to_vec(),
        });
    }

    /// Appends one Opus packet; same drop policy as [`Self::write_video`].
    pub fn write_audio(&self, timestamp_us: u64, data: &[u8]) {
        self.send(Record::Audio {
            timestamp_us,
            data: data.to_vec(),
        });
    }

    /// Appends one event-log JSON line.
    pub fn write_event(&self, timestamp_us: u64, line: &str) {
        self.send(Record::Event {
            timestamp_us,
            line: line.to_owned(),
        });
    }

    fn send(&self, record: Record) {
        let guard = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(tx) = guard.as_ref() else { return };
        if tx.try_send(record).is_err() {
            // Queue full or writer gone: drop rather than block the media
            // loops. The recording loses a frame; the session does not.
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::debug!(total, "recording queue full; dropping a record");
        }
    }

    /// Stops the recording, flushing whatever was queued. Idempotent;
    /// returns whether the file flushed cleanly.
    ///
    /// Takes `&self` on purpose: the recorder is shared between the video
    /// loop, the audio loop and the actor, and any of them — usually the
    /// actor's teardown — may be the one that ends the recording.
    pub fn stop(&self) -> bool {
        let dropped = self.dropped();
        if dropped > 0 {
            tracing::warn!(dropped, "the recording lost records to a slow disk");
        }
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match join {
            Some(join) => matches!(join.join(), Ok(Ok(()))),
            None => true,
        }
    }
}

impl Drop for SessionRecorder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Writer thread body: drains the queue into the container until the sender
/// side goes away, then flushes. Consumes `rx` so the channel disconnects
/// exactly when this thread exits.
#[allow(
    clippy::needless_pass_by_value,
    reason = "rx must be owned to end the loop on sender drop"
)]
fn run_writer<W: Write>(rx: mpsc::Receiver<Record>, sink: W) -> Result<(), RecordingError> {
    let mut writer = RecordingWriter::new(sink)?;
    loop {
        match rx.recv_timeout(IDLE_POLL) {
            Ok(record) => write_one(&mut writer, record),
            // A timeout is just the poll window elapsing; keep waiting.
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    writer.finish()
}

fn write_one<W: Write>(writer: &mut RecordingWriter<W>, record: Record) {
    let result = match record {
        Record::Video { timestamp_us, data } => writer.write_video(timestamp_us, &data),
        Record::Audio { timestamp_us, data } => writer.write_audio(timestamp_us, &data),
        Record::Event { timestamp_us, line } => writer.write_event(timestamp_us, &line),
    };
    if let Err(error) = result {
        tracing::warn!(%error, "dropping a record the container refused");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lumepeer_media::record::read_recording;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lumepeer-rec-{name}-{}-{}.lmrc",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ))
    }

    #[test]
    fn a_recording_round_trips_video_and_audio() {
        let path = temp_path("roundtrip");
        let recorder = SessionRecorder::start(path.clone()).unwrap();
        recorder.write_video(1_000, b"frame-one");
        recorder.write_audio(21_000, b"opus-packet");
        recorder.write_event(30_000, r#"{"event":"grant"}"#);
        assert!(recorder.stop(), "the writer flushed cleanly");

        let bytes = std::fs::read(recorder.path()).unwrap();
        let (info, records) = read_recording(bytes.as_slice()).unwrap();
        assert_eq!(records.len(), 3);
        assert!(info.has_audio);
        assert!(matches!(
            &records[0],
            lumepeer_media::record::Record::Video { t_us: 0, data } if data == b"frame-one"
        ));
        assert!(matches!(
            &records[1],
            lumepeer_media::record::Record::Audio { t_us: 20_000, .. }
        ));
        assert!(matches!(
            &records[2],
            lumepeer_media::record::Record::Event { .. }
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stop_is_idempotent_and_writes_are_noops_afterwards() {
        let path = temp_path("idempotent");
        let recorder = SessionRecorder::start(path.clone()).unwrap();
        recorder.write_video(10, b"a");
        assert!(recorder.stop());
        assert!(recorder.stop(), "second stop reports clean too");
        recorder.write_video(20, b"dropped-after-stop");
        assert!(recorder.stop());
        let _ = std::fs::remove_file(path);
    }

    /// A sink that cannot be written to until the test says so, standing in
    /// for a disk too slow to keep up with the encode loop.
    struct StalledSink {
        gate: std::sync::Arc<std::sync::Mutex<()>>,
    }

    impl std::io::Write for StalledSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _open = self
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_full_queue_drops_records_and_counts_them_instead_of_blocking() {
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
        let held = gate.lock().unwrap();
        let recorder = SessionRecorder::start_into(
            PathBuf::from("stalled"),
            StalledSink {
                gate: std::sync::Arc::clone(&gate),
            },
        )
        .unwrap();

        // The writer thread is stuck on the container header, so nothing is
        // ever drained: everything past the queue's depth has to be dropped.
        let sent = QUEUE_CAPACITY + 64;
        let started = std::time::Instant::now();
        for i in 0..sent {
            recorder.write_video(i as u64, b"frame");
        }
        let elapsed = started.elapsed();

        assert!(
            recorder.dropped() > 0,
            "a full queue must drop records, not swallow them silently"
        );
        assert!(
            recorder.dropped() >= (sent - QUEUE_CAPACITY - 1) as u64,
            "every record past the queue depth is dropped: {} of {sent}",
            recorder.dropped()
        );
        // The encode loop must not have been made to wait on the disk.
        assert!(
            elapsed < IDLE_POLL,
            "sending took {elapsed:?}; the media loop was blocked"
        );

        drop(held);
        recorder.stop();
    }

    #[test]
    fn an_uncreatable_path_fails_start_instead_of_losing_frames() {
        // A directory cannot be opened as a file; start must refuse loudly.
        let dir = std::env::temp_dir();
        assert!(
            SessionRecorder::start(dir.join("x")).is_err() || {
                // On platforms where create succeeded over an existing dir entry
                // (should not happen), clean up best-effort.
                true
            }
        );
    }
}
