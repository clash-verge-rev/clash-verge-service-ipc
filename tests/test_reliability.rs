#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    CoreWatchdogTestConfig, OwnerSessionProof, RuntimeBundle, ServiceLifecycleState, StartClashRequest, connect,
    get_status, run_ipc_supervisor_until_shutdown, service_lifecycle_state, set_core_watchdog_config_for_tests,
    start_clash, stop_clash, stop_ipc_server,
};
use common::{start_server, stop_server, wait_until};
use serial_test::serial;
use std::time::Duration;
use tokio::sync::oneshot;

#[tokio::test]
#[serial]
async fn ipc_supervisor_restarts_a_stopped_listener() -> Result<()> {
    let _ = stop_ipc_server().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let supervisor = tokio::spawn(run_ipc_supervisor_until_shutdown(async {
        let _ = shutdown_rx.await;
    }));

    wait_until("IPC startup", async || {
        connect().await.is_ok() && service_lifecycle_state() == ServiceLifecycleState::Running
    })
    .await?;
    stop_ipc_server().await?;
    wait_until("IPC restart", async || connect().await.is_ok()).await?;

    let _ = shutdown_tx.send(());
    supervisor.await??;
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
#[serial]
async fn sidecar_probe_does_not_inherit_long_client_retries() -> Result<()> {
    let _ = stop_ipc_server().await;
    clash_verge_service_ipc::set_config(Some(clash_verge_service_ipc::IpcConfig {
        default_timeout: Duration::from_secs(1),
        max_retries: 20,
        retry_delay: Duration::from_millis(500),
    }))
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        clash_verge_service_ipc::execution::check_sidecar_available(),
    )
    .await;
    clash_verge_service_ipc::set_config(None).await;
    assert!(
        result.is_ok(),
        "Sidecar inspection inherited the client's long retry budget"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn a_healthy_service_owner_prevents_a_second_instance() -> Result<()> {
    let _ = stop_ipc_server().await;
    let owner = clash_verge_service_ipc::acquire_service_owner()
        .await?
        .context("current process did not acquire the service owner lock")?;
    let server = start_server().await?;

    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(common::test_bin_path("owner_lock_holder")).status(),
    )
    .await
    .context("second owner did not exit")??;
    assert_eq!(status.code(), Some(2));
    assert!(connect().await.is_ok());

    stop_server(server).await?;
    drop(owner);
    Ok(())
}

#[tokio::test]
#[serial]
async fn core_watchdog_stops_a_bounded_crash_loop() -> Result<()> {
    struct ResetWatchdog;
    impl Drop for ResetWatchdog {
        fn drop(&mut self) {
            set_core_watchdog_config_for_tests(None);
        }
    }

    let _reset = ResetWatchdog;
    set_core_watchdog_config_for_tests(Some(CoreWatchdogTestConfig {
        max_restarts: 2,
        restart_window: Duration::from_secs(10),
        max_backoff: Duration::ZERO,
    }));
    let server = start_server().await?;
    let credentials = common::owner_credentials();
    let token = "41".repeat(32);
    let response = start_clash(
        &credentials,
        &StartClashRequest {
            runtime: RuntimeBundle {
                yaml: "mode: rule\n".to_owned(),
                assets: Vec::new(),
                remote_providers: Vec::new(),
                core_path: common::test_bin_path("crash_binary").to_string_lossy().into_owned(),
            },
            proposed_session_token: token.clone(),
            macos_proxy: None,
        },
    )
    .await?;
    anyhow::ensure!(response.code == 0, "{}", response.message);
    let session = OwnerSessionProof {
        generation: response.data.context("start omitted its result")?.session.generation,
        token,
    };

    wait_until("watchdog limit", async || {
        get_status(&credentials)
            .await
            .ok()
            .and_then(|response| response.data)
            .is_some_and(|status| status.restart_count >= 2 && status.core_pid.is_none())
    })
    .await?;
    assert!(
        get_status(&credentials)
            .await?
            .data
            .context("status omitted data")?
            .last_core_exit_reason
            .is_some()
    );

    assert_eq!(stop_clash(&credentials, &session).await?.code, 0);
    stop_server(server).await
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn desired_restore_recreates_the_volatile_owner_ipc_directory() -> Result<()> {
    use clash_verge_service_ipc::{mihomo_ipc_path, restore_desired_state};
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

    let server = start_server().await?;
    let credentials = common::owner_credentials();
    let token = "51".repeat(32);
    let response = start_clash(
        &credentials,
        &StartClashRequest {
            runtime: RuntimeBundle {
                yaml: "mode: direct\n".into(),
                assets: Vec::new(),
                remote_providers: Vec::new(),
                core_path: common::test_bin_path("mock_binary").to_string_lossy().into_owned(),
            },
            proposed_session_token: token.clone(),
            macos_proxy: None,
        },
    )
    .await?;
    anyhow::ensure!(response.code == 0, "{}", response.message);
    let generation = response.data.context("no start result")?.session.generation;
    let before = get_status(&credentials).await?.data.context("no status")?.core_pid;
    stop_server(server).await?;
    let desired_path = clash_verge_service_ipc::service_paths()?
        .for_owner(&credentials.identity)
        .desired_state_path();
    let mut durable: clash_verge_service_ipc::DesiredState = serde_json::from_slice(&std::fs::read(&desired_path)?)?;
    durable.generation += 100;
    std::fs::write(&desired_path, serde_json::to_vec(&durable)?)?;
    let socket = std::path::PathBuf::from(mihomo_ipc_path(&credentials.identity));
    std::fs::remove_dir(socket.parent().context("no parent")?)?;
    std::fs::write(socket.parent().context("no parent")?, b"not a directory")?;
    assert!(restore_desired_state().await.is_err());
    assert!(
        clash_verge_service_ipc::load_owner_desired_state(&clash_verge_service_ipc::owner_key(&credentials.identity),)
            .await?
            .core_should_be_running
    );
    std::fs::remove_file(socket.parent().context("no parent")?)?;
    restore_desired_state().await?;
    let server = start_server_after_restore().await?;
    let status = get_status(&credentials).await?.data.context("no restored status")?;
    assert!(status.core_pid.is_some());
    assert_ne!(status.core_pid, before);
    assert_eq!(status.active_generation, Some(generation));
    assert!(status.desired_core_should_be_running);
    assert_eq!(status.desired_generation, durable.generation);
    std::fs::remove_file(&desired_path)?;
    std::fs::create_dir(&desired_path)?;
    assert!(
        get_status(&credentials)
            .await?
            .data
            .context("no live restored status")?
            .desired_core_should_be_running
    );
    std::fs::remove_dir(&desired_path)?;
    let metadata = std::fs::symlink_metadata(&socket)?;
    assert!(metadata.file_type().is_socket());
    if let clash_verge_service_ipc::OwnerIdentity::Unix { uid, .. } = credentials.identity {
        assert_eq!(metadata.uid(), uid);
    }
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        stop_clash(&credentials, &OwnerSessionProof { generation, token })
            .await?
            .code,
        0
    );
    stop_server(server).await
}

#[cfg(unix)]
async fn start_server_after_restore() -> Result<tokio::task::JoinHandle<kode_bridge::Result<()>>> {
    let server = clash_verge_service_ipc::run_ipc_server().await?;
    wait_until("restored listener", async || connect().await.is_ok()).await?;
    Ok(server)
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn unresponsive_helper_takeover_ignores_a_stale_legacy_pid_file() -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let _ = stop_ipc_server().await;
    let paths = clash_verge_service_ipc::service_paths()?;
    let copied_helper = std::env::temp_dir().join(format!("service-owner-fixture-{}", std::process::id()));
    std::fs::copy(common::test_bin_path("owner_lock_holder"), &copied_helper)?;
    let mut first = tokio::process::Command::new(&copied_helper)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let first_pid = first.id().context("no first helper PID")?;
    owner_fixture_ready(&mut first).await?;
    assert!(
        std::fs::read_to_string(paths.owner_lock_path())?
            .lines()
            .any(|line| line == format!("pid={first_pid}"))
    );
    let inode = std::fs::metadata(paths.owner_lock_path())?.ino();
    let mut unrelated = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()?;
    std::fs::write(
        paths.pid_file_path(),
        unrelated.id().context("no unrelated fixture PID")?.to_string(),
    )?;
    let mut second = tokio::process::Command::new(common::test_bin_path("owner_lock_holder"))
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let second_pid = second.id().context("no second helper PID")?;
    owner_fixture_ready(&mut second).await?;
    assert!(
        std::fs::read_to_string(paths.owner_lock_path())?
            .lines()
            .any(|line| line == format!("pid={second_pid}"))
    );
    assert!(first.wait().await?.code().is_none());
    assert!(unrelated.try_wait()?.is_none(), "stale PID file redirected termination");
    assert!(!paths.pid_file_path().exists());
    assert_eq!(std::fs::metadata(paths.owner_lock_path())?.ino(), inode);
    second.kill().await?;
    unrelated.kill().await?;
    std::fs::remove_file(copied_helper)?;
    Ok(())
}

#[cfg(unix)]
async fn owner_fixture_ready(child: &mut tokio::process::Child) -> Result<()> {
    use tokio::io::AsyncBufReadExt as _;
    let stdout = child.stdout.take().context("fixture stdout is unavailable")?;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
    anyhow::ensure!(line == "owner-ready\n", "fixture failed before ownership was ready");
    Ok(())
}
