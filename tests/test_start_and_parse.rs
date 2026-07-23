#![cfg(feature = "standalone")]

mod common;

#[cfg(test)]
mod tests {
    use clash_verge_service_ipc::{
        PROTOCOL_EPOCH, PROTOCOL_REVISION, VERSION, connect, get_status, get_version,
        run_ipc_server, stop_ipc_server,
    };
    use serial_test::serial;
    use tokio::task::JoinHandle;
    use tokio::time::{Duration, sleep};

    async fn wait_for_ipc_ready(
        mut handle: JoinHandle<kode_bridge::Result<()>>,
    ) -> JoinHandle<kode_bridge::Result<()>> {
        for _ in 0..40 {
            if connect().await.is_ok() {
                return handle;
            }
            tokio::select! {
                result = &mut handle => panic!("IPC server task exited before readiness: {:?}", result),
                _ = sleep(Duration::from_millis(50)) => {}
            }
        }

        panic!("IPC server did not become reachable before timeout");
    }

    #[tokio::test]
    #[serial]
    async fn test_reinstall_service_needed() {
        #[cfg(unix)]
        {
            use std::fs::File;
            use std::path::Path;

            let _ = stop_ipc_server().await;

            assert!(
                !clash_verge_service_ipc::is_ipc_path_exists(),
                "IPC path should not exist after stopping the server"
            );

            let ipc_path = Path::new(clash_verge_service_ipc::IPC_PATH);
            let _ = std::fs::create_dir(ipc_path.parent().unwrap());
            File::create(ipc_path).unwrap();
            assert!(
                clash_verge_service_ipc::is_ipc_path_exists(),
                "IPC path should exist after creating the file"
            );

            assert!(
                clash_verge_service_ipc::is_reinstall_service_needed().await,
                "Reinstall should be needed when IPC path exists but no server is running"
            );
            std::fs::remove_file(ipc_path).unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn test_start_and_parse() {
        crate::common::init_tracing_for_tests();
        let _ = stop_ipc_server().await;

        let mut server_handle = run_ipc_server()
            .await
            .expect("Starting IPC server should return Ok");

        server_handle = wait_for_ipc_ready(server_handle).await;

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
        assert_eq!(version.build_version, VERSION);
        assert_eq!(version.protocol.epoch, PROTOCOL_EPOCH);
        assert_eq!(version.protocol.revision, PROTOCOL_REVISION);

        let status = get_status(&crate::common::owner_credentials()).await;
        assert!(
            status.is_ok(),
            "Should receive a response from Status command"
        );

        stop_ipc_server().await.unwrap();
        let res = server_handle.await.unwrap();
        assert!(res.is_ok(), "server should exit cleanly");
    }
}
