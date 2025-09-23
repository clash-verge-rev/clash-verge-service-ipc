#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use anyhow::Result;
    use clash_verge_service_ipc::{IPC_PATH, IpcCommand, run_ipc_server, stop_ipc_server};
    use kode_bridge::IpcHttpClient;
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
    async fn test_stop_ipc_server_when_not_running() {
        assert!(
            stop_ipc_server().await.is_ok(),
            "Stopping IPC server when not running should return Ok"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_connect_ipc_when_server_not_running() {
        let _ = stop_ipc_server().await;
        assert!(
            connect_ipc().await.is_err(),
            "Connecting to IPC when server is not running should return an error"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_start_and_stop_ipc_server() {
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

    //     #[tokio::test]
    //     #[serial]
    //     async fn test_mihomo_start_success() {
    //         // Arrange: set up any required state or mocks

    //         // Act: call the function to start Mihomo

    //         // Assert: check that Mihomo started successfully
    //         // Example: assert_eq!(result, expected_value);
    //     }

    //     #[tokio::test]
    //     #[serial]
    //     async fn test_mihomo_start_failure() {
    //         // Arrange: set up state to cause failure

    //         // Act: attempt to start Mihomo

    //         // Assert: verify that the error is handled as expected
    //         // Example: assert!(result.is_err());
    //     }

    //     #[tokio::test]
    //     #[serial]
    //     async fn test_mihomo_start_with_custom_config() {
    //         // Arrange: prepare a custom configuration

    //         // Act: start Mihomo with the custom config

    //         // Assert: verify Mihomo started with the correct config
    //         // Example: assert_eq!(config_used, custom_config);
    //     }
}
