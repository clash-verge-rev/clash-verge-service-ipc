#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use anyhow::Result;
    use clash_verge_service_ipc::{IPC_PATH, IpcCommand, VERSION, run_ipc_server, stop_ipc_server};
    use kode_bridge::IpcHttpClient;
    use serde_json::Value;
    use serial_test::serial;
    use tracing::debug;

    async fn connect_ipc() -> Result<IpcHttpClient> {
        debug!("Connecting to IPC at {}", IPC_PATH);
        let client = kode_bridge::IpcHttpClient::new(IPC_PATH)?;
        client.get(IpcCommand::Magic.as_ref()).send().await?;
        Ok(client)
    }

    #[tokio::test]
    #[serial]
    async fn test_start_and_parse() {
        let _ = stop_ipc_server().await;

        let server_handle = tokio::spawn(async {
            assert!(
                run_ipc_server().await.is_ok(),
                "Starting IPC server should return Ok"
            );
        });

        let client = connect_ipc().await;
        assert!(
            client.is_ok(),
            "Should be able to connect to IPC server after starting"
        );

        let version = client
            .unwrap()
            .get(IpcCommand::GetVersion.as_ref())
            .send()
            .await;
        assert!(
            version.is_ok(),
            "Should receive a response from GetVersion command"
        );

        let version_value: Value = version
            .unwrap()
            .json()
            .expect("Should parse GetVersion response");
        assert!(!version_value.is_null(), "Version value should not be null");

        assert!(
            version_value["data"] == VERSION,
            "Version value should be a string"
        );

        assert!(
            stop_ipc_server().await.is_ok(),
            "Stopping IPC server after starting should return Ok"
        );

        let _ = server_handle.await;
    }
}
