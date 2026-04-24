use crate::WriterConfig;
use crate::core::ClashConfig;
use crate::core::logger::{get_writer, set_or_update_writer};
use crate::core::runtime::{
    CoreRuntimeRecord, remove_core_runtime_record, write_core_runtime_record,
};
use crate::core::state::set_service_lifecycle_state;
use crate::core::structure::ServiceLifecycleState;
use anyhow::{Result, anyhow};
use clash_verge_logger::AsyncLogger;
use compact_str::CompactString;
use flexi_logger::writers::LogWriter;
use flexi_logger::{DeferredNow, Record};
use once_cell::sync::Lazy;
use std::process::Stdio;
#[cfg(feature = "test")]
use std::sync::Mutex as StdMutex;
use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncBufReadExt;
use tokio::{io::BufReader, process::Command};
use tokio::{
    process::Child,
    sync::{Mutex, oneshot},
    task::JoinHandle,
};
use tracing::{error, info, warn};

#[derive(Debug)]
pub struct CoreExitInfo {
    pub exit_code: Option<i32>,
    #[cfg(unix)]
    pub signal: Option<i32>,
    pub uptime: Duration,
}

impl CoreExitInfo {
    pub fn diagnosis(&self) -> &'static str {
        #[cfg(unix)]
        {
            if let Some(sig) = self.signal {
                return match sig {
                    9 => "Killed by OOM killer or admin (SIGKILL)",
                    11 => "Segmentation fault (SIGSEGV)",
                    15 => "Graceful shutdown (SIGTERM)",
                    6 => "Aborted (SIGABRT)",
                    _ => "Terminated by signal",
                };
            }
        }
        match self.exit_code {
            Some(0) => "Normal exit",
            Some(_) => "Abnormal exit",
            None => "Unknown exit reason",
        }
    }
}

pub struct ChildGuard {
    child: Option<Child>,
    readers: Vec<JoinHandle<()>>,
}

impl ChildGuard {
    fn inner(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    fn take(mut self) -> Option<Child> {
        self.child.take()
    }

    async fn kill_now(mut self) {
        for reader in self.readers.drain(..) {
            reader.abort();
        }

        if let Some(mut child) = self.child.take() {
            if let Err(e) = child.kill().await {
                warn!("Failed to kill child ({:?}): {e}", child.id());
            } else {
                info!("Successfully killed child ({:?})", child.id());
            }
        } else {
            info!("No running core process found");
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        for reader in self.readers.drain(..) {
            reader.abort();
        }
        if let Some(mut child) = self.child.take() {
            tokio::spawn(async move {
                if let Err(e) = child.kill().await {
                    warn!("Failed to kill child ({:?}): {e}", child.id());
                } else {
                    info!("Successfully killed child ({:?})", child.id());
                }
            });
        } else {
            info!("No running core process found");
        }
    }
}

#[derive(Clone, Copy)]
struct WatchdogConfig {
    max_restarts: u32,
    restart_window: Duration,
    max_backoff: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            max_restarts: 10,
            restart_window: Duration::from_secs(600),
            max_backoff: Duration::from_secs(30),
        }
    }
}

#[cfg(feature = "test")]
#[derive(Clone, Copy)]
pub struct CoreWatchdogTestConfig {
    pub max_restarts: u32,
    pub restart_window: Duration,
    pub max_backoff: Duration,
}

#[cfg(feature = "test")]
static WATCHDOG_CONFIG_OVERRIDE: Lazy<StdMutex<Option<WatchdogConfig>>> =
    Lazy::new(|| StdMutex::new(None));

#[cfg(feature = "test")]
pub fn set_core_watchdog_config_for_tests(config: Option<CoreWatchdogTestConfig>) {
    let mut guard = WATCHDOG_CONFIG_OVERRIDE.lock().unwrap();
    *guard = config.map(|config| WatchdogConfig {
        max_restarts: config.max_restarts,
        restart_window: config.restart_window,
        max_backoff: config.max_backoff,
    });
}

fn watchdog_config() -> WatchdogConfig {
    #[cfg(feature = "test")]
    if let Some(config) = *WATCHDOG_CONFIG_OVERRIDE.lock().unwrap() {
        return config;
    }

    WatchdogConfig::default()
}

fn backoff_delay(attempt: u32, max: Duration) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }

    let base = Duration::from_secs(1u64 << (attempt - 1).min(5));
    base.min(max)
}

fn core_args(config: &ClashConfig) -> Vec<String> {
    vec![
        "-d".to_string(),
        config.core_config.config_dir.clone(),
        "-f".to_string(),
        config.core_config.config_path.clone(),
        if cfg!(windows) {
            "-ext-ctl-pipe".to_string()
        } else {
            "-ext-ctl-unix".to_string()
        },
        config.core_config.core_ipc_path.clone(),
    ]
}

