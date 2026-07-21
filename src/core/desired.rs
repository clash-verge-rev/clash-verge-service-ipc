use crate::core::auth::AuthenticatedOwner;
use crate::core::logger::set_or_update_writer;
use crate::core::manager::CORE_MANAGER;
use crate::core::paths::service_paths;
use crate::core::state::set_core_lifecycle_state;
use crate::{ClashConfig, OwnerIdentity, ServiceLifecycleState, WriterConfig};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveOwnerState {
    pub owner_key: String,
    pub identity: OwnerIdentity,
    pub app_data_root: String,
}

impl From<&AuthenticatedOwner> for ActiveOwnerState {
    fn from(owner: &AuthenticatedOwner) -> Self {
        Self {
            owner_key: owner.key.clone(),
            identity: owner.identity.clone(),
            app_data_root: owner.app_data_root.to_string_lossy().into_owned(),
        }
    }
}

pub async fn load_owner_desired_state(owner_key: &str) -> Result<DesiredState> {
    let path = service_paths()
        .for_owner_key(owner_key)
        .desired_state_path();
    read_json_or_default(&path).await
}

pub async fn persist_owner_core_started(
    owner: &AuthenticatedOwner,
    config: &ClashConfig,
) -> Result<DesiredState> {
    update_owner_desired_state(&owner.key, |state| {
        state.core_should_be_running = true;
        state.last_clash_config = Some(config.clone());
        state.last_writer_config = Some(config.log_config.clone());
    })
    .await
}

pub async fn persist_owner_core_stopped(owner: &AuthenticatedOwner) -> Result<DesiredState> {
    persist_owner_core_stopped_by_key(&owner.key).await
}

pub async fn persist_owner_core_stopped_by_key(owner_key: &str) -> Result<DesiredState> {
    update_owner_desired_state(owner_key, |state| {
        state.core_should_be_running = false;
    })
    .await
}

pub async fn persist_owner_writer_config(
    owner: &AuthenticatedOwner,
    config: &WriterConfig,
) -> Result<DesiredState> {
    update_owner_desired_state(&owner.key, |state| {
        state.last_writer_config = Some(config.clone());
        if let Some(clash_config) = state.last_clash_config.as_mut() {
            clash_config.log_config = config.clone();
        }
    })
    .await
}

pub async fn load_active_owner() -> Result<Option<ActiveOwnerState>> {
    let path = service_paths().active_owner_path();
    secure_state_file_if_exists(&path)?;
    match tokio::fs::read(&path).await {
        Ok(content) => serde_json::from_slice(&content)
            .map(Some)
            .with_context(|| format!("failed to parse active owner {path:?}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read active owner {path:?}")),
    }
}

pub async fn persist_active_owner(owner: &AuthenticatedOwner) -> Result<ActiveOwnerState> {
    let _guard = DESIRED_STATE_LOCK.lock().await;
    let state = ActiveOwnerState::from(owner);
    write_json_atomic(&service_paths().active_owner_path(), &state).await?;
    Ok(state)
}

pub async fn clear_active_owner() -> Result<()> {
    let _guard = DESIRED_STATE_LOCK.lock().await;
    let path = service_paths().active_owner_path();
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove active owner {path:?}")),
    }
}

pub async fn restore_desired_state() -> Result<()> {
    backup_legacy_desired_states().await;

    let Some(active_owner) = load_active_owner().await? else {
        info!("No active owner to restore");
        return Ok(());
    };
    let state = load_owner_desired_state(&active_owner.owner_key).await?;

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
    if let Err(error) = CORE_MANAGER
        .lock()
        .await
        .start_core(config, active_owner.identity.clone())
        .await
    {
        // core 路径不存在通常表示 desired-state 已过期；清掉运行意图，避免重启时反复重试。
        // 其它失败保留意图并交给上层记录。
        if is_not_found_error(&error) {
            warn!(
                "Core binary not found while restoring desired state (stale/translocated path?); \
                 clearing desired core-run state to stop retrying: {error:#}"
            );
            if let Err(clear_error) =
                persist_owner_core_stopped_by_key(&active_owner.owner_key).await
            {
                warn!(
                    "Failed to clear stale desired state after not-found core path: {clear_error:#}"
                );
            }
            set_core_lifecycle_state(ServiceLifecycleState::Running);
            return Ok(());
        }
        set_core_lifecycle_state(ServiceLifecycleState::Fatal);
        return Err(error);
    }
    Ok(())
}

/// 判断错误链中是否包含 NotFound I/O 错误，用于识别失效的 core 路径。
fn is_not_found_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
    })
}

async fn backup_legacy_desired_states() {
    let legacy_files = vec![service_paths().desired_state_path().to_path_buf()];
    #[cfg(target_os = "macos")]
    let legacy_files = legacy_files
        .into_iter()
        .chain([
            std::path::PathBuf::from("/var/lib/clash-verge-service/desired-state.json"),
            std::path::PathBuf::from(
                "/var/root/.local/state/clash-verge-service/desired-state.json",
            ),
        ])
        .collect::<Vec<_>>();

    for legacy in legacy_files {
        match backup_legacy_state_file(&legacy).await {
            Ok(Some(backup)) => info!(
                "Backed up legacy desired-state {:?} -> {:?}",
                legacy, backup
            ),
            Ok(None) => {}
            Err(error) => warn!(
                "Failed to back up legacy desired-state {:?}: {}",
                legacy, error
            ),
        }
    }
}

