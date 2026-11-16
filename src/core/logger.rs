use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::Result;
use flexi_logger::{
    Cleanup, Criterion, DeferredNow, FileSpec, Naming, WriteMode,
    writers::{FileLogWriter, FileLogWriterBuilder, LogWriter as _},
};
use log::{Level, Record};
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::{Mutex, RwLock};
use tracing_estuary::{Pipeline, PipelineBuilder};

use crate::core::paths::{ensure_persistent_state_layout, service_paths};
use crate::core::platform_security;
use crate::core::structure::WriterConfig;

// Buffered writes avoid flexi's one-syscall-per-line default; the background
// flusher bounds the hard-kill loss window.
const WRITE_BUFFER_CAPACITY: usize = 64 * 1024;
const WRITE_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

const SERVICE_LOG_MAX_SIZE: u64 = 10 * 1024 * 1024;
const SERVICE_LOG_MAX_FILES: usize = 3;

static GLOBAL_WRITER: RwLock<Option<FileLogWriter>> = RwLock::new(None);
static SERVICE_LOG: OnceCell<Pipeline> = OnceCell::new();

/// Logs to stdout and to the service log file, since launchd and the Windows SCM discard stdout.
pub fn init_service_logging() {
    let (pipeline, file_error) =
        match service_log_writer().and_then(|writer| PipelineBuilder::new().file_writer(writer).init()) {
            Ok(pipeline) => (Ok(pipeline), None),
            Err(error) => (PipelineBuilder::new().init(), Some(error)),
        };
    match pipeline {
        Ok(pipeline) => {
            let _ = SERVICE_LOG.set(pipeline);
        }
        Err(error) => eprintln!("Failed to install service logging: {error:#}"),
    }
    if let Some(error) = file_error {
        tracing::warn!("Service log file unavailable, logging to stdout only: {error:#}");
    }
}

/// Writes out queued service log lines before the process exits.
pub fn flush_service_log() {
    if let Some(pipeline) = SERVICE_LOG.get() {
        pipeline.flush_all();
    }
}

fn service_log_writer() -> anyhow::Result<FileLogWriterBuilder> {
    ensure_persistent_state_layout()?;
    let directory = service_paths()?.logs_dir();
    platform_security::ensure_private_service_directory(&directory)?;
    Ok(FileLogWriter::builder(
        FileSpec::default()
            .directory(directory)
            .basename(crate::SERVICE_SLUG)
            .suppress_timestamp(),
    )
    .format(tracing_estuary::file_format_with_level)
    // Unbuffered, so a killed service keeps its last lines.
    .write_mode(WriteMode::Direct)
    // A restart must not rotate the previous run away.
    .append()
    .rotate(
        Criterion::Size(SERVICE_LOG_MAX_SIZE),
        Naming::TimestampsCustomFormat {
            current_infix: Some("latest"),
            format: "%Y-%m-%d_%H-%M-%S",
        },
        Cleanup::KeepLogFiles(SERVICE_LOG_MAX_FILES),
    ))
}

fn service_writer_builder(config: &WriterConfig) -> FileLogWriterBuilder {
    FileLogWriter::builder(
        FileSpec::default()
            .directory(config.directory.clone())
            .basename("service")
            .suppress_timestamp(),
    )
    .format(tracing_estuary::file_format_without_level)
    .write_mode(WriteMode::BufferAndFlushWith(
        WRITE_BUFFER_CAPACITY,
        WRITE_FLUSH_INTERVAL,
    ))
    .rotate(
        Criterion::Size(config.max_log_size),
        Naming::TimestampsCustomFormat {
            current_infix: Some("latest"),
            format: "%Y-%m-%d_%H-%M-%S",
        },
        Cleanup::KeepLogFiles(config.max_log_files),
    )
}

/// Resets the writer in place instead of rebuilding it: flexi's flusher thread
/// cannot be stopped, so every dropped writer would leak a thread and an fd.
pub async fn set_or_update_writer(config: &WriterConfig) -> Result<()> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || configure_writer(&config)).await?
}

fn configure_writer(config: &WriterConfig) -> Result<()> {
    core_log_worker()?;
    let builder = service_writer_builder(config);
    let mut slot = GLOBAL_WRITER.write();
    match slot.as_ref() {
        Some(writer) => {
            // Propagate the drain failure so the buffered tail stays retryable.
            writer.flush()?;
            writer.reset(&builder)?;
        }
        None => *slot = Some(builder.try_build()?),
    }
    Ok(())
}

fn write_file_line(epoch: &Arc<()>, level: Level, line: &str) -> std::io::Result<()> {
    let guard = GLOBAL_WRITER.read();
    // Holding the writer lock through this check prevents a stale reader from writing into
    // the next owner's reset writer. Ring resets invalidate both queued and in-flight readers.
    if !LOG_RING.is_current(epoch) {
        return Ok(());
    }
    if let Some(writer) = guard.as_ref() {
        let mut now = DeferredNow::default();
        let args = format_args!("{line}");
        let record = Record::builder().args(args).level(level).target("service").build();
        writer.write(&mut now, &record)?;
    }
    Ok(())
}

