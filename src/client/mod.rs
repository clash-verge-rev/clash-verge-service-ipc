use std::{sync::Arc, time::Duration};

use anyhow::Result;
use compact_str::CompactString;
use kode_bridge::{ClientConfig, IpcHttpClient, pool::PoolConfig};
use log::{debug, warn};
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

use crate::{
    ClashConfig, IPC_AUTH_EXPECT, IPC_PATH, IpcCommand,
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

    // Disabled existence check, see in https://github.com/clash-verge-rev/clash-verge-service-ipc/pull/13#discussion_r2585472857
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
            pool_config: PoolConfig {
                max_retries: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    )?;

    if let Err(e) = client.get(IpcCommand::Magic.as_ref()).send().await {
        warn!("Failed to connect to IPC server: {}", e);
        return Err(anyhow::anyhow!("Failed to connect to IPC server: {}", e));
    }

    Ok(client)
}

pub async fn connect() -> Result<IpcHttpClient> {
    try_connect().await
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
