use std::{sync::Arc, time::Duration};

use anyhow::Result;
use compact_str::CompactString;
use kode_bridge::{ClientConfig, IpcHttpClient, pool::PoolConfig};
use log::debug;
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

use crate::{ClashConfig, IPC_AUTH_EXPECT, IPC_PATH, IpcCommand, core::structure::Response};

static CLIENT_CONFIG: Lazy<Arc<RwLock<Option<IpcConfig>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

static CACHED_CLIENT: Lazy<Arc<RwLock<Option<Arc<IpcHttpClient>>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

static IPC_AUTH_HEADER_KEY: &str = "X-IPC-Magic";

#[derive(Debug, Clone)]
pub struct IpcConfig {
    pub default_timeout: Duration,
    pub retry_delay: Duration,
    pub max_retries: usize,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_millis(50),
            retry_delay: Duration::from_millis(125),
            max_retries: 6,
        }
    }
}

pub async fn set_config(config: Option<IpcConfig>) {
    let mut guard = CLIENT_CONFIG.write().await;
    *guard = config;
}

async fn try_connect() -> Result<Arc<IpcHttpClient>> {
    debug!("Connecting to IPC at {}", IPC_PATH);
    let c = { CLIENT_CONFIG.read().await.clone() }.unwrap_or_default();
    debug!("Using config: {:?}", c);

    if let Some(cached) = { CACHED_CLIENT.read().await.clone() } {
        return Ok(cached);
    }

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
    client.get(IpcCommand::Magic.as_ref()).send().await?;

    let arc_client = Arc::new(client);
    *CACHED_CLIENT.write().await = Some(arc_client.clone());
    Ok(arc_client)
}

pub async fn connect() -> Result<Arc<IpcHttpClient>> {
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
        .json_body(&serde_json::to_value(body)?)
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
