#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use anyhow::Result;
    use clash_verge_service_ipc::{IPC_PATH, IpcCommand, run_ipc_server, stop_ipc_server};
    use kode_bridge::IpcHttpClient;
    use serial_test::serial;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tracing::debug;

    async fn connect_ipc() -> Result<IpcHttpClient> {
        debug!("Connecting to IPC at {}", IPC_PATH);
        let client = kode_bridge::IpcHttpClient::new(IPC_PATH)?;
        client.get(IpcCommand::Magic.as_ref()).send().await?;
        Ok(client)
    }
    #[tokio::test]
    #[serial]
    async fn start_and_check_permissions() {
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
        {
            use platform_lib::{S_IRWXU, S_IRWXG, S_IRWXO};
            
            let owner_perm = u32::from(S_IRWXU); // 用户权限 (rwx------ = 700)
            let group_perm = u32::from(S_IRWXG); // 组权限   (---rwx--- = 070)
            let other_perm = u32::from(S_IRWXO); // 其他权限 (------rwx = 007)
            let full_mask = owner_perm | group_perm | other_perm; // 完整权限掩码 (rwxrwxrwx = 777)
            
            let actual_perms = permissions.mode() & full_mask;
            
            debug!("IPC file permissions: {:o}", permissions.mode());
            assert_eq!(
                actual_perms,
                full_mask,
                "IPC file permissions should be 777 (actual: {:o})",
                actual_perms
            );
        }
        #[cfg(windows)]
        assert!(!permissions.readonly(), "IPC file should not be readonly");

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
