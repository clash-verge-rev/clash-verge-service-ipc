use anyhow::Result;
use kode_bridge::IpcHttpClient;
use log::debug;

use crate::{IPC_PATH, IpcCommand};

async fn connect() -> Result<IpcHttpClient> {
    debug!("Connecting to IPC at {}", IPC_PATH);
    let client = kode_bridge::IpcHttpClient::new(IPC_PATH)?;
    client.get(IpcCommand::Magic.as_ref()).send().await?;
    Ok(client)
}
