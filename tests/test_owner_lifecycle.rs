#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
#[cfg(windows)]
use clash_verge_service_ipc::service_paths;
use clash_verge_service_ipc::{
    IpcCommand, OwnerCredentials, OwnerSessionProof, RuntimeBundle, ServiceErrorCode,
    ServiceStatusSnapshot, StartClashRequest, StartClashResult, connect,
    get_status as client_get_status, load_active_owner, load_owner_desired_state, owner_key,
    run_ipc_server, start_clash as client_start_clash, stop_clash as client_stop_clash,
    stop_ipc_server,
};
#[cfg(unix)]
use clash_verge_service_ipc::{
    MacosProxyConfig, ProxyApplyOutcome, WriterConfig,
    get_clash_log_snapshot as client_get_clash_log_snapshot,
    get_clash_logs as client_get_clash_logs, restore_desired_state,
    set_system_proxy as client_set_system_proxy, update_writer as client_update_writer,
};
use serde::Deserialize;
use serial_test::serial;
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::process::{Child, Command, Stdio};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
const RUNTIME_LOCK_TARGET_ENV: &str = "SERVICE_IPC_TEST_RUNTIME_LOCK_TARGET";
#[cfg(windows)]
const RUNTIME_LOCK_READY_ENV: &str = "SERVICE_IPC_TEST_RUNTIME_LOCK_READY";
#[cfg(windows)]
static RUNTIME_LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn owner_credentials_for_uid(name: &str, uid: u32) -> clash_verge_service_ipc::OwnerCredentials {
    let app_data_dir =
        std::env::temp_dir().join(format!("service-ipc-owner-{}-{name}", std::process::id()));
    clash_verge_service_ipc::test_owner_credentials_for_uid(&app_data_dir, uid)
        .expect("synthetic test owner credentials should be valid")
}

#[cfg(windows)]
#[test]
#[ignore = "helper process launched by the Windows runtime lock lifecycle test"]
fn windows_runtime_file_lock_holder() -> Result<()> {
    use std::io::Read as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let Some(target) = std::env::var_os(RUNTIME_LOCK_TARGET_ENV).map(PathBuf::from) else {
        // Keep `cargo test -- --ignored` useful; the parent process supplies this
        // variable when this test is acting as the lock-holder helper.
        return Ok(());
    };
    let ready = std::env::var_os(RUNTIME_LOCK_READY_ENV)
        .map(PathBuf::from)
        .context("runtime lock ready path was not provided")?;
    let _locked_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&target)
        .with_context(|| format!("failed to lock runtime file {target:?}"))?;

    std::fs::write(&ready, b"ready")
        .with_context(|| format!("failed to report runtime lock readiness at {ready:?}"))?;
    let _ = std::io::stdin().read(&mut [0])?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct WireResponse<T> {
    code: u16,
    message: String,
    data: Option<T>,
}

async fn start_clash(
    credentials: &OwnerCredentials,
    runtime: &RuntimeBundle,
    proposed_session_token: &str,
) -> Result<WireResponse<StartClashResult>> {
    let response = client_start_clash(
        credentials,
        &StartClashRequest {
            runtime: runtime.clone(),
            proposed_session_token: proposed_session_token.to_owned(),
            macos_proxy: None,
        },
    )
    .await?;
    Ok(WireResponse {
        code: response.code,
        message: response.message,
        data: response.data,
    })
}

#[cfg(windows)]
struct RuntimeFileLockHolder {
    child: Option<Child>,
    ready: PathBuf,
}

