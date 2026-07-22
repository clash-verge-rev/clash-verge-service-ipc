#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    AuthenticatedRequest, AuthenticatedSessionRequest, IpcCommand, MacosProxyConfig,
    OwnerCredentials, OwnerSessionProof, ProxyApplyOutcome, RuntimeBundle, SERVICE_PROTOCOL_HEADER,
    ServiceErrorCode, ServiceStatusSnapshot, StartClashRequest, StartClashResult, VERSION,
    WriterConfig, connect, load_active_owner, load_owner_desired_state, owner_key,
    restore_desired_state, run_ipc_server, stop_ipc_server,
};
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
struct WireResponse<T> {
    code: u16,
    message: String,
    data: Option<T>,
}

fn protocol_request<T: serde::Serialize>(
    credentials: &OwnerCredentials,
    payload: T,
) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(AuthenticatedRequest {
        credentials: credentials.clone(),
        payload,
    })?)
}

fn session_request<T: serde::Serialize>(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    payload: T,
) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(AuthenticatedSessionRequest {
        credentials: credentials.clone(),
        session: session.clone(),
        payload,
    })?)
}

async fn start_clash(
    credentials: &OwnerCredentials,
    runtime: &RuntimeBundle,
    proposed_session_token: &str,
) -> Result<WireResponse<StartClashResult>> {
    let client = connect().await?;
    let payload = protocol_request(
        credentials,
        StartClashRequest {
            runtime: runtime.clone(),
            proposed_session_token: proposed_session_token.to_owned(),
            macos_proxy: None,
        },
    )?;
    Ok(client
        .post(IpcCommand::StartClash.as_ref())
        .timeout(Duration::from_secs(30))
        .header(SERVICE_PROTOCOL_HEADER, VERSION)
        .json_body(&payload)
        .send()
        .await?
        .json()?)
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
    let client = connect().await?;
    let payload = protocol_request(credentials, ())?;
    Ok(client
        .get(IpcCommand::Status.as_ref())
        .header(SERVICE_PROTOCOL_HEADER, VERSION)
        .json_body(&payload)
        .send()
        .await?
        .json()?)
}

async fn get_clash_logs(credentials: &OwnerCredentials) -> Result<WireResponse<Vec<String>>> {
    let client = connect().await?;
    let payload = protocol_request(credentials, ())?;
    Ok(client
        .get(IpcCommand::GetClashLogs.as_ref())
        .header(SERVICE_PROTOCOL_HEADER, VERSION)
        .json_body(&payload)
        .send()
        .await?
        .json()?)
}

async fn get_clash_log_snapshot(credentials: &OwnerCredentials) -> Result<WireResponse<String>> {
    let client = connect().await?;
    let payload = protocol_request(credentials, ())?;
    Ok(client
        .get(IpcCommand::GetClashLogSnapshot.as_ref())
        .header(SERVICE_PROTOCOL_HEADER, VERSION)
        .json_body(&payload)
        .send()
        .await?
        .json()?)
}

async fn stop_clash(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
) -> Result<WireResponse<()>> {
    let client = connect().await?;
    let payload = session_request(credentials, session, ())?;
    Ok(client
        .delete(IpcCommand::StopClash.as_ref())
        .timeout(Duration::from_secs(30))
        .header(SERVICE_PROTOCOL_HEADER, VERSION)
        .json_body(&payload)
        .send()
        .await?
        .json()?)
}

async fn update_writer(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    writer: &WriterConfig,
) -> Result<WireResponse<()>> {
    let client = connect().await?;
    let payload = session_request(credentials, session, writer.clone())?;
    Ok(client
        .put(IpcCommand::UpdateWriter.as_ref())
        .header(SERVICE_PROTOCOL_HEADER, VERSION)
        .json_body(&payload)
        .send()
        .await?
        .json()?)
}

async fn set_system_proxy(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    proxy: MacosProxyConfig,
) -> Result<WireResponse<ProxyApplyOutcome>> {
    let client = connect().await?;
    let payload = session_request(credentials, session, proxy)?;
    Ok(client
        .put(IpcCommand::SetSystemProxy.as_ref())
        .header(SERVICE_PROTOCOL_HEADER, VERSION)
        .json_body(&payload)
        .send()
        .await?
        .json()?)
}

#[tokio::test]
#[serial]
async fn protected_routes_reject_protocol_mismatch_before_deserialization() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let server_handle = run_ipc_server().await?;
    wait_for_ipc().await?;

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
        core_path: test_bin_path("no_ipc_binary")
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

    assert_eq!(stop_clash(active_owner, &active_session).await?.code, 0);
    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}
