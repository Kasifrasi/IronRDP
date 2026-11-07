//! Bounded recordings of decompressed EGFX messages for offline decoder comparison.

use core::time::Duration;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;
use std::time::Instant;

const MAGIC: &[u8; 8] = b"IRGFX001";
const MAX_MESSAGE: u32 = 64 * 1024 * 1024;
const MAX_CAPTURE: u64 = 256 * 1024 * 1024;
const MAX_DURATION: Duration = Duration::from_secs(60);

/// Completion status stored at the last complete message boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureEnd {
    Closed,
    Limit,
    /// A flushed message prefix whose writer may not have closed normally.
    Checkpoint,
}

/// One complete decompressed graphics-channel message, retaining its original bytes.
pub struct CaptureMessage {
    pub elapsed: Duration,
    pub data: Vec<u8>,
}

/// Reads bounded graphics recordings without opening a network connection.
pub struct CaptureReader {
    reader: BufReader<File>,
    end: Option<CaptureEnd>,
}

impl CaptureReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        if MAX_CAPTURE < file.metadata()?.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graphics capture exceeds size limit",
            ));
        }
        let mut reader = BufReader::new(file);
        let mut magic = [0; 8];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid graphics capture header",
            ));
        }
        Ok(Self { reader, end: None })
    }

    /// Returns the next message, or the recorded end marker.
    /// A truncated file reports an error instead of implying a complete capture.
    pub fn next_message(&mut self) -> io::Result<Option<CaptureMessage>> {
        if self.end.is_some() {
            return Ok(None);
        }
        let mut length = [0; 4];
        self.reader.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length);
        let mut elapsed = [0; 8];
        self.reader.read_exact(&mut elapsed)?;
        if length == 0 {
            let mut reason = [0; 1];
            self.reader.read_exact(&mut reason)?;
            self.end = Some(match reason[0] {
                0 => CaptureEnd::Closed,
                1 => CaptureEnd::Limit,
                2 => CaptureEnd::Checkpoint,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid capture end marker")),
            });
            let mut trailing = [0; 1];
            if self.reader.read(&mut trailing)? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data after capture end marker",
                ));
            }
            return Ok(None);
        }
        if MAX_MESSAGE < length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture message exceeds allocation limit",
            ));
        }
        let mut data =
            vec![
                0;
                usize::try_from(length)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "capture message exceeds address space"))?
            ];
        self.reader.read_exact(&mut data)?;
        Ok(Some(CaptureMessage {
            elapsed: Duration::from_micros(u64::from_le_bytes(elapsed)),
            data,
        }))
    }

    pub fn end(&self) -> Option<CaptureEnd> {
        self.end
    }
}

pub(crate) struct Recorder {
    writer: Option<BufWriter<File>>,
    started: Instant,
    bytes: u64,
    byte_limit: u64,
    time_limit: Duration,
}

impl Recorder {
    pub(crate) fn from_env() -> Option<Self> {
        let path = std::env::var_os("IRONRDP_GFX_CAPTURE")?;
        match Self::create(Path::new(&path), MAX_CAPTURE, MAX_DURATION) {
            Ok(recorder) => {
                tracing::info!(path = ?path, max_bytes = MAX_CAPTURE, max_seconds = MAX_DURATION.as_secs(), "Recording graphics for offline comparison");
                Some(recorder)
            }
            Err(error) => {
                tracing::warn!(%error, "Unable to create graphics recording");
                None
            }
        }
    }

