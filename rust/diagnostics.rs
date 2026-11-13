use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
    thread::{self, JoinHandle},
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use serde::Serialize;
use windows::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetSystemTime};

use crate::{
    engine::{Processing, Status},
    latency::Latency,
};

const FILE_LIMIT: u64 = 256 * 1024;
const FILES: [&str; 3] = ["diagnostics.log", "diagnostics.1.log", "diagnostics.2.log"];

pub(crate) fn directory() -> anyhow::Result<PathBuf> {
    Ok(crate::preferences::profile_path()?.with_file_name("logs"))
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event {
    SessionStarted {
        version: &'static str,
        processing: Processing,
    },
    RouteOpened {
        batch_ms: usize,
        wasapi: &'static str,
        capture_period_frames: usize,
        render_period_frames: usize,
        render_rate_hz: usize,
        render_period_ms: f64,
    },
    RouteInterrupted {
        reason: &'static str,
        action: &'static str,
        batch_ms_before: usize,
        batch_ms_after: usize,
        retry_delay_ms: Option<u32>,
        #[serde(flatten)]
        counters: Counters,
    },
    Timing {
        #[serde(flatten)]
        counters: Counters,
    },
    LatencyMeasurement {
        id: u64,
        outcome: &'static str,
        result: Option<Latency>,
    },
    SessionEnded {
        outcome: &'static str,
        dropped_events: usize,
        #[serde(flatten)]
        counters: Counters,
    },
}

#[derive(Serialize)]
pub(crate) struct Counters {
    pub(crate) underruns: usize,
    pub(crate) batch_deadline_misses: usize,
    recoveries: usize,
    inference_p99_ms: Option<usize>,
    queue_wait_p99_ms: Option<usize>,
    batch_p99_ms: Option<usize>,
}

impl From<&Status> for Counters {
    fn from(status: &Status) -> Self {
        Self {
            underruns: status.underruns,
            batch_deadline_misses: status.batch_deadline_misses,
            recoveries: status.recoveries,
            inference_p99_ms: status.inference_p99_ms,
            queue_wait_p99_ms: status.queue_wait_p99_ms,
            batch_p99_ms: status.batch_p99_ms,
        }
    }
}

type Entry = (SYSTEMTIME, Event);

pub(crate) struct Recorder {
    writer: Option<(Sender<Entry>, JoinHandle<io::Result<()>>)>,
    dropped: AtomicUsize,
}

impl Recorder {
    pub(crate) fn start(processing: &Processing) -> Self {
        let (sender, receiver) = bounded(64);
        let started = utc_now();
        let recorder = Self {
            writer: thread::Builder::new()
                .name("diagnostic-log".into())
                .spawn(move || write_session(receiver, started))
                .ok()
                .map(|writer| (sender, writer)),
            dropped: AtomicUsize::new(0),
        };
        recorder.record(Event::SessionStarted {
            version: env!("CARGO_PKG_VERSION"),
            processing: processing.clone(),
        });
        recorder
    }

    pub(crate) fn record(&self, event: Event) {
        if let Some((sender, _)) = &self.writer
            && let Err(TrySendError::Full(_)) = sender.try_send((utc_now(), event))
        {
            self.dropped.fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn finish(self, outcome: &'static str, status: &Status) {
        if let Some((sender, writer)) = self.writer {
            let _ = sender.send((
                utc_now(),
                Event::SessionEnded {
                    outcome,
                    dropped_events: self.dropped.load(Relaxed),
                    counters: status.into(),
                },
            ));
            drop(sender);
            let _ = writer.join();
        }
    }
}

fn utc_now() -> SYSTEMTIME {
    // GetSystemTime fills the returned structure with the current UTC time.
    unsafe { GetSystemTime() }
}

fn timestamp(time: SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds,
    )
}

fn write_session(receiver: Receiver<Entry>, started: SYSTEMTIME) -> io::Result<()> {
    #[derive(Serialize)]
    struct Record<'a> {
        time_utc: String,
        session: &'a str,
        #[serde(flatten)]
        event: Event,
    }

    let directory = directory().map_err(io::Error::other)?;
    fs::create_dir_all(&directory)?;
    // The desktop runs one audio worker. Guard rotation if another worker is launched directly.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".diagnostics.lock"))?;
    lock.try_lock()?;
    let mut log = Log::open(directory, FILE_LIMIT)?;
    let session = format!("{}-{}", timestamp(started), process::id());
    for (time, event) in receiver {
        let mut line = serde_json::to_vec(&Record {
            time_utc: timestamp(time),
            session: &session,
            event,
        })?;
        line.push(b'\n');
        log.append(&line)?;
    }
    Ok(())
}

struct Log {
    directory: PathBuf,
    limit: u64,
    file: Option<File>,
    len: u64,
    needs_newline: bool,
}

impl Log {
    fn open(directory: PathBuf, limit: u64) -> io::Result<Self> {
        let mut file = open_current(&directory)?;
        let len = file.metadata()?.len();
        let mut needs_newline = false;
        if len != 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            needs_newline = last[0] != b'\n';
        }
        Ok(Self {
            directory,
            limit,
            file: Some(file),
            len,
            needs_newline,
        })
    }

    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        let size = line.len() as u64;
        if size > self.limit {
            return Ok(());
        }
        if self.len + u64::from(self.needs_newline) + size > self.limit {
            drop(self.file.take());
            ignore_missing(fs::remove_file(self.directory.join(FILES[2])))?;
            for pair in FILES.windows(2).rev() {
                ignore_missing(fs::rename(
                    self.directory.join(pair[0]),
                    self.directory.join(pair[1]),
                ))?;
            }
            self.file = Some(open_current(&self.directory)?);
            self.len = 0;
            self.needs_newline = false;
        }
        let file = self.file.as_mut().expect("current diagnostic log is open");
        if self.needs_newline {
            file.write_all(b"\n")?;
            self.len += 1;
            self.needs_newline = false;
        }
        file.write_all(line)?;
        self.len += size;
        Ok(())
    }
}

fn open_current(directory: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(directory.join(FILES[0]))
}

fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_only_the_three_newest_files_across_reopen() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut log = Log::open(dir.path().to_owned(), 4)?;
        for line in [b"a\n", b"b\n", b"c\n", b"d\n", b"e\n", b"f\n"] {
            log.append(line)?;
        }
        drop(log);
        let mut log = Log::open(dir.path().to_owned(), 4)?;
        log.append(b"g\n")?;
        for (file, expected) in FILES.into_iter().zip(["g\n", "e\nf\n", "c\nd\n"]) {
            assert_eq!(fs::read_to_string(dir.path().join(file))?, expected);
        }
        assert_eq!(fs::read_dir(dir.path())?.count(), FILES.len());
        Ok(())
    }

    #[test]
    fn appending_after_a_partial_write_keeps_the_next_event_on_its_own_line() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(FILES[0]);
        fs::write(&path, "partial")?;
        let mut log = Log::open(dir.path().to_owned(), 16)?;
        log.append(b"next\n")?;
        assert_eq!(fs::read_to_string(path)?, "partial\nnext\n");
        Ok(())
    }
}
