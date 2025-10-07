use anyhow::Result;
use kode_bridge::{ClientConfig, IpcHttpClient};
use log::debug;

use crate::{IPC_PATH, IpcCommand};

pub async fn connect(config: Option<ClientConfig>) -> Result<IpcHttpClient> {
    debug!("Connecting to IPC at {}", IPC_PATH);
    let client = kode_bridge::IpcHttpClient::with_config(IPC_PATH, config.unwrap_or_default())?;
    client.get(IpcCommand::Magic.as_ref()).send().await?;
    Ok(client)
}
