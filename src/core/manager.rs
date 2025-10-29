use crate::WriterConfig;
use crate::core::ClashConfig;
use crate::core::logger::{get_writer, set_or_update_writer};
use anyhow::Result;
use clash_verge_logger::AsyncLogger;
use compact_str::CompactString;
use flexi_logger::writers::LogWriter;
use flexi_logger::{DeferredNow, Record};
use once_cell::sync::Lazy;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::{io::BufReader, process::Command};
use tokio::{process::Child, sync::Mutex};
use tracing::{info, warn};

pub struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
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

impl ChildGuard {
    fn inner(&mut self) -> Option<&mut Child> {
        self.0.as_mut()
    }
}

pub struct CoreManager {
    running_child: Arc<Mutex<Option<ChildGuard>>>,
}

impl CoreManager {
    fn new() -> Self {
        CoreManager {
            running_child: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn start_core(&mut self, config: ClashConfig) -> Result<()> {
        if let Some(child) = self.running_child.lock().await.take() {
            info!("Core is already running, stopping existing instance");
            drop(child);
        }

        info!("Starting core with config: {:?}", config);

        let args = vec![
            "-d",
            config.core_config.config_dir.as_str(),
            "-f",
            config.core_config.config_path.as_str(),
        ];

        let child_guard =
            run_with_logging(&config.core_config.core_path, &args, &config.log_config).await?;

        let mut child_lock = self.running_child.lock().await;
        *child_lock = Some(child_guard);

        self.after_start().await;

        Ok(())
    }

    pub async fn stop_core(&mut self) -> Result<()> {
        info!("Stopping core");
        LOGGER_MANAGER.clear_logs().await;

        let child_guard = self.running_child.lock().await.take();
        drop(child_guard);

        self.after_stop().await;

        Ok(())
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
    }
}

pub async fn run_with_logging(
    bin_path: &str,
    args: &Vec<&str>,
    writer_config: &WriterConfig,
) -> Result<ChildGuard> {
    set_or_update_writer(writer_config).await?;
    let shared_writer = get_writer().unwrap();

    let child = Command::new(bin_path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut child_guard = ChildGuard(Some(child));

    let stdout = child_guard
        .inner()
        .as_mut()
        .and_then(|c| c.stdout.take())
        .unwrap();
    let stderr = child_guard
        .inner()
        .as_mut()
        .and_then(|c| c.stderr.take())
        .unwrap();

    let mut stdout_reader = BufReader::new(stdout).lines();
    let shared_writer_clone = shared_writer.clone();
    tokio::spawn(async move {
        let w = shared_writer_clone.lock().await;
        while let Ok(Some(line)) = stdout_reader.next_line().await {
            let message = CompactString::from(line.as_str());
            {
                let mut now = DeferredNow::default();
                let arg = format_args!("{}", line);
                let record = Record::builder()
                    .args(arg)
                    .level(log::Level::Info)
                    .target("service")
                    .build();
                let _ = w.write(&mut now, &record);
            }
            LOGGER_MANAGER.append_log(message).await;
        }
    });

    let mut stderr_reader = BufReader::new(stderr).lines();
    let shared_writer_clone = shared_writer.clone();
    tokio::spawn(async move {
        let w = shared_writer_clone.lock().await;
        while let Ok(Some(line)) = stderr_reader.next_line().await {
            let message = CompactString::from(line.as_str());
            {
                let mut now = DeferredNow::default();
                let arg = format_args!("{}", line);
                let record = Record::builder()
                    .args(arg)
                    .level(log::Level::Error)
                    .target("service")
                    .build();
                let _ = w.write(&mut now, &record);
            }
            LOGGER_MANAGER.append_log(message).await;
        }
    });

    Ok(child_guard)
}

pub static CORE_MANAGER: Lazy<Arc<Mutex<CoreManager>>> =
    Lazy::new(|| Arc::new(Mutex::new(CoreManager::new())));

pub static LOGGER_MANAGER: Lazy<Arc<AsyncLogger>> = Lazy::new(|| Arc::new(AsyncLogger::new()));