fn flush_file_writer() {
    let guard = GLOBAL_WRITER.read();
    if let Some(writer) = guard.as_ref() {
        let _ = writer.flush();
    }
}

pub(super) const MAX_CORE_LINE_BYTES: usize = 16 * 1024;
const LOGS_QUEUE_LEN: usize = 100;
const MAX_RING_BYTES: usize = 256 * 1024;
const FILE_QUEUE_LEN: usize = 64;

struct RingState {
    logs: VecDeque<String>,
    bytes: usize,
    epoch: Arc<()>,
}

pub struct LogRing {
    inner: Mutex<RingState>,
}

impl LogRing {
    fn new() -> Self {
        Self {
            inner: Mutex::new(RingState {
                logs: VecDeque::with_capacity(LOGS_QUEUE_LEN),
                bytes: 0,
                epoch: Arc::new(()),
            }),
        }
    }

    pub fn reader_epoch(&self) -> Arc<()> {
        Arc::clone(&self.inner.lock().epoch)
    }

    fn is_current(&self, epoch: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.inner.lock().epoch, epoch)
    }

    pub fn append_log(&self, epoch: &Arc<()>, log: String) {
        let mut state = self.inner.lock();
        if !Arc::ptr_eq(&state.epoch, epoch) {
            return;
        }
        // Readers already cap input bytes. Keep this boundary safe for every caller as well.
        let mut log = log;
        let mut end = log.len().min(MAX_CORE_LINE_BYTES);
        while !log.is_char_boundary(end) {
            end -= 1;
        }
        log.truncate(end);
        while state.logs.len() >= LOGS_QUEUE_LEN || state.bytes + log.len() > MAX_RING_BYTES {
            if let Some(old) = state.logs.pop_front() {
                state.bytes -= old.len();
            } else {
                break;
            }
        }
        state.bytes += log.len();
        state.logs.push_back(log);
    }

    pub fn get_logs(&self) -> Vec<String> {
        self.inner.lock().logs.iter().cloned().collect()
    }

    pub fn clear_logs(&self) {
        let mut state = self.inner.lock();
        state.logs.clear();
        state.bytes = 0;
        state.epoch = Arc::new(());
    }
}

pub static LOG_RING: Lazy<LogRing> = Lazy::new(LogRing::new);

enum CoreLogMessage {
    Line { epoch: Arc<()>, level: Level, line: String },
    Flush(mpsc::Sender<()>),
}

struct CoreLogWorker {
    sender: SyncSender<CoreLogMessage>,
    dropped: Arc<AtomicU64>,
    // One worker belongs to the process-wide writer, across writer resets.
    _thread: JoinHandle<()>,
}

static CORE_LOG_WORKER: OnceCell<CoreLogWorker> = OnceCell::new();

