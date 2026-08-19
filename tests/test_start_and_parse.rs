#![cfg(all(feature = "standalone", feature = "client"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    PROTOCOL_EPOCH, PROTOCOL_REVISION, VERSION, get_status, get_version, run_ipc_server,
    stop_ipc_server,
};
use serial_test::serial;

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn a_stale_ipc_path_requires_service_reinstallation() -> Result<()> {
    let _ = stop_ipc_server().await;
    let ipc_path = std::path::Path::new(clash_verge_service_ipc::IPC_PATH);
    std::fs::create_dir_all(ipc_path.parent().context("IPC path has no parent")?)?;
    std::fs::write(ipc_path, b"")?;

    assert!(clash_verge_service_ipc::is_reinstall_service_needed().await);

    std::fs::remove_file(ipc_path)?;
    Ok(())
}

#[tokio::test]
#[serial]
async fn running_server_reports_its_protocol_and_status() -> Result<()> {
    let _ = stop_ipc_server().await;
    let server = run_ipc_server().await?;
    common::wait_for_ipc().await?;

    let version = get_version().await?.data.context("version omitted data")?;
    assert_eq!(version.build_version, VERSION);
    assert_eq!(version.protocol.epoch, PROTOCOL_EPOCH);
    assert_eq!(version.protocol.revision, PROTOCOL_REVISION);
    assert!(
        get_status(&common::owner_credentials())
            .await?
            .data
            .is_some()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let socket = std::path::Path::new(clash_verge_service_ipc::IPC_PATH);
        assert_eq!(
            std::fs::metadata(socket)?.permissions().mode() & 0o777,
            0o666
        );
        assert_eq!(
            std::fs::metadata(socket.parent().context("IPC path has no parent")?)?
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    stop_ipc_server().await?;
    server.await??;
    Ok(())
}
