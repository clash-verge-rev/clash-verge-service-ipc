use clash_verge_service_ipc::{OwnerCredentials, test_owner_credentials};

// Each integration-test binary uses a different subset of these helpers.
#[allow(dead_code)]
pub fn owner_credentials() -> OwnerCredentials {
    let app_data_dir =
        std::env::temp_dir().join(format!("service-ipc-owner-{}", std::process::id()));
    test_owner_credentials(&app_data_dir)
        .expect("test owner credentials should be secured for the current user")
}

/// Returns the path to a helper binary built beside the integration tests.
#[allow(dead_code)]
pub fn test_bin_path(name: &str) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("debug");
    path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path
}

/// Waits for the asynchronously started IPC listener to become ready.
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
