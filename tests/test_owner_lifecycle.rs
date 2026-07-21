#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    RuntimeBundle, connect, get_status, load_active_owner, load_owner_desired_state, owner_key,
    run_ipc_server, start_clash, stop_clash, stop_ipc_server,
};
use serial_test::serial;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn test_bin_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("debug");
    path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path
}

async fn wait_for_ipc() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if connect().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("IPC server did not become ready")
}

#[tokio::test]
#[serial]
async fn same_owner_restart_concurrent_start_and_failed_update_remain_atomic() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let server_handle = run_ipc_server().await?;
    wait_for_ipc().await?;

    let credentials = common::owner_credentials();
    let mock_binary = test_bin_path("mock_binary");
    anyhow::ensure!(
        mock_binary.exists(),
        "missing mock_binary at {mock_binary:?}"
    );
    let bundle = RuntimeBundle {
        yaml: "mode: rule\n".to_string(),
        assets: vec![],
        core_path: mock_binary.to_string_lossy().into_owned(),
    };

    assert_eq!(start_clash(&credentials, &bundle).await?.code, 0);
    let first_pid = get_status(&credentials)
        .await?
        .data
        .context("first status omitted data")?
        .core_pid
        .context("first start omitted core PID")?;

    assert_eq!(start_clash(&credentials, &bundle).await?.code, 0);
    let restarted_pid = get_status(&credentials)
        .await?
        .data
        .context("restart status omitted data")?
        .core_pid
        .context("restart omitted core PID")?;
    assert_ne!(
        first_pid, restarted_pid,
        "same-owner Start must restart core"
    );

    let (left, right) = tokio::join!(
        start_clash(&credentials, &bundle),
        start_clash(&credentials, &bundle)
    );
    assert_eq!(left?.code, 0);
    assert_eq!(right?.code, 0);

    let committed = get_status(&credentials)
        .await?
        .data
        .context("concurrent status omitted data")?;
    let committed_pid = committed
        .core_pid
        .context("concurrent Start omitted core PID")?;
    assert!(committed.is_active);
    assert!(committed.desired_core_should_be_running);

    let invalid = RuntimeBundle {
        core_path: mock_binary
            .with_file_name("missing-core")
            .to_string_lossy()
            .into_owned(),
        ..bundle
    };
    assert_ne!(start_clash(&credentials, &invalid).await?.code, 0);
    let after_failure = get_status(&credentials)
        .await?
        .data
        .context("failure status omitted data")?;
    assert_eq!(after_failure.core_pid, Some(committed_pid));
    assert!(after_failure.is_active);

    let key = owner_key(&credentials.identity);
    assert_eq!(
        load_active_owner().await?.map(|owner| owner.owner_key),
        Some(key.clone())
    );
    assert!(load_owner_desired_state(&key).await?.core_should_be_running);

    assert_eq!(stop_clash(&credentials).await?.code, 0);
    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}