async fn backup_legacy_state_file(path: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    match tokio::fs::try_exists(path).await {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect legacy state {path:?}"));
        }
    }

    let backup = path.with_extension("json.legacy.bak");
    tokio::fs::rename(path, &backup)
        .await
        .with_context(|| format!("failed to back up legacy state {path:?} to {backup:?}"))?;
    Ok(Some(backup))
}

async fn update_owner_desired_state(
    owner_key: &str,
    update: impl FnOnce(&mut DesiredState),
) -> Result<DesiredState> {
    let _guard = DESIRED_STATE_LOCK.lock().await;
    let path = service_paths()
        .for_owner_key(owner_key)
        .desired_state_path();
    let mut state = read_json_or_default(&path).await?;
    update(&mut state);
    state.generation = state.generation.saturating_add(1);
    state.updated_at = unix_timestamp_secs();
    write_json_atomic(&path, &state).await?;
    Ok(state)
}

async fn read_json_or_default<T>(path: &std::path::Path) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Default,
{
    secure_state_file_if_exists(path)?;
    match tokio::fs::read(path).await {
        Ok(content) => serde_json::from_slice(&content)
            .with_context(|| format!("failed to parse state {path:?}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read state {path:?}")),
    }
}

async fn write_json_atomic<T>(path: &std::path::Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    crate::core::paths::ensure_persistent_state_layout()?;
    if let Some(parent) = path.parent() {
        #[cfg(unix)]
        crate::core::unix_security::ensure_private_service_directory(parent)?;
        #[cfg(windows)]
        crate::core::windows_security::ensure_private_service_directory(parent)?;
    }

    let temp_path = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(value)?;
    tokio::fs::write(&temp_path, json)
        .await
        .with_context(|| format!("failed to write state temp file {temp_path:?}"))?;
    secure_state_file_if_exists(&temp_path)?;
    crate::core::atomic_file::replace(&temp_path, path)
        .await
        .with_context(|| format!("failed to move state into {path:?}"))?;
    secure_state_file_if_exists(path)?;

    Ok(())
}

fn secure_state_file_if_exists(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    crate::core::unix_security::secure_service_file_if_exists(path)?;
    #[cfg(windows)]
    crate::core::windows_security::secure_private_service_file_if_exists(path)?;
    Ok(())
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod owner_tests {
    use super::{
        backup_legacy_state_file, clear_active_owner, load_active_owner, load_owner_desired_state,
        persist_active_owner, persist_owner_core_started, persist_owner_core_stopped,
    };
    use crate::core::auth::AuthenticatedOwner;
    use crate::{ClashConfig, CoreConfig, OwnerIdentity};
    use serial_test::serial;

    fn test_owner(uid: u32) -> AuthenticatedOwner {
        AuthenticatedOwner {
            key: uid.to_string(),
            identity: OwnerIdentity::Unix { uid, gid: 20 },
            app_data_root: std::env::temp_dir(),
        }
    }

    #[tokio::test]
    async fn desired_state_is_scoped_by_owner_key() -> anyhow::Result<()> {
        let owner_a = test_owner(90_001);
        let owner_b = test_owner(90_002);
        let config = ClashConfig {
            core_config: CoreConfig {
                core_path: "/tmp/mock-core-a".to_string(),
                ..Default::default()
            },
            log_config: Default::default(),
        };

        persist_owner_core_started(&owner_a, &config).await?;
        persist_owner_core_stopped(&owner_b).await?;

        assert!(
            load_owner_desired_state(&owner_a.key)
                .await?
                .core_should_be_running
        );
        assert!(
            !load_owner_desired_state(&owner_b.key)
                .await?
                .core_should_be_running
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn active_owner_can_be_atomically_replaced_and_cleared() -> anyhow::Result<()> {
        let owner_a = test_owner(90_003);
        let owner_b = test_owner(90_004);

        persist_active_owner(&owner_a).await?;
        assert_eq!(
            load_active_owner()
                .await?
                .as_ref()
                .map(|state| state.owner_key.as_str()),
            Some("90003")
        );

        persist_active_owner(&owner_b).await?;
        assert_eq!(
            load_active_owner()
                .await?
                .as_ref()
                .map(|state| state.owner_key.as_str()),
            Some("90004")
        );

        clear_active_owner().await?;
        assert!(load_active_owner().await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn legacy_global_state_is_backed_up_without_becoming_owner_state() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "legacy-desired-state-backup-{}",
            std::process::id()
        ));
        let legacy = root.join("desired-state.json");
        let backup = root.join("desired-state.json.legacy.bak");
        std::fs::create_dir_all(&root)?;
        std::fs::write(&legacy, br#"{"core_should_be_running":true}"#)?;

        backup_legacy_state_file(&legacy).await?;

        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read(&backup)?,
            br#"{"core_should_be_running":true}"#
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
