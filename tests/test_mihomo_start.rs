#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use anyhow::Result;
    use clash_verge_service_ipc::{IPC_PATH, run_ipc_server, stop_ipc_server};
    use kode_bridge::IpcHttpClient;
    use std::time::Duration;
    use tokio::time::sleep;
    use tracing::debug;

    fn connect_ipc() -> Result<IpcHttpClient> {
        debug!("Connecting to IPC at {}", IPC_PATH);
        Ok(kode_bridge::IpcHttpClient::new(IPC_PATH)?)
    }

    #[tokio::test]
    async fn test_ipc_connection() {
        // Start the IPC server in a background thread
        tokio::spawn(async {
            run_ipc_server().await.unwrap();
        });

        // Wait up to 1 second for the server to be up, checking every 100ms
        let mut connected = false;
        for _ in 0..10 {
            if connect_ipc().is_ok() {
                connected = true;
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        assert!(
            connected,
            "Should be able to connect to IPC socket within 1 second"
        );

        // Clean up
        stop_ipc_server().await.unwrap();
    }

    #[tokio::test]
    async fn test_mihomo_start_success() {
        // Arrange: set up any required state or mocks

        // Act: call the function to start Mihomo

        // Assert: check that Mihomo started successfully
        // Example: assert_eq!(result, expected_value);
    }

    #[tokio::test]
    async fn test_mihomo_start_failure() {
        // Arrange: set up state to cause failure

        // Act: attempt to start Mihomo

        // Assert: verify that the error is handled as expected
        // Example: assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mihomo_start_with_custom_config() {
        // Arrange: prepare a custom configuration

        // Act: start Mihomo with the custom config

        // Assert: verify Mihomo started with the correct config
        // Example: assert_eq!(config_used, custom_config);
    }
}