    fn create(path: &Path, byte_limit: u64, time_limit: Duration) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut writer = BufWriter::new(options.open(path)?);
        writer.write_all(MAGIC)?;
        writer.write_all(&[0; 12])?;
        writer.write_all(&[2])?;
        writer.seek(SeekFrom::Current(-13))?;
        Ok(Self {
            writer: Some(writer),
            started: Instant::now(),
            bytes: 8,
            byte_limit,
            time_limit,
        })
    }

    pub(crate) fn record(&mut self, message: &[u8]) -> io::Result<()> {
        if self.writer.is_none() {
            return Ok(());
        }
        let length = u32::try_from(message.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "graphics message exceeds recording limit"))?;
        let record_size = 12 + u64::from(length);
        if self.time_limit <= self.started.elapsed()
            || MAX_MESSAGE < length
            || self.byte_limit < self.bytes.saturating_add(record_size).saturating_add(13)
        {
            self.finish(CaptureEnd::Limit)?;
            tracing::info!(bytes = self.bytes, "Graphics recording reached its limit");
            return Ok(());
        }
        if message.is_empty() {
            return Ok(());
        }
        let elapsed = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let writer = self.writer.as_mut().expect("recording is open");
        writer.write_all(&length.to_le_bytes())?;
        writer.write_all(&elapsed.to_le_bytes())?;
        writer.write_all(message)?;
        // Keep a readable prefix when the GUI process exits before the connection thread drops.
        writer.write_all(&0u32.to_le_bytes())?;
        writer.write_all(&elapsed.to_le_bytes())?;
        writer.write_all(&[2])?;
        writer.seek(SeekFrom::Current(-13))?;
        self.bytes += record_size;
        Ok(())
    }

    pub(crate) fn finish(&mut self, reason: CaptureEnd) -> io::Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.write_all(&0u32.to_le_bytes())?;
            let elapsed = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
            writer.write_all(&elapsed.to_le_bytes())?;
            writer.write_all(&[match reason {
                CaptureEnd::Closed => 0,
                CaptureEnd::Limit => 1,
                CaptureEnd::Checkpoint => 2,
            }])?;
            writer.flush()?;
        }
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Err(error) = self.finish(CaptureEnd::Closed) {
            tracing::warn!(%error, "Unable to finish graphics recording");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    struct TestFile(std::path::PathBuf);
    impl TestFile {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            Self(std::env::temp_dir().join(format!(
                "ironrdp-gfx-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }
    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn preserves_message_bytes_and_order() -> io::Result<()> {
        // oracle: property round-trip.
        let file = TestFile::new();
        let mut recorder = Recorder::create(&file.0, 1024, Duration::from_secs(10))?;
        recorder.record(&[0, 255, 17])?;
        recorder.record(&[42, 0])?;
        recorder.finish(CaptureEnd::Closed)?;
        let mut reader = CaptureReader::open(&file.0)?;
        assert_eq!(reader.next_message()?.expect("first message").data, [0, 255, 17]);
        assert_eq!(reader.next_message()?.expect("second message").data, [42, 0]);
        assert!(reader.next_message()?.is_none());
        assert_eq!(reader.end(), Some(CaptureEnd::Closed));
        Ok(())
    }

    #[test]
    fn byte_limit_stops_before_partial_message() -> io::Result<()> {
        // oracle: reference hand-derived budget: 8-byte header + 15-byte message + 13-byte footer = 36.
        let file = TestFile::new();
        let mut recorder = Recorder::create(&file.0, 36, Duration::from_secs(10))?;
        recorder.record(&[1, 2, 3])?;
        recorder.record(&[4])?;
        let mut reader = CaptureReader::open(&file.0)?;
        assert_eq!(reader.next_message()?.expect("complete message").data, [1, 2, 3]);
        assert!(reader.next_message()?.is_none());
        assert_eq!(reader.end(), Some(CaptureEnd::Limit));
        Ok(())
    }

    #[test]
    fn zero_duration_stops_without_message() -> io::Result<()> {
        // oracle: reference an elapsed-time limit of zero permits no message.
        let file = TestFile::new();
        let mut recorder = Recorder::create(&file.0, 1024, Duration::ZERO)?;
        recorder.record(&[1])?;
        let mut reader = CaptureReader::open(&file.0)?;
        assert!(reader.next_message()?.is_none());
        assert_eq!(reader.end(), Some(CaptureEnd::Limit));
        Ok(())
    }

    #[test]
    fn missing_footer_is_reported_as_truncation() -> io::Result<()> {
        // oracle: spec Rust std::io::Read::read_exact reports UnexpectedEof on a short input.
        let file = TestFile::new();
        std::fs::write(&file.0, MAGIC)?;
        let mut reader = CaptureReader::open(&file.0)?;
        assert_eq!(
            reader.next_message().err().expect("truncated recording").kind(),
            io::ErrorKind::UnexpectedEof
        );
        Ok(())
    }
    #[test]
    fn refuses_to_overwrite_existing_file() -> io::Result<()> {
        // oracle: spec Rust std::fs::OpenOptions::create_new preserves an existing file.
        let file = TestFile::new();
        std::fs::write(&file.0, [7, 8, 9])?;
        let error = Recorder::create(&file.0, 1024, Duration::from_secs(10))
            .err()
            .expect("existing file");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&file.0)?, [7, 8, 9]);
        Ok(())
    }
    #[test]
    fn message_checkpoint_is_readable_before_close() -> io::Result<()> {
        // oracle: property round-trip.
        let file = TestFile::new();
        let mut recorder = Recorder::create(&file.0, 1024, Duration::from_secs(10))?;
        recorder.record(&[11, 22, 33])?;
        let mut reader = CaptureReader::open(&file.0)?;
        assert_eq!(reader.next_message()?.expect("flushed message").data, [11, 22, 33]);
        assert!(reader.next_message()?.is_none());
        assert_eq!(reader.end(), Some(CaptureEnd::Checkpoint));
        recorder.record(&[44])?;
        recorder.finish(CaptureEnd::Closed)?;
        let mut reader = CaptureReader::open(&file.0)?;
        assert_eq!(reader.next_message()?.expect("first message").data, [11, 22, 33]);
        assert_eq!(reader.next_message()?.expect("second message").data, [44]);
        assert!(reader.next_message()?.is_none());
        assert_eq!(reader.end(), Some(CaptureEnd::Closed));
        Ok(())
    }
}
