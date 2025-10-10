use std::{sync::Arc, time::Duration};

use anyhow::Result;
use kode_bridge::{ClientConfig, IpcHttpClient, pool::PoolConfig};
use log::debug;
use once_cell::sync::Lazy;
use tokio::sync::{Mutex, MutexGuard};

use crate::{
    ClashConfig, IPC_PATH, IpcCommand,
    core::structure::{JsonConvert, Response},
};

static CLIENT: Lazy<Arc<Mutex<Option<IpcHttpClient>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));

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

pub async fn connect(config: Option<IpcConfig>) -> Result<()> {
    debug!("Connecting to IPC at {}", IPC_PATH);
    debug!("Using config: {:?}", config);
    let c = config.unwrap_or_default();

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
    CLIENT.lock().await.replace(client);
    Ok(())
}

pub async fn get_client() -> Result<MutexGuard<'static, Option<IpcHttpClient>>> {
    let guard = CLIENT.lock().await;
    if guard.is_some() {
        Ok(guard)
    } else {
        drop(guard);
        Err(anyhow::anyhow!("IPC client not connected"))
    }
}

pub async fn get_version() -> Result<Response<String>> {
    let client = get_client().await?;
    let response = client
        .as_ref()
        .unwrap()
        .get(IpcCommand::GetVersion.as_ref())
        .send()
        .await?
        .json::<Response<String>>()?;
    Ok(response)
}

pub async fn start_clash(body: &ClashConfig) -> Result<Response<()>> {
    let client = get_client().await?;
    let response = client
        .as_ref()
        .unwrap()
        .post(IpcCommand::StartClash.as_ref())
        .json_body(&body.to_json_value()?)
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}

pub async fn stop_clash() -> Result<Response<()>> {
    let client = get_client().await?;
    let response = client
        .as_ref()
        .unwrap()
        .delete(IpcCommand::StopClash.as_ref())
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}