fn core_log_worker() -> std::io::Result<&'static CoreLogWorker> {
    CORE_LOG_WORKER.get_or_try_init(|| {
        let (sender, receiver) = mpsc::sync_channel(FILE_QUEUE_LEN);
        let dropped = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&dropped);
        let thread = std::thread::Builder::new()
            .name("core-log-writer".into())
            .spawn(move || {
                let mut failed = false;
                while let Ok(message) = receiver.recv() {
                    let lost = observed.swap(0, Ordering::Relaxed);
                    if lost != 0 {
                        tracing::warn!("Dropped {lost} core file log lines because the writer queue was full");
                    }
                    match message {
                        CoreLogMessage::Line { epoch, level, line } => match write_file_line(&epoch, level, &line) {
                            Ok(()) => failed = false,
                            Err(error) => {
                                if !failed {
                                    tracing::warn!("Core log file writer failed; continuing to drain output: {error}");
                                }
                                failed = true;
                            }
                        },
                        CoreLogMessage::Flush(done) => {
                            flush_file_writer();
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(CoreLogWorker {
            sender,
            dropped,
            _thread: thread,
        })
    })
}

impl CoreLogWorker {
    fn enqueue(&self, epoch: &Arc<()>, level: Level, line: String) {
        if self
            .sender
            .try_send(CoreLogMessage::Line {
                epoch: Arc::clone(epoch),
                level,
                line,
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn write_core_line(epoch: &Arc<()>, level: Level, line: String) {
    LOG_RING.append_log(epoch, line.clone());
    if let Some(worker) = CORE_LOG_WORKER.get() {
        worker.enqueue(epoch, level, line);
    }
}

pub fn flush_writer() {
    let Some(worker) = CORE_LOG_WORKER.get() else {
        return;
    };
    let (done, receiver) = mpsc::channel();
    let mut message = CoreLogMessage::Flush(done);
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match worker.sender.try_send(message) {
            Ok(()) => {
                let _ = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
                return;
            }
            Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return;
                }
                message = returned;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_bounds_bytes_and_rejects_outgoing_reader_tail() {
        let ring = LogRing::new();
        let outgoing = ring.reader_epoch();
        ring.append_log(&outgoing, "owner A".into());
        ring.clear_logs();
        let current = ring.reader_epoch();
        ring.append_log(&outgoing, "late owner A".into());
        for _ in 0..100 {
            ring.append_log(&current, "x".repeat(MAX_CORE_LINE_BYTES + 1));
        }
        let logs = ring.get_logs();
        assert!(logs.iter().all(|line| line.len() <= MAX_CORE_LINE_BYTES));
        assert!(logs.iter().map(String::len).sum::<usize>() <= MAX_RING_BYTES);
        assert!(!logs.iter().any(|line| line.contains("owner A")));
    }

    #[test]
    fn full_or_failed_file_queue_never_backpressures_output() {
        let (sender, receiver) = mpsc::sync_channel(FILE_QUEUE_LEN);
        let worker = CoreLogWorker {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            _thread: std::thread::spawn(|| {}),
        };
        let epoch = Arc::new(());
        for _ in 0..FILE_QUEUE_LEN + 100 {
            worker.enqueue(&epoch, Level::Info, "fixture".into());
        }
        assert_eq!(worker.dropped.load(Ordering::Relaxed), 100);
        assert_eq!(receiver.try_iter().count(), FILE_QUEUE_LEN);
        drop(receiver);
        worker.enqueue(&epoch, Level::Info, "failed writer".into());
        assert_eq!(worker.dropped.load(Ordering::Relaxed), 101);
    }

    #[test]
    #[serial_test::serial]
    fn outgoing_reader_cannot_write_into_new_owner_ring_or_file() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("service-log-owner-{}", std::process::id()));
        let configure = |name: &str| {
            configure_writer(&WriterConfig {
                directory: root.join(name).to_string_lossy().into_owned(),
                max_log_size: 1024 * 1024,
                max_log_files: 1,
            })
        };
        LOG_RING.clear_logs();
        configure("a")?;
        let old = LOG_RING.reader_epoch();
        let paused = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::clone(&paused);
        let reader = std::thread::spawn(move || {
            release.wait();
            LOG_RING.append_log(&old, "outgoing-owner-tail".into());
            write_file_line(&old, Level::Info, "outgoing-owner-tail")
        });
        LOG_RING.clear_logs();
        configure("b")?;
        paused.wait();
        reader.join().expect("fixture reader failed")?;
        let current = LOG_RING.reader_epoch();
        write_file_line(&current, Level::Info, "current-owner")?;
        flush_file_writer();
        assert!(
            !LOG_RING
                .get_logs()
                .iter()
                .any(|line| line.contains("outgoing-owner-tail"))
        );
        let log = std::fs::read_to_string(root.join("b/service_latest.log"))?;
        assert!(log.contains("current-owner"));
        assert!(!log.contains("outgoing-owner-tail"));
        // Reset the persistent writer before removing only this fixture's directory.
        configure_writer(&WriterConfig {
            directory: crate::core::paths::service_paths()?
                .logs_dir()
                .to_string_lossy()
                .into_owned(),
            max_log_size: 1024 * 1024,
            max_log_files: 1,
        })?;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(feature = "test")]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn waiting_writer_configuration_does_not_block_status_or_output() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("service-log-blocked-{}", std::process::id()));
        let credentials = crate::test_owner_credentials(&root)?;
        let owner = crate::core::auth::AuthenticatedOwner {
            key: crate::owner_key(&credentials.identity),
            identity: credentials.identity,
            app_data_root: root.clone(),
        };
        let config = WriterConfig {
            directory: root.join("logs").to_string_lossy().into_owned(),
            max_log_size: 1024 * 1024,
            max_log_files: 1,
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let held = std::thread::spawn(move || {
            let _writer = GLOBAL_WRITER.read();
            let _ = ready_tx.send(());
            let _ = release_rx.recv();
        });
        ready_rx.await?;
        let update = tokio::spawn(async move { set_or_update_writer(&config).await });
        // Yield to the configuration task while the file writer's lock is intentionally held.
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!update.is_finished());
        let progress = tokio::time::timeout(Duration::from_secs(1), async {
            crate::core::status::service_status_snapshot(&owner).await?;
            let mut drained = Vec::new();
            crate::core::manager::read_core_lines(&b"core output fixture\n"[..], |line| drained.push(line)).await;
            assert_eq!(drained, ["core output fixture"]);
            Ok::<_, anyhow::Error>(())
        })
        .await;
        let _ = release_tx.send(());
        held.join().expect("fixture holder failed");
        update.await??;
        progress??;
        configure_writer(&WriterConfig {
            directory: crate::core::paths::service_paths()?
                .logs_dir()
                .to_string_lossy()
                .into_owned(),
            max_log_size: 1024 * 1024,
            max_log_files: 1,
        })?;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
