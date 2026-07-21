#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    RuntimeBundle, ServiceErrorCode, WriterConfig, connect, get_clash_log_snapshot, get_clash_logs,
    get_status, load_active_owner, load_owner_desired_state, owner_key, restore_desired_state,
    run_ipc_server, start_clash, stop_clash, stop_ipc_server, update_writer,
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

#[cfg(unix)]
fn owner_credentials_for_uid(name: &str, uid: u32) -> clash_verge_service_ipc::OwnerCredentials {
    let app_data_dir =
        std::env::temp_dir().join(format!("service-ipc-owner-{}-{name}", std::process::id()));
    clash_verge_service_ipc::test_owner_credentials_for_uid(&app_data_dir, uid)
        .expect("synthetic test owner credentials should be valid")
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

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn different_owner_takeover_routes_failure_and_restore_are_isolated() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let mut server_handle = run_ipc_server().await?;
    wait_for_ipc().await?;

    let owner_a = owner_credentials_for_uid("a", 91_001);
    let owner_b = owner_credentials_for_uid("b", 91_002);
    let owner_c = owner_credentials_for_uid("c", 91_003);
    let key_a = owner_key(&owner_a.identity);
    let key_b = owner_key(&owner_b.identity);
    let key_c = owner_key(&owner_c.identity);
    let bundle = RuntimeBundle {
        yaml: "mode: rule\n".to_string(),
        assets: vec![],
        core_path: test_bin_path("mock_binary").to_string_lossy().into_owned(),
    };

    let start_a = start_clash(&owner_a, &bundle)
        .await
        .context("initial owner A start request failed")?;
    assert_eq!(start_a.code, 0, "{}", start_a.message);
    let start_b = start_clash(&owner_b, &bundle)
        .await
        .context("owner B takeover request failed")?;
    assert_eq!(start_b.code, 0, "{}", start_b.message);
    assert_eq!(
        load_active_owner().await?.map(|owner| owner.owner_key),
        Some(key_b.clone())
    );
    assert!(
        !load_owner_desired_state(&key_a)
            .await?
            .core_should_be_running
    );
    assert!(
        load_owner_desired_state(&key_b)
            .await?
            .core_should_be_running
    );

    let inactive_status = get_status(&owner_a)
        .await?
        .data
        .context("inactive status omitted data")?;
    assert!(!inactive_status.is_active);
    assert_eq!(inactive_status.core_pid, None);
    assert_eq!(
        stop_clash(&owner_a).await?.code,
        ServiceErrorCode::NotActive as u16
    );
    assert_eq!(
        get_clash_logs(&owner_a).await?.code,
        ServiceErrorCode::NotActive as u16
    );
    assert_eq!(
        get_clash_log_snapshot(&owner_a).await?.code,
        ServiceErrorCode::NotActive as u16
    );
    assert_eq!(
        update_writer(&owner_a, &WriterConfig::default())
            .await?
            .code,
        ServiceErrorCode::NotActive as u16
    );

    assert_eq!(
        start_clash(&owner_a, &bundle)
            .await
            .context("owner A reactivation request failed")?
            .code,
        0
    );
    let no_ipc_bundle = RuntimeBundle {
        core_path: test_bin_path("no_ipc_binary")
            .to_string_lossy()
            .into_owned(),
        ..bundle.clone()
    };
    assert_eq!(
        start_clash(&owner_c, &no_ipc_bundle)
            .await
            .context("owner C failing takeover request failed")?
            .code,
        ServiceErrorCode::OwnerSwitchFailed as u16
    );
    assert!(load_active_owner().await?.is_none());
    assert!(
        !load_owner_desired_state(&key_a)
            .await?
            .core_should_be_running
    );
    assert!(
        !load_owner_desired_state(&key_c)
            .await?
            .core_should_be_running
    );

    let (start_a, start_b) = tokio::join!(
        start_clash(&owner_a, &bundle),
        start_clash(&owner_b, &bundle)
    );
    assert_eq!(start_a?.code, 0);
    assert_eq!(start_b?.code, 0);
    let active_key = load_active_owner()
        .await?
        .context("concurrent starts did not persist an active owner")?
        .owner_key;
    assert!(active_key == key_a || active_key == key_b);
    let inactive_owner = if active_key == key_a {
        &owner_b
    } else {
        &owner_a
    };
    let active_owner = if active_key == key_a {
        &owner_a
    } else {
        &owner_b
    };
    let active_status = get_status(active_owner)
        .await?
        .data
        .context("active status omitted data")?;
    let inactive_status = get_status(inactive_owner)
        .await?
        .data
        .context("inactive status omitted data")?;
    assert!(active_status.is_active);
    assert!(active_status.core_pid.is_some());
    assert!(!inactive_status.is_active);
    assert_eq!(inactive_status.core_pid, None);
    assert_eq!(
        usize::from(active_status.desired_core_should_be_running)
            + usize::from(inactive_status.desired_core_should_be_running),
        1
    );

    stop_ipc_server().await?;
    server_handle.await??;
    restore_desired_state().await?;
    server_handle = run_ipc_server().await?;
    wait_for_ipc().await?;
    let restored = get_status(active_owner)
        .await?
        .data
        .context("restored status omitted data")?;
    let not_restored = get_status(inactive_owner)
        .await?
        .data
        .context("inactive restored status omitted data")?;
    assert!(restored.is_active);
    assert!(restored.core_pid.is_some());
    assert!(!not_restored.is_active);
    assert_eq!(not_restored.core_pid, None);

    assert_eq!(stop_clash(active_owner).await?.code, 0);
    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}
