use std::{path::Path, sync::Arc, time::Duration};

#[cfg(windows)]
use anyhow::Result;
#[cfg(unix)]
use anyhow::{Result, anyhow};
use compact_str::CompactString;
use kode_bridge::{ClientConfig, IpcHttpClient};
use log::{debug, warn};
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

use crate::{
    ClashConfig, IPC_AUTH_EXPECT, IPC_PATH, IpcCommand, WriterConfig,
    core::structure::{JsonConvert, Response},
};

static CLIENT_CONFIG: Lazy<Arc<RwLock<Option<IpcConfig>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

static IPC_AUTH_HEADER_KEY: &str = "X-IPC-Magic";

#[derive(Debug, Clone)]
pub struct IpcConfig {
    pub default_timeout: Duration,
    pub max_retries: usize,
    pub retry_delay: Duration,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_millis(50),
            max_retries: 8,
            retry_delay: Duration::from_millis(150),
        }
    }
}

pub async fn set_config(config: Option<IpcConfig>) {
    let mut guard = CLIENT_CONFIG.write().await;
    *guard = config;
}

pub async fn connect() -> Result<IpcHttpClient> {
    debug!("Connecting to IPC at {}", IPC_PATH);

    #[cfg(unix)]
    {
        if let Err(err) = Path::metadata(IPC_PATH.as_ref()) {
            return Err(anyhow!("IPC path unavailable: {err}"));
        }
    }

    let c = { CLIENT_CONFIG.read().await.clone() }.unwrap_or_default();
    debug!("Using config: {:?}", c);
    let client = kode_bridge::IpcHttpClient::with_config(
        IPC_PATH,
        ClientConfig {
            default_timeout: c.default_timeout,
            max_retries: c.max_retries,
            retry_delay: c.retry_delay,
            enable_pooling: true,
            ..Default::default()
        },
    )?;

    if let Err(e) = client
        .get(IpcCommand::Magic.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await
    {
        warn!("Failed to connect to IPC server: {}", e);
        return Err(anyhow::anyhow!("Failed to connect to IPC server: {}", e));
    }

    Ok(client)
}

pub fn is_ipc_path_exists() -> bool {
    Path::new(IPC_PATH).exists()
}

pub async fn get_version() -> Result<Response<String>> {
    let client = connect().await?;
    let response = client
        .get(IpcCommand::GetVersion.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<String>>()?;
    Ok(response)
}

pub async fn is_reinstall_service_needed() -> bool {
    is_ipc_path_exists()
        && match get_version().await {
            Ok(resp) => {
                if let Some(ver) = resp.data {
                    ver != crate::VERSION
                } else {
                    true
                }
            }
            Err(_) => true,
        }
}

pub async fn start_clash(body: &ClashConfig) -> Result<Response<()>> {
    let client = connect().await?;
    let payload = body.to_json_value()?;
    let response = client
        .post(IpcCommand::StartClash.as_ref())
        .json_body(&payload)
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}

pub async fn get_clash_logs() -> Result<Response<Vec<CompactString>>> {
    let client = connect().await?;
    let response = client
        .get(IpcCommand::GetClashLogs.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<Vec<CompactString>>>()?;
    Ok(response)
}

pub async fn stop_clash() -> Result<Response<()>> {
    let client = connect().await?;
    let response = client
        .delete(IpcCommand::StopClash.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}

pub async fn update_writer(body: &WriterConfig) -> Result<Response<()>> {
    let client = connect().await?;
    let payload = body.to_json_value()?;
    let response = client
        .put(IpcCommand::UpdateWriter.as_ref())
        .json_body(&payload)
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}
