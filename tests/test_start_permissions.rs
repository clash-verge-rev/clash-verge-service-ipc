#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use anyhow::Result;
    use clash_verge_service_ipc::{
        IPC_AUTH_EXPECT, IPC_PATH, IpcCommand, run_ipc_server, stop_ipc_server,
    };
    use kode_bridge::IpcHttpClient;
    use serial_test::serial;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tracing::debug;

    async fn connect_ipc() -> Result<IpcHttpClient> {
        debug!("Connecting to IPC at {}", IPC_PATH);
        let client = kode_bridge::IpcHttpClient::new(IPC_PATH)?;
        client
            .get(IpcCommand::Magic.as_ref())
            .header("X-IPC-Magic", IPC_AUTH_EXPECT)
            .send()
            .await?;
        Ok(client)
    }
    #[tokio::test]
    #[serial]
    async fn start_and_check_permissions() {
        let server_handle = run_ipc_server()
            .await
            .expect("Starting IPC server should return Ok");

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
        {
            let actual_perms = permissions.mode() & 0o777;
            assert_eq!(
                actual_perms, 0o666,
                "control socket must be world-connectable"
            );
            let parent = std::path::Path::new(IPC_PATH).parent().unwrap();
            let parent_mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                parent_mode, 0o755,
                "control runtime directory must be root-managed"
            );
        }
        #[cfg(windows)]
        assert!(!permissions.readonly(), "IPC file should not be readonly");

        let client = connect_ipc().await;
        assert!(
            client.is_ok(),
            "Should be able to connect to IPC server after starting"
        );
        let version = client
            .unwrap()
            .get(IpcCommand::GetVersion.as_ref())
            .header("X-IPC-Magic", IPC_AUTH_EXPECT)
            .send()
            .await;
        assert!(
            version.is_ok(),
            "Should receive a response from GetVersion command"
        );

        assert!(
            stop_ipc_server().await.is_ok(),
            "Stopping IPC server after starting should return Ok"
        );

        let res = server_handle.await.unwrap();
        assert!(res.is_ok(), "server should exit cleanly");

        assert!(
            connect_ipc().await.is_err(),
            "Should not be able to connect after stopping IPC server"
        );
    }
}
