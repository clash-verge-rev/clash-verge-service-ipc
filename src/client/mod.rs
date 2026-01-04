use std::{path::Path, sync::Arc, time::Duration};

use anyhow::Result;
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
            max_retries: 6,
            retry_delay: Duration::from_millis(125),
        }
    }
}

pub async fn set_config(config: Option<IpcConfig>) {
    let mut guard = CLIENT_CONFIG.write().await;
    *guard = config;
}

async fn try_connect() -> Result<IpcHttpClient> {
    debug!("Connecting to IPC at {}", IPC_PATH);

    //? Maybe we should check the path existence outside?
    // if let Err(e) = Path::metadata(IPC_PATH.as_ref()) {
    //     warn!("Clash Verge Service IPC path does not exist: {}", e);
    //     return Err(anyhow::anyhow!(
    //         "Clash Verge Service IPC path does not exist: {}",
    //         e
    //     ));
    // }

    let c = { CLIENT_CONFIG.read().await.clone() }.unwrap_or_default();
    debug!("Using config: {:?}", c);
    let client = kode_bridge::IpcHttpClient::with_config(
        IPC_PATH,
        ClientConfig {
            default_timeout: c.default_timeout,
            max_retries: c.max_retries,
            retry_delay: c.retry_delay,
            enable_pooling: false,
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

pub async fn connect() -> Result<IpcHttpClient> {
    try_connect().await
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
    let response = client
        .post(IpcCommand::StartClash.as_ref())
        .json_body(&body.to_json_value()?)
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
    let response = client
        .put(IpcCommand::UpdateWriter.as_ref())
        .json_body(&body.to_json_value()?)
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}