#[cfg(windows)]
impl RuntimeFileLockHolder {
    async fn spawn(target: &std::path::Path) -> Result<Self> {
        let sequence = RUNTIME_LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let ready = std::env::temp_dir().join(format!(
            "service-ipc-runtime-lock-ready-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::remove_file(&ready) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let child = Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "windows_runtime_file_lock_holder",
                "--nocapture",
            ])
            .env(RUNTIME_LOCK_TARGET_ENV, target)
            .env(RUNTIME_LOCK_READY_ENV, &ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn runtime file lock holder")?;
        let mut holder = Self {
            child: Some(child),
            ready,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if holder.ready.is_file() {
                std::fs::remove_file(&holder.ready)?;
                return Ok(holder);
            }
            if let Some(status) = holder
                .child
                .as_mut()
                .context("runtime file lock holder is missing")?
                .try_wait()?
            {
                anyhow::bail!(
                    "runtime file lock holder exited with {status} before reporting ready"
                );
            }
            if Instant::now() >= deadline {
                anyhow::bail!("runtime file lock holder did not report ready within 5 seconds");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn release(mut self) -> Result<()> {
        let stdin = self
            .child
            .as_mut()
            .context("runtime file lock holder is missing")?
            .stdin
            .take()
            .context("runtime file lock holder stdin is missing")?;
        drop(stdin);
        self.wait_for_exit().await
    }

    async fn wait_for_exit(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let child = self
                .child
                .as_mut()
                .context("runtime file lock holder was already released")?;
            if let Some(status) = child.try_wait()? {
                self.child.take();
                anyhow::ensure!(
                    status.success(),
                    "runtime file lock holder failed with {status}"
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("runtime file lock holder did not exit within 5 seconds");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(windows)]
impl Drop for RuntimeFileLockHolder {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.ready);
    }
}

#[cfg(windows)]
async fn start_clash_while_runtime_file_is_locked(
    credentials: &OwnerCredentials,
    runtime: &RuntimeBundle,
    proposed_session_token: &str,
    locked_file: &std::path::Path,
) -> Result<(WireResponse<StartClashResult>, RuntimeFileLockHolder)> {
    let holder = RuntimeFileLockHolder::spawn(locked_file).await?;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        start_clash_ok(credentials, runtime, proposed_session_token),
    )
    .await
    .context("Start waited for the stale runtime file lock to be released")??;
    Ok((response, holder))
}

#[cfg(windows)]
async fn wait_for_path_removal(path: &std::path::Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::fs::symlink_metadata(path).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Ok(_) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect stale runtime path {path:?}"));
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("stale runtime path was not removed within 5 seconds: {path:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(windows)]
fn windows_runtime_bundle(yaml: &str) -> Result<RuntimeBundle> {
    let mock_binary = common::test_bin_path("mock_binary");
    anyhow::ensure!(
        mock_binary.exists(),
        "missing mock_binary at {mock_binary:?}"
    );
    Ok(RuntimeBundle {
        yaml: yaml.to_owned(),
        assets: vec![],
        remote_providers: Vec::new(),
        core_path: mock_binary.to_string_lossy().into_owned(),
    })
}

#[cfg(windows)]
async fn start_clash_ok(
    credentials: &OwnerCredentials,
    runtime: &RuntimeBundle,
    proposed_session_token: &str,
) -> Result<WireResponse<StartClashResult>> {
    let response = start_clash(credentials, runtime, proposed_session_token).await?;
    anyhow::ensure!(response.code == 0, "{}", response.message);
    Ok(response)
}

#[cfg(windows)]
async fn active_runtime_config_path(credentials: &OwnerCredentials) -> Result<PathBuf> {
    let key = owner_key(&credentials.identity);
    let desired = load_owner_desired_state(&key).await?;
    desired
        .last_clash_config
        .map(|config| PathBuf::from(config.core_config.config_path))
        .context("active owner desired state omitted ClashConfig")
}

#[cfg(windows)]
async fn assert_new_runtime_config(
    credentials: &OwnerCredentials,
    previous: &std::path::Path,
    expected_yaml: &str,
) -> Result<PathBuf> {
    let current = active_runtime_config_path(credentials).await?;
    // The path is deliberately the same one: an owner keeps a single generation, so that the core
    // keeps the state it writes there — `cache.db`, and with it the node the user picked. What a
    // start must replace is the configuration inside it, not the directory around it.
    anyhow::ensure!(
        current == previous,
        "same-owner Start moved the runtime generation from {previous:?} to {current:?}"
    );
    anyhow::ensure!(
        std::fs::read_to_string(&current)? == expected_yaml,
        "active runtime config at {current:?} was not replaced"
    );
    Ok(current)
}

#[cfg(windows)]
async fn with_windows_ipc_server(
    test: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let mut server_handle = run_ipc_server().await?;
    let test_result = async {
        common::wait_for_ipc().await?;
        test.await
    }
    .await;

    let stop_result: Result<()> =
        match tokio::time::timeout(Duration::from_secs(5), stop_ipc_server()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(anyhow::anyhow!(
                "IPC server shutdown did not finish within 5 seconds"
            )),
        };
    let server_result: Result<()> =
        match tokio::time::timeout(Duration::from_secs(5), &mut server_handle).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(error.into()),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => {
                server_handle.abort();
                let _ = server_handle.await;
                Err(anyhow::anyhow!(
                    "IPC server task did not stop within 5 seconds"
                ))
            }
        };

    test_result?;
    stop_result?;
    server_result
}

fn session_from_start(
    response: &WireResponse<StartClashResult>,
    token: &str,
) -> Result<OwnerSessionProof> {
    Ok(OwnerSessionProof {
        generation: response
            .data
            .as_ref()
            .context("start response omitted session")?
            .session
            .generation,
        token: token.to_owned(),
    })
}

async fn get_status(credentials: &OwnerCredentials) -> Result<WireResponse<ServiceStatusSnapshot>> {
    let response = client_get_status(credentials).await?;
    Ok(WireResponse {
        code: response.code,
        message: response.message,
        data: response.data,
    })
}

#[cfg(unix)]
async fn get_clash_logs(credentials: &OwnerCredentials) -> Result<WireResponse<Vec<String>>> {
    let response = client_get_clash_logs(credentials).await?;
    Ok(WireResponse {
        code: response.code,
        message: response.message,
        data: response
            .data
            .map(|logs| logs.into_iter().map(Into::into).collect()),
    })
}

#[cfg(unix)]
async fn get_clash_log_snapshot(credentials: &OwnerCredentials) -> Result<WireResponse<String>> {
    let response = client_get_clash_log_snapshot(credentials).await?;
    Ok(WireResponse {
        code: response.code,
        message: response.message,
        data: response.data,
    })
}

async fn stop_clash(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
) -> Result<WireResponse<()>> {
    let response = client_stop_clash(credentials, session).await?;
    Ok(WireResponse {
        code: response.code,
        message: response.message,
        data: response.data,
    })
}

#[cfg(unix)]
async fn update_writer(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    writer: &WriterConfig,
) -> Result<WireResponse<()>> {
    let response = client_update_writer(credentials, session, writer).await?;
    Ok(WireResponse {
        code: response.code,
        message: response.message,
        data: response.data,
    })
}

#[cfg(unix)]
async fn set_system_proxy(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    proxy: MacosProxyConfig,
) -> Result<WireResponse<ProxyApplyOutcome>> {
    let response = client_set_system_proxy(credentials, session, &proxy).await?;
    Ok(WireResponse {
        code: response.code,
        message: response.message,
        data: response.data,
    })
}

#[tokio::test]
#[serial]
async fn protected_routes_reject_protocol_mismatch_before_deserialization() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let server_handle = run_ipc_server().await?;
    common::wait_for_ipc().await?;

    let invalid = serde_json::Value::String("not an authenticated request".to_owned());
    let client = connect().await?;
    let responses = [
        client
            .get(IpcCommand::Status.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
        client
            .post(IpcCommand::StartClash.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
        client
            .delete(IpcCommand::StopClash.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
        client
            .get(IpcCommand::GetClashLogs.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
        client
            .get(IpcCommand::GetClashLogSnapshot.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
        client
            .put(IpcCommand::StageRuntime.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
        client
            .put(IpcCommand::UpdateWriter.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
        client
            .put(IpcCommand::SetSystemProxy.as_ref())
            .json_body(&invalid)
            .send()
            .await?,
    ];
    for response in responses {
        let response = response.json::<WireResponse<()>>()?;
        assert_eq!(response.code, ServiceErrorCode::ProtocolMismatch as u16);
    }

    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}

#[tokio::test]
#[serial]
async fn same_owner_restart_concurrent_start_and_failed_update_remain_atomic() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let server_handle = run_ipc_server().await?;
    common::wait_for_ipc().await?;

    let credentials = common::owner_credentials();
    let mock_binary = common::test_bin_path("mock_binary");
    anyhow::ensure!(
        mock_binary.exists(),
        "missing mock_binary at {mock_binary:?}"
    );
    let bundle = RuntimeBundle {
        yaml: "mode: rule\n".to_string(),
        assets: vec![],
        remote_providers: Vec::new(),
        core_path: mock_binary.to_string_lossy().into_owned(),
    };

    let first_token = "11".repeat(32);
    let first_start = start_clash(&credentials, &bundle, &first_token).await?;
    assert_eq!(first_start.code, 0);
    let first_pid = get_status(&credentials)
        .await?
        .data
        .context("first status omitted data")?
        .core_pid
        .context("first start omitted core PID")?;

    let restart_token = "22".repeat(32);
    let restart = start_clash(&credentials, &bundle, &restart_token).await?;
    assert_eq!(restart.code, 0);
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

    let left_token = "33".repeat(32);
    let right_token = "44".repeat(32);
    let (left, right) = tokio::join!(
        start_clash(&credentials, &bundle, &left_token),
        start_clash(&credentials, &bundle, &right_token)
    );
    let left = left?;
    let right = right?;
    assert_eq!(left.code, 0, "{}", left.message);
    assert_eq!(right.code, 0, "{}", right.message);
    let (active_start, active_token) = if left
        .data
        .as_ref()
        .context("left concurrent start omitted data")?
        .session
        .generation
        > right
            .data
            .as_ref()
            .context("right concurrent start omitted data")?
            .session
            .generation
    {
        (&left, left_token.as_str())
    } else {
        (&right, right_token.as_str())
    };
    let active_session = session_from_start(active_start, active_token)?;

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
        remote_providers: Vec::new(),
        core_path: mock_binary
            .with_file_name("missing-core")
            .to_string_lossy()
            .into_owned(),
        ..bundle
    };
    assert_ne!(
        start_clash(&credentials, &invalid, &"55".repeat(32))
            .await?
            .code,
        0
    );
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

    assert_eq!(stop_clash(&credentials, &active_session).await?.code, 0);
    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
#[serial]
async fn windows_restart_does_not_wait_for_locked_stale_runtime_cleanup() -> Result<()> {
    with_windows_ipc_server(async {
        let credentials = common::owner_credentials();
        let mut bundle = windows_runtime_bundle("mode: rule\n")?;

        let first_token = "a1".repeat(32);
        start_clash_ok(&credentials, &bundle, &first_token).await?;
        let owner_paths = service_paths().for_owner(&credentials.identity);
        let runtime_file = active_runtime_config_path(&credentials).await?;
        anyhow::ensure!(
            runtime_file.is_file(),
            "active runtime config is missing at {runtime_file:?}"
        );

        let stale_generation = owner_paths.root().join("runtime.generation-locked");
        std::fs::create_dir(&stale_generation)?;
        let stale_generation_file = stale_generation.join("stale.lock");
        std::fs::write(&stale_generation_file, b"stale")?;
        bundle.yaml = "mode: direct\n".to_owned();
        let restart_token = "a2".repeat(32);
        let (_, holder) = start_clash_while_runtime_file_is_locked(
            &credentials,
            &bundle,
            &restart_token,
            &stale_generation_file,
        )
        .await?;
        let restarted_runtime_file =
            assert_new_runtime_config(&credentials, &runtime_file, "mode: direct\n").await?;
        tokio::fs::symlink_metadata(&stale_generation)
            .await
            .context("locked stale runtime generation disappeared before handle release")?;
        holder.release().await?;
        wait_for_path_removal(&stale_generation).await?;

        let backup = owner_paths.root().join("runtime.backup");
        std::fs::create_dir(&backup)?;
        let stale_backup_file = backup.join("stale.lock");
        std::fs::write(&stale_backup_file, b"stale")?;
        bundle.yaml = "mode: global\n".to_owned();
        let cleanup_token = "a3".repeat(32);
        let (cleanup_restart, holder) = start_clash_while_runtime_file_is_locked(
            &credentials,
            &bundle,
            &cleanup_token,
            &stale_backup_file,
        )
        .await?;
        assert_new_runtime_config(&credentials, &restarted_runtime_file, "mode: global\n").await?;
        tokio::fs::symlink_metadata(&backup)
            .await
            .context("locked runtime backup disappeared before handle release")?;
        holder.release().await?;
        wait_for_path_removal(&backup).await?;

        let cleanup_session = session_from_start(&cleanup_restart, &cleanup_token)?;
        anyhow::ensure!(
            stop_clash(&credentials, &cleanup_session).await?.code == 0,
            "failed to stop restarted core"
        );
        Ok(())
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn different_owner_takeover_routes_failure_and_restore_are_isolated() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let mut server_handle = run_ipc_server().await?;
    common::wait_for_ipc().await?;

    let owner_a = owner_credentials_for_uid("a", 91_001);
    let owner_b = owner_credentials_for_uid("b", 91_002);
    let owner_c = owner_credentials_for_uid("c", 91_003);
    let key_a = owner_key(&owner_a.identity);
    let key_b = owner_key(&owner_b.identity);
    let key_c = owner_key(&owner_c.identity);
    let bundle = RuntimeBundle {
        yaml: "mode: rule\n".to_string(),
        assets: vec![],
        remote_providers: Vec::new(),
        core_path: common::test_bin_path("mock_binary")
            .to_string_lossy()
            .into_owned(),
    };

    let token_a = "66".repeat(32);
    let start_a = start_clash(&owner_a, &bundle, &token_a)
        .await
        .context("initial owner A start request failed")?;
    assert_eq!(start_a.code, 0, "{}", start_a.message);
    let session_a = session_from_start(&start_a, &token_a)?;
    let token_b = "77".repeat(32);
    let start_b = start_clash(&owner_b, &bundle, &token_b)
        .await
        .context("owner B takeover request failed")?;
    assert_eq!(start_b.code, 0, "{}", start_b.message);
    let session_b = session_from_start(&start_b, &token_b)?;
    assert_eq!(
        update_writer(&owner_b, &session_b, &WriterConfig::default())
            .await?
            .code,
        0
    );
    let proxy = set_system_proxy(&owner_b, &session_b, MacosProxyConfig::Disabled).await?;
    assert_eq!(proxy.code, 0);
    assert_eq!(proxy.data, Some(ProxyApplyOutcome::Applied));
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
        stop_clash(&owner_a, &session_a).await?.code,
        ServiceErrorCode::StaleOwnerSession as u16
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
        update_writer(&owner_a, &session_a, &WriterConfig::default())
            .await?
            .code,
        ServiceErrorCode::StaleOwnerSession as u16
    );
    assert_eq!(
        set_system_proxy(
            &owner_a,
            &session_a,
            MacosProxyConfig::Global {
                host: "127.0.0.1".to_owned(),
                port: 0,
                bypass: String::new(),
            },
        )
        .await?
        .code,
        ServiceErrorCode::StaleOwnerSession as u16
    );

    assert_eq!(
        start_clash(&owner_a, &bundle, &"88".repeat(32))
            .await
            .context("owner A reactivation request failed")?
            .code,
        0
    );
    let no_ipc_bundle = RuntimeBundle {
        remote_providers: Vec::new(),
        core_path: common::test_bin_path("no_ipc_binary")
            .to_string_lossy()
            .into_owned(),
        ..bundle.clone()
    };
    assert_eq!(
        start_clash(&owner_c, &no_ipc_bundle, &"99".repeat(32))
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

    let concurrent_token_a = "aa".repeat(32);
    let concurrent_token_b = "bb".repeat(32);
    let (start_a, start_b) = tokio::join!(
        start_clash(&owner_a, &bundle, &concurrent_token_a),
        start_clash(&owner_b, &bundle, &concurrent_token_b)
    );
    let start_a = start_a?;
    let start_b = start_b?;
    assert_eq!(start_a.code, 0);
    assert_eq!(start_b.code, 0);
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
    let active_session = if active_key == key_a {
        session_from_start(&start_a, &concurrent_token_a)?
    } else {
        session_from_start(&start_b, &concurrent_token_b)?
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
    common::wait_for_ipc().await?;
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

    assert_eq!(stop_clash(active_owner, &active_session).await?.code, 0);
    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}
