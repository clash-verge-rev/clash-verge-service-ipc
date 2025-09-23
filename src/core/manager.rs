use anyhow::Result;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::{
    process::{Child, Command},
    sync::{Arc, Mutex},
};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub core_path: String,
    pub config_path: String,
    pub config_dir: String,
    pub log_dir: String,
}

pub struct CoreManager {
    running_child: Arc<Mutex<Option<Child>>>,
    running_config: Arc<Mutex<Option<CoreConfig>>>,
}

impl CoreManager {
    fn new() -> Self {
        CoreManager {
            running_child: Arc::new(Mutex::new(None)),
            running_config: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start_core(&mut self, config: CoreConfig) -> Result<()> {
        if self.running_child.lock().unwrap().is_some() {
            info!("Core is already running");
            let _ = self.stop_core();
            return Ok(());
        }

        info!("Starting core with config: {:?}", config);
        self.running_config = Arc::new(Mutex::new(Some(config)));

        if let Some(config) = self.running_config.lock().unwrap().as_ref() {
            let args = vec![
                "-d",
                config.config_dir.as_str(),
                "-f",
                config.config_path.as_str(),
            ];

            let child = Command::new(config.core_path.as_str()).args(args).spawn()?;

            self.running_child = Arc::new(Mutex::new(Some(child)));
        }

        Ok(())
    }

    pub fn stop_core(&mut self) -> Result<()> {
        info!("Stopping core");

        let child_opt = self.running_child.lock().unwrap().take();
        if let Some(mut child) = child_opt {
            if let Err(e) = child.kill() {
                warn!("Failed to kill child ({0}): {e}", child.id());
            }
        } else {
            info!("No running core process found");
        }

        let config_opt = self.running_config.lock().unwrap().take();
        if let Some(config) = config_opt {
            info!("Clearing running config: {:?}", config);
        } else {
            info!("No running config to clear");
        }

        Ok(())
    }
}

pub static CORE_MANAGER: Lazy<Arc<Mutex<CoreManager>>> =
    Lazy::new(|| Arc::new(Mutex::new(CoreManager::new())));
