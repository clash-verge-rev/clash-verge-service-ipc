use crate::core::paths::service_paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CoreRuntimeRecord {
    pub(super) pid: u32,
    pub(super) ipc_path: String,
}

pub(super) async fn write_core_runtime_record(record: &CoreRuntimeRecord) -> Result<()> {
    let paths = service_paths();
    if let Some(parent) = paths.core_runtime_path().parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create core runtime directory {:?}", parent))?;
    }

    let json = serde_json::to_vec_pretty(record)?;
    tokio::fs::write(paths.core_runtime_path(), json)
        .await
        .with_context(|| {
            format!(
                "failed to write core runtime record {:?}",
                paths.core_runtime_path()
            )
        })?;

    Ok(())
}

pub(super) async fn read_core_runtime_record() -> Result<Option<CoreRuntimeRecord>> {
    let paths = service_paths();
    let content = match tokio::fs::read(paths.core_runtime_path()).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read core runtime record {:?}",
                    paths.core_runtime_path()
                )
            });
        }
    };

    match serde_json::from_slice(&content) {
        Ok(record) => Ok(Some(record)),
        Err(error) => {
            warn!(
                "Ignoring invalid core runtime record {:?}: {}",
                paths.core_runtime_path(),
                error
            );
            Ok(None)
        }
    }
}

pub(super) async fn remove_core_runtime_record() {
    let paths = service_paths();
    let _ = tokio::fs::remove_file(paths.core_runtime_path()).await;
}

pub(super) async fn is_core_socket_reachable(path: &str) -> bool {
    #[cfg(unix)]
    {
        tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::UnixStream::connect(path),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    }

    #[cfg(windows)]
    {
        tokio::time::timeout(Duration::from_millis(300), async {
            tokio::net::windows::named_pipe::ClientOptions::new().open(path)
        })
        .await
        .is_ok_and(|result| result.is_ok())
    }
}

pub(super) async fn cleanup_core_socket(path: &str) {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(path);
        if path.exists() {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    #[cfg(windows)]
    {
        let _ = path;
    }
}
