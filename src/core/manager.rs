use crate::WriterConfig;
use crate::core::StartClash;
use crate::core::logger::{get_writer, set_or_update_writer};
use anyhow::Result;
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
    running_config: Arc<Mutex<Option<StartClash>>>,
}

impl CoreManager {
    fn new() -> Self {
        CoreManager {
            running_child: Arc::new(Mutex::new(None)),
            running_config: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn start_core(&mut self, config: StartClash) -> Result<()> {
        if self.running_child.lock().await.is_some() {
            info!("Core is already running");
            let _ = self.stop_core().await;
            return Ok(());
        }

        info!("Starting core with config: {:?}", config);
        self.running_config = Arc::new(Mutex::new(Some(config)));

        if let Some(config) = self.running_config.lock().await.as_ref() {
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
        }

        Ok(())
    }

    pub async fn stop_core(&mut self) -> Result<()> {
        info!("Stopping core");

        let child_guard = self.running_child.lock().await.take();
        drop(child_guard);

        let start_clash = self.running_config.lock().await.take();
        if let Some(config) = start_clash {
            info!("Clearing running config: {:?}", config);
        } else {
            info!("No running config to clear");
        }

        Ok(())
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
            let mut now = DeferredNow::default();
            let arg = format_args!("{}", line);
            let record = Record::builder()
                .args(arg)
                .level(log::Level::Info)
                .target("service")
                .build();
            let _ = w.write(&mut now, &record);
        }
    });

    let mut stderr_reader = BufReader::new(stderr).lines();
    let shared_writer_clone = shared_writer.clone();
    tokio::spawn(async move {
        let w = shared_writer_clone.lock().await;
        while let Ok(Some(line)) = stderr_reader.next_line().await {
            let mut now = DeferredNow::default();
            let arg = format_args!("{}", line);
            let record = Record::builder()
                .args(arg)
                .level(log::Level::Error)
                .target("service")
                .build();
            let _ = w.write(&mut now, &record);
        }
    });

    Ok(child_guard)
}

pub static CORE_MANAGER: Lazy<Arc<Mutex<CoreManager>>> =
    Lazy::new(|| Arc::new(Mutex::new(CoreManager::new())));
