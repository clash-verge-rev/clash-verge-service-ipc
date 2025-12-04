#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use clash_verge_service_ipc::{VERSION, run_ipc_server, stop_ipc_server};
    use clash_verge_service_ipc::{connect, get_version};
    use serial_test::serial;
    use tokio::time;

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

        time::sleep(std::time::Duration::from_millis(100)).await;

        let client = connect().await;
        assert!(
            client.is_ok(),
            "Should be able to connect to IPC server after starting"
        );

        let version = get_version().await;
        assert!(
            version.is_ok(),
            "Should receive a response from GetVersion command"
        );

        let version_data = version.unwrap().data;
        assert!(version_data.is_some(), "Version data should not be None");

        let version = version_data.unwrap();
        assert!(
            version == VERSION,
            "Version data should match expected VERSION constant"
        );

        let mock_version = "mock_version_1.0.0";
        assert!(
            mock_version != version,
            "Version should not match mock version"
        );

        let _ = server_handle.await;
    }
}
