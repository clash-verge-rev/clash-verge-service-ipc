use crate::core::logger::set_or_update_writer;
use crate::core::manager::CORE_MANAGER;
use crate::core::paths::service_paths;
use crate::{ClashConfig, WriterConfig};
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{info, warn};

static DESIRED_STATE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DesiredState {
    pub core_should_be_running: bool,
    pub last_clash_config: Option<ClashConfig>,
    pub last_writer_config: Option<WriterConfig>,
    pub generation: u64,
    pub updated_at: u64,
}

pub async fn load_desired_state() -> Result<DesiredState> {
    let paths = service_paths();
    let content = match tokio::fs::read(paths.desired_state_path()).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DesiredState::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read desired state {:?}",
                    paths.desired_state_path()
                )
            });
        }
    };

    serde_json::from_slice(&content).with_context(|| {
        format!(
            "failed to parse desired state {:?}",
            paths.desired_state_path()
        )
    })
}

pub async fn persist_core_started(config: &ClashConfig) -> Result<DesiredState> {
    update_desired_state(|state| {
        state.core_should_be_running = true;
        state.last_clash_config = Some(config.clone());
        state.last_writer_config = Some(config.log_config.clone());
    })
    .await
}

pub async fn persist_core_stopped() -> Result<DesiredState> {
    update_desired_state(|state| {
        state.core_should_be_running = false;
    })
    .await
}

pub async fn persist_writer_config(config: &WriterConfig) -> Result<DesiredState> {
    update_desired_state(|state| {
        state.last_writer_config = Some(config.clone());
        if let Some(clash_config) = state.last_clash_config.as_mut() {
            clash_config.log_config = config.clone();
        }
    })
    .await
}

pub async fn restore_desired_state() -> Result<()> {
    let state = load_desired_state().await?;

    if let Some(writer_config) = state.last_writer_config.as_ref()
        && let Err(error) = set_or_update_writer(writer_config).await
    {
        warn!("Failed to restore writer config: {}", error);
    }

    if !state.core_should_be_running {
        info!("Desired state does not require core restore");
        return Ok(());
    }

    let Some(config) = state.last_clash_config else {
        warn!("Desired state requests core restore but has no ClashConfig");
        return Ok(());
    };

    info!(
        "Restoring core from desired state generation {}",
        state.generation
    );
    CORE_MANAGER.lock().await.start_core(config).await
}

async fn update_desired_state(update: impl FnOnce(&mut DesiredState)) -> Result<DesiredState> {
    let _guard = DESIRED_STATE_LOCK.lock().await;
    let mut state = load_desired_state().await?;
    update(&mut state);
    state.generation = state.generation.saturating_add(1);
    state.updated_at = unix_timestamp_secs();
    write_desired_state(&state).await?;
    Ok(state)
}

async fn write_desired_state(state: &DesiredState) -> Result<()> {
    let paths = service_paths();
    if let Some(parent) = paths.desired_state_path().parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create desired state directory {:?}", parent))?;
    }

    let temp_path = paths.desired_state_path().with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(state)?;
    tokio::fs::write(&temp_path, json)
        .await
        .with_context(|| format!("failed to write desired state temp file {:?}", temp_path))?;
    tokio::fs::rename(&temp_path, paths.desired_state_path())
        .await
        .with_context(|| {
            format!(
                "failed to move desired state into {:?}",
                paths.desired_state_path()
            )
        })?;

    Ok(())
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}
