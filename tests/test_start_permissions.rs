#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use kode_bridge::IpcHttpClient;
    use serial_test::serial;
    use anyhow::Result;
    use clash_verge_service_ipc::{IPC_PATH, IpcCommand, run_ipc_server, stop_ipc_server};
    use tracing::debug;

    async fn connect_ipc() -> Result<IpcHttpClient> {
        debug!("Connecting to IPC at {}", IPC_PATH);
        let client = kode_bridge::IpcHttpClient::new(IPC_PATH)?;
        client.get(IpcCommand::Magic.as_ref()).send().await?;
        Ok(client)
    }
    #[tokio::test]
    #[serial]
    async fn start_and_stop_ipc_server_helper() {
        let server_handle = tokio::spawn(async {
            assert!(
                run_ipc_server().await.is_ok(),
                "Starting IPC server should return Ok"
            );
        });

        let client = {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            connect_ipc().await
        };

        assert!(
            client.is_ok(),
            "Should be able to connect to IPC server after starting"
        );

        let permision = std::fs::metadata(IPC_PATH).expect("Failed to get metadata");
        let permissions = permision.permissions();
        #[cfg(unix)]
        assert_eq!(permissions.mode() & 0o777, 0o777, "IPC file permissions should be 777");
        #[cfg(windows)]
        assert!(permissions.readonly() == false, "IPC file should not be readonly");

        assert!(
            stop_ipc_server().await.is_ok(),
            "Stopping IPC server after starting should return Ok"
        );

        let _ = server_handle.await;

        assert!(
            connect_ipc().await.is_err(),
            "Should not be able to connect after stopping IPC server"
        );
    }
}