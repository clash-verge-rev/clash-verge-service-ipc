use crate::WriterConfig;
use crate::core::ClashConfig;
use crate::core::logger::{get_writer, set_or_update_writer};
use anyhow::{Result, anyhow};
use clash_verge_logger::AsyncLogger;
use compact_str::CompactString;
use flexi_logger::writers::LogWriter;
use flexi_logger::{DeferredNow, Record};
use once_cell::sync::Lazy;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;
use tokio::{io::BufReader, process::Command};
use tokio::{process::Child, sync::Mutex, task::JoinHandle};
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

    fn take(mut self) -> Option<Child> {
        self.child.take()
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

fn backoff_delay(attempt: u32, max: Duration) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.min(5));
    base.min(max)
}

pub struct CoreManager {
    running_child: Arc<Mutex<Option<ChildGuard>>>,
    running_config: Arc<Mutex<Option<ClashConfig>>>,
    core_start_time: Arc<Mutex<Option<Instant>>>,
    watchdog_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl CoreManager {
    fn new() -> Self {
        CoreManager {
            running_child: Arc::new(Mutex::new(None)),
            running_config: Arc::new(Mutex::new(None)),
            core_start_time: Arc::new(Mutex::new(None)),
            watchdog_handle: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn start_core(&self, config: ClashConfig) -> Result<()> {
        let value = self.running_child.lock().await.take();
        if let Some(child) = value {
            info!("Core is already running, stopping existing instance");
            drop(child);
            LOGGER_MANAGER.clear_logs().await;
        }

        info!("Starting core with config: {:?}", config);

        let args = vec![
            "-d",
            config.core_config.config_dir.as_str(),
            "-f",
            config.core_config.config_path.as_str(),
            if cfg!(windows) {
                "-ext-ctl-pipe"
            } else {
                "-ext-ctl-unix"
            },
            config.core_config.core_ipc_path.as_str(),
        ];

        let child_guard =
            run_with_logging(&config.core_config.core_path, &args, &config.log_config).await?;

        {
            let mut child_lock = self.running_child.lock().await;
            *child_lock = Some(child_guard);
            *self.core_start_time.lock().await = Some(Instant::now());
        }

        *self.running_config.lock().await = Some(config);

        self.after_start().await;
        self.start_watchdog().await;

        Ok(())
    }

    pub async fn stop_core(&self) -> Result<()> {
        info!("Stopping core");
        LOGGER_MANAGER.clear_logs().await;

        self.stop_watchdog().await;

        let child_guard = self.running_child.lock().await.take();
        drop(child_guard);

        *self.core_start_time.lock().await = None;

        let start_clash = self.running_config.lock().await.take();
        if let Some(config) = start_clash {
            info!("Clearing running config: {:?}", config);
        } else {
            info!("No running config to clear");
        }

        self.after_stop().await;

        Ok(())
    }

    async fn start_watchdog(&self) {
        let child_arc = Arc::clone(&self.running_child);
        let config_arc = Arc::clone(&self.running_config);
        let start_time_arc = Arc::clone(&self.core_start_time);
        let watchdog_config = WatchdogConfig::default();

        let handle = tokio::spawn(async move {
            let mut restart_timestamps: Vec<Instant> = Vec::new();
            let mut consecutive_attempt = 0u32;

            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;

                let mut child_lock = child_arc.lock().await;
                let child_opt = child_lock.as_mut();

                if let Some(guard) = child_opt {
                    if let Some(child) = guard.inner() {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                let uptime = start_time_arc
                                    .lock()
                                    .await
                                    .map(|t| t.elapsed())
                                    .unwrap_or_default();

                                let exit_info = CoreExitInfo {
                                    exit_code: status.code(),
                                    #[cfg(unix)]
                                    signal: {
                                        #[cfg(unix)]
                                        {
                                            use std::os::unix::process::ExitStatusExt;
                                            status.signal()
                                        }
                                    },
                                    uptime,
                                };

                                error!(
                                    "Core exited unexpectedly — code: {:?}, diagnosis: {}, uptime: {:.1}s",
                                    exit_info.exit_code,
                                    exit_info.diagnosis(),
                                    exit_info.uptime.as_secs_f64()
                                );

                                #[cfg(unix)]
                                if let Some(sig) = exit_info.signal {
                                    error!("Core terminated by signal: {}", sig);
                                }

                                let dead_guard = child_lock.take();
                                if let Some(guard) = dead_guard {
                                    let _ = guard.take();
                                }
                                drop(child_lock);

                                let now = Instant::now();
                                restart_timestamps.retain(|t| {
                                    now.duration_since(*t) < watchdog_config.restart_window
                                });
                                restart_timestamps.push(now);

                                if restart_timestamps.len() as u32 > watchdog_config.max_restarts {
                                    error!(
                                        "Core restarted {} times in {}s, giving up",
                                        restart_timestamps.len(),
                                        watchdog_config.restart_window.as_secs()
                                    );
                                    break;
                                }

                                let delay =
                                    backoff_delay(consecutive_attempt, watchdog_config.max_backoff);
                                info!(
                                    "Restart attempt #{} after {}ms backoff",
                                    consecutive_attempt + 1,
                                    delay.as_millis()
                                );
                                tokio::time::sleep(delay).await;

                                let config_guard = config_arc.lock().await;
                                if let Some(config) = config_guard.as_ref() {
                                    let args = vec![
                                        "-d",
                                        config.core_config.config_dir.as_str(),
                                        "-f",
                                        config.core_config.config_path.as_str(),
                                    ];

                                    match run_with_logging(
                                        &config.core_config.core_path,
                                        &args,
                                        &config.log_config,
                                    )
                                    .await
                                    {
                                        Ok(new_guard) => {
                                            let mut lock = child_arc.lock().await;
                                            *lock = Some(new_guard);
                                            *start_time_arc.lock().await = Some(Instant::now());
                                            consecutive_attempt += 1;
                                            info!(
                                                "Core restarted successfully (attempt #{})",
                                                consecutive_attempt
                                            );
                                        }
                                        Err(e) => {
                                            error!("Failed to restart core: {}", e);
                                            consecutive_attempt += 1;
                                        }
                                    }
                                } else {
                                    warn!("No saved config for restart, watchdog stopping");
                                    break;
                                }
                            }
                            Ok(None) => {
                                consecutive_attempt = 0;
                            }
                            Err(e) => {
                                warn!("Failed to check child process status: {}", e);
                            }
                        }
                    }
                } else {
                    break;
                }
            }
        });

        *self.watchdog_handle.lock().await = Some(handle);
    }

    async fn stop_watchdog(&self) {
        if let Some(handle) = self.watchdog_handle.lock().await.take() {
            handle.abort();
            info!("Watchdog stopped");
        }
    }

    pub async fn after_start(&self) {
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            use std::path::Path;
            use tokio::fs;

            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let target = Path::new("/tmp/verge/verge-mihomo.sock");
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
    }

    pub async fn after_stop(&self) {
        #[cfg(unix)]
        {
            use std::path::Path;
            use tokio::fs;

            let target = Path::new("/tmp/verge/verge-mihomo.sock");
            info!("Removing socket file {:?}", target);
            if !target.exists() {
                info!("{:?} does not exist, no need to remove", target);
                return;
            }
            match fs::remove_file(target).await {
                Ok(_) => info!("Successfully removed {:?}", target),
                Err(e) => warn!("Failed to remove {:?}: {}", target, e),
            }
        }
        LOGGER_MANAGER.clear_logs().await;
    }
}

pub async fn run_with_logging(
    bin_path: &str,
    args: &Vec<&str>,
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
