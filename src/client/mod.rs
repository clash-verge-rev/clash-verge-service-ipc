use std::sync::Arc;

use anyhow::Result;
use kode_bridge::{ClientConfig, IpcHttpClient};
use log::debug;
use once_cell::sync::Lazy;
use tokio::sync::{Mutex, MutexGuard};

use crate::{
    ClashConfig, IPC_PATH, IpcCommand,
    core::structure::{JsonConvert, Response},
};

static CLIENT: Lazy<Arc<Mutex<Option<IpcHttpClient>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));

pub async fn connect(config: Option<ClientConfig>) -> Result<()> {
    debug!("Connecting to IPC at {}", IPC_PATH);
    let client = kode_bridge::IpcHttpClient::with_config(IPC_PATH, config.unwrap_or_default())?;
    client.get(IpcCommand::Magic.as_ref()).send().await?;
    CLIENT.lock().await.replace(client);
    Ok(())
}

pub async fn get_client() -> Result<MutexGuard<'static, Option<IpcHttpClient>>> {
    let guard = CLIENT.lock().await;
    if guard.is_some() {
        Ok(guard)
    } else {
        // drop(guard);
        // Err(anyhow::anyhow!("IPC client not connected"))
        connect(None).await?;
        Ok(guard)
    }
}

pub async fn get_version() -> Result<Response<String>> {
    let client = get_client().await?;
    let response = client
        .as_ref()
        .unwrap()
        .get(IpcCommand::GetVersion.as_ref())
        .send()
        .await?.json::<Response<String>>()?;
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
