use clash_verge_service_ipc::{OwnerCredentials, test_owner_credentials};

// Each test binary compiles this module separately and uses a different part of it; a helper one
// binary does not need is not an unused helper.
#[allow(dead_code)]
pub fn owner_credentials() -> OwnerCredentials {
    let app_data_dir =
        std::env::temp_dir().join(format!("service-ipc-owner-{}", std::process::id()));
    test_owner_credentials(&app_data_dir)
        .expect("test owner credentials should be secured for the current user")
}

/// Where `cargo` put a helper binary this test needs to spawn.
///
/// The mock core, crashing core, and owner-lock probe are built beside the tests.
#[allow(dead_code)]
pub fn test_bin_path(name: &str) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("debug");
    path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path
}

/// Wait until the service is answering on its socket.
///
/// `run_ipc_server` returns before the listener has necessarily bound its socket.
#[allow(dead_code)]
pub async fn wait_for_ipc() -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if clash_verge_service_ipc::connect().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    anyhow::bail!("IPC server did not become ready")
}