fn log_core_exit(status: &std::process::ExitStatus, uptime: Duration) -> String {
    let exit_info = CoreExitInfo {
        exit_code: status.code(),
        #[cfg(unix)]
        signal: {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        },
        uptime,
    };

    error!(
        "Core exited unexpectedly - code: {:?}, diagnosis: {}, uptime: {:.1}s",
        exit_info.exit_code,
        exit_info.diagnosis(),
        exit_info.uptime.as_secs_f64()
    );

    #[cfg(unix)]
    if let Some(sig) = exit_info.signal {
        error!("Core terminated by signal: {}", sig);
    }

    format!(
        "{} (code: {:?})",
        exit_info.diagnosis(),
        exit_info.exit_code
    )
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn non_zero_u32(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

fn non_zero_u64(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

async fn write_runtime_record_for_config(
    pid: Option<u32>,
    config: &ClashConfig,
    context: &'static str,
) {
    if let Some(pid) = pid
        && let Err(error) = write_core_runtime_record(&CoreRuntimeRecord {
            pid,
            ipc_path: config.core_config.core_ipc_path.clone(),
        })
        .await
    {
        warn!("Failed to write core runtime record {context}: {error}");
    }
}

pub struct CoreManager {
    running_pid: Arc<AtomicU32>,
    running_config: Mutex<Option<ClashConfig>>,
    core_start_time: Arc<Mutex<Option<Instant>>>,
    core_started_at: Arc<AtomicU64>,
    last_core_exit_reason: Arc<Mutex<Option<String>>>,
    restart_count: Arc<AtomicU32>,
    last_recovery_at: Arc<AtomicU64>,
    watchdog_shutdown: Mutex<Option<oneshot::Sender<()>>>,
    watchdog_handle: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
pub(super) struct CoreStatusSnapshot {
    pub(super) core_pid: Option<u32>,
    pub(super) core_started_at: Option<u64>,
    pub(super) last_core_exit_reason: Option<String>,
    pub(super) restart_count: u32,
    pub(super) last_recovery_at: Option<u64>,
}

impl CoreManager {
    fn new() -> Self {
        CoreManager {
            running_pid: Arc::new(AtomicU32::new(0)),
            running_config: Mutex::new(None),
            core_start_time: Arc::new(Mutex::new(None)),
            core_started_at: Arc::new(AtomicU64::new(0)),
            last_core_exit_reason: Arc::new(Mutex::new(None)),
            restart_count: Arc::new(AtomicU32::new(0)),
            last_recovery_at: Arc::new(AtomicU64::new(0)),
            watchdog_shutdown: Mutex::new(None),
            watchdog_handle: Mutex::new(None),
        }
    }

    pub async fn start_core(&self, config: ClashConfig) -> Result<()> {
        if self.running_pid.load(Ordering::Relaxed) != 0 {
            info!("Core is already running, stopping existing instance");
            self.stop_core().await?;
        }

        info!("Starting core with config: {:?}", config);

        let args = core_args(&config);

        let child_guard =
            run_with_logging(&config.core_config.core_path, &args, &config.log_config).await?;
        let child_pid = child_guard.id();

        *self.core_start_time.lock().await = Some(Instant::now());
        self.core_started_at
            .store(unix_timestamp_secs(), Ordering::Relaxed);
        self.running_pid
            .store(child_pid.unwrap_or_default(), Ordering::Relaxed);
        *self.running_config.lock().await = Some(config.clone());

        write_runtime_record_for_config(child_pid, &config, "after start").await;

        self.after_start(config.core_config.core_ipc_path.clone());
        self.start_watchdog(child_guard, config).await;

        Ok(())
    }

    pub async fn stop_core(&self) -> Result<()> {
        info!("Stopping core");
        LOGGER_MANAGER.clear_logs().await;

        self.stop_watchdog().await;

        self.running_pid.store(0, Ordering::Relaxed);
        *self.core_start_time.lock().await = None;
        self.core_started_at.store(0, Ordering::Relaxed);

        let start_clash = self.running_config.lock().await.take();
        let core_ipc_path = start_clash
            .as_ref()
            .map(|config| config.core_config.core_ipc_path.clone());
        if let Some(config) = start_clash {
            info!("Clearing running config: {:?}", config);
        } else {
            info!("No running config to clear");
        }

        remove_core_runtime_record().await;
        self.after_stop(core_ipc_path).await;

        Ok(())
    }

    async fn start_watchdog(&self, child_guard: ChildGuard, config: ClashConfig) {
        let running_pid_arc = Arc::clone(&self.running_pid);
        let start_time_arc = Arc::clone(&self.core_start_time);
        let started_at_arc = Arc::clone(&self.core_started_at);
        let last_exit_reason_arc = Arc::clone(&self.last_core_exit_reason);
        let restart_count_arc = Arc::clone(&self.restart_count);
        let last_recovery_at_arc = Arc::clone(&self.last_recovery_at);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let watchdog_config = watchdog_config();

        let handle = tokio::spawn(async move {
            let mut child_guard = Some(child_guard);
            let mut shutdown_rx = shutdown_rx;
            let mut restart_timestamps: Vec<Instant> = Vec::new();
            let mut consecutive_attempt = 0u32;

            'watchdog: loop {
                let Some(mut current_guard) = child_guard.take() else {
                    break;
                };

                let wait_result = {
                    let Some(child) = current_guard.inner() else {
                        break;
                    };

                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            info!("Core watchdog received shutdown signal");
                            current_guard.kill_now().await;
                            break 'watchdog;
                        }
                        wait_result = child.wait() => wait_result,
                    }
                };

                let status = match wait_result {
                    Ok(status) => status,
                    Err(error) => {
                        warn!("Failed to wait for core process: {}", error);
                        break;
                    }
                };

                let uptime = start_time_arc
                    .lock()
                    .await
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                let exit_reason = log_core_exit(&status, uptime);
                *last_exit_reason_arc.lock().await = Some(exit_reason);
                set_service_lifecycle_state(ServiceLifecycleState::RecoveringCore);

                let _ = current_guard.take();
                running_pid_arc.store(0, Ordering::Relaxed);
                started_at_arc.store(0, Ordering::Relaxed);
                remove_core_runtime_record().await;

                let now = Instant::now();
                restart_timestamps
                    .retain(|t| now.duration_since(*t) < watchdog_config.restart_window);
                if restart_timestamps.is_empty() {
                    consecutive_attempt = 0;
                }
                restart_timestamps.push(now);

                loop {
                    if restart_timestamps.len() as u32 > watchdog_config.max_restarts {
                        error!(
                            "Core restarted {} times in {}s, giving up",
                            restart_timestamps.len(),
                            watchdog_config.restart_window.as_secs()
                        );
                        break 'watchdog;
                    }

                    let delay = backoff_delay(consecutive_attempt, watchdog_config.max_backoff);
                    info!(
                        "Restart attempt #{} after {}ms backoff",
                        consecutive_attempt + 1,
                        delay.as_millis()
                    );

                    if !delay.is_zero() {
                        tokio::select! {
                            _ = &mut shutdown_rx => break 'watchdog,
                            _ = tokio::time::sleep(delay) => {}
                        }
                    }

                    let args = core_args(&config);
                    match run_with_logging(&config.core_config.core_path, &args, &config.log_config)
                        .await
                    {
                        Ok(new_guard) => {
                            let new_pid = new_guard.id();
                            running_pid_arc.store(new_pid.unwrap_or_default(), Ordering::Relaxed);
                            *start_time_arc.lock().await = Some(Instant::now());
                            let now_secs = unix_timestamp_secs();
                            started_at_arc.store(now_secs, Ordering::Relaxed);
                            restart_count_arc.fetch_add(1, Ordering::Relaxed);
                            last_recovery_at_arc.store(now_secs, Ordering::Relaxed);
                            write_runtime_record_for_config(new_pid, &config, "after restart")
                                .await;

                            consecutive_attempt += 1;
                            info!(
                                "Core restarted successfully (attempt #{})",
                                consecutive_attempt
                            );
                            set_service_lifecycle_state(ServiceLifecycleState::Running);
                            child_guard = Some(new_guard);
                            continue 'watchdog;
                        }
                        Err(error) => {
                            error!("Failed to restart core: {}", error);
                            consecutive_attempt += 1;
                            let now = Instant::now();
                            restart_timestamps.retain(|t| {
                                now.duration_since(*t) < watchdog_config.restart_window
                            });
                            restart_timestamps.push(now);
                        }
                    }
                }
            }

            running_pid_arc.store(0, Ordering::Relaxed);
            *start_time_arc.lock().await = None;
            started_at_arc.store(0, Ordering::Relaxed);
            remove_core_runtime_record().await;
        });

        *self.watchdog_shutdown.lock().await = Some(shutdown_tx);
        *self.watchdog_handle.lock().await = Some(handle);
    }

    async fn stop_watchdog(&self) {
        if let Some(shutdown_tx) = self.watchdog_shutdown.lock().await.take() {
            let _ = shutdown_tx.send(());
        }

        if let Some(handle) = self.watchdog_handle.lock().await.take() {
            if let Err(error) = handle.await {
                warn!("Watchdog task failed to join: {}", error);
            }
            info!("Watchdog stopped");
        }
    }

    pub(super) async fn status(&self) -> CoreStatusSnapshot {
        CoreStatusSnapshot {
            core_pid: non_zero_u32(self.running_pid.load(Ordering::Relaxed)),
            core_started_at: non_zero_u64(self.core_started_at.load(Ordering::Relaxed)),
            last_core_exit_reason: self.last_core_exit_reason.lock().await.clone(),
            restart_count: self.restart_count.load(Ordering::Relaxed),
            last_recovery_at: non_zero_u64(self.last_recovery_at.load(Ordering::Relaxed)),
        }
    }

    fn after_start(&self, core_ipc_path: String) {
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            use std::path::Path;
            use tokio::fs;

            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let target = Path::new(&core_ipc_path);
                info!("Setting permissions for {:?}", target);
                if !target.exists() {
                    warn!("{:?} does not exist, skipping permission setting", target);
                    return;
                }
                match fs::set_permissions(target, Permissions::from_mode(0o777)).await {
                    Ok(_) => info!("Permissions set to 777 for {:?}", target),
                    Err(e) => warn!("Failed to set permissions for {:?}: {}", target, e),
                }
            });
        }
        #[cfg(not(unix))]
        {
            let _ = core_ipc_path;
        }
    }

    async fn after_stop(&self, core_ipc_path: Option<String>) {
        #[cfg(unix)]
        {
            use std::path::Path;
            use tokio::fs;

            if let Some(core_ipc_path) = core_ipc_path {
                let target = Path::new(&core_ipc_path);
                info!("Removing socket file {:?}", target);
                if !target.exists() {
                    info!("{:?} does not exist, no need to remove", target);
                } else {
                    match fs::remove_file(target).await {
                        Ok(_) => info!("Successfully removed {:?}", target),
                        Err(e) => warn!("Failed to remove {:?}: {}", target, e),
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = core_ipc_path;
        }
        LOGGER_MANAGER.clear_logs().await;
    }
}

pub async fn run_with_logging(
    bin_path: &str,
    args: &[String],
    writer_config: &WriterConfig,
) -> Result<ChildGuard> {
    set_or_update_writer(writer_config).await?;

    #[cfg(not(unix))]
    let child = Command::new(bin_path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    #[cfg(unix)]
    let child = unsafe {
        Command::new(bin_path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .pre_exec(|| {
                platform_lib::umask(0o007);
                Ok(())
            })
            .spawn()?
    };

    let mut child_guard = ChildGuard {
        child: Some(child),
        readers: Vec::new(),
    };

    let (Some(stdout), Some(stderr)) = (
        child_guard.inner().and_then(|c| c.stdout.take()),
        child_guard.inner().and_then(|c| c.stderr.take()),
    ) else {
        return Err(anyhow!("Failed to capture child output"));
    };

    let stdout_handle = tokio::spawn(async move {
        let mut stdout_reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = stdout_reader.next_line().await {
            let message = CompactString::from(line.as_str());
            {
                if let Some(shared_writer) = get_writer() {
                    let w = shared_writer.lock().await;
                    let mut now = DeferredNow::default();
                    let arg = format_args!("{}", line);
                    let record = Record::builder()
                        .args(arg)
                        .level(log::Level::Info)
                        .target("service")
                        .build();
                    let _ = w.write(&mut now, &record);
                }
            }
            LOGGER_MANAGER.append_log(message).await;
        }
    });

    let stderr_handle = tokio::spawn(async move {
        let mut stderr_reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = stderr_reader.next_line().await {
            let message = CompactString::from(line.as_str());
            {
                if let Some(shared_writer) = get_writer() {
                    let w = shared_writer.lock().await;
                    let mut now = DeferredNow::default();
                    let arg = format_args!("{}", line);
                    let record = Record::builder()
                        .args(arg)
                        .level(log::Level::Error)
                        .target("service")
                        .build();
                    let _ = w.write(&mut now, &record);
                }
            }
            LOGGER_MANAGER.append_log(message).await;
        }
    });

    child_guard.readers.push(stdout_handle);
    child_guard.readers.push(stderr_handle);

    Ok(child_guard)
}

pub static CORE_MANAGER: Lazy<Arc<Mutex<CoreManager>>> =
    Lazy::new(|| Arc::new(Mutex::new(CoreManager::new())));

pub static LOGGER_MANAGER: Lazy<Arc<AsyncLogger>> = Lazy::new(|| Arc::new(AsyncLogger::new()));
