#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    IpcCommand, OwnerCredentials, OwnerSessionProof, RuntimeBundle, ServiceErrorCode, StartClashRequest,
    StartClashResult, connect, get_status, start_clash, stop_clash,
};
use common::{start_server, stop_server};
use serde::Deserialize;
use serial_test::serial;

#[derive(Deserialize)]
struct WireResponse {
    code: u16,
}

fn runtime_bundle() -> RuntimeBundle {
    RuntimeBundle {
        yaml: "mode: rule\n".to_owned(),
        assets: Vec::new(),
        remote_providers: Vec::new(),
        core_path: common::test_bin_path("mock_binary").to_string_lossy().into_owned(),
    }
}

async fn start(credentials: &OwnerCredentials, token: &str) -> Result<(StartClashResult, OwnerSessionProof)> {
    let response = start_clash(
        credentials,
        &StartClashRequest {
            runtime: runtime_bundle(),
            proposed_session_token: token.to_owned(),
            macos_proxy: None,
        },
    )
    .await?;
    anyhow::ensure!(response.code == 0, "{}", response.message);
    let result = response.data.context("start omitted its result")?;
    let session = OwnerSessionProof {
        generation: result.session.generation,
        token: token.to_owned(),
    };
    Ok((result, session))
}

#[tokio::test]
#[serial]
async fn protocol_mismatch_is_rejected_before_payload_deserialization() -> Result<()> {
    let server = start_server().await?;
    let response = connect()
        .await?
        .post(IpcCommand::StartClash.as_ref())
        .json_body(&serde_json::Value::String("invalid request".to_owned()))
        .send()
        .await?
        .json::<WireResponse>()?;

    assert_eq!(response.code, ServiceErrorCode::ProtocolMismatch as u16);
    stop_server(server).await
}

#[tokio::test]
#[serial]
async fn restarting_an_owner_invalidates_the_previous_session() -> Result<()> {
    let server = start_server().await?;
    let credentials = common::owner_credentials();

    let (first, first_session) = start(&credentials, &"11".repeat(32)).await?;
    let (second, second_session) = start(&credentials, &"22".repeat(32)).await?;

    assert!(second.session.generation > first.session.generation);
    assert_eq!(
        stop_clash(&credentials, &first_session).await?.code,
        ServiceErrorCode::StaleOwnerSession as u16
    );
    let status = get_status(&credentials).await?.data.context("status omitted data")?;
    assert!(status.is_active);
    assert!(status.core_pid.is_some());
    assert_eq!(stop_clash(&credentials, &second_session).await?.code, 0);

    stop_server(server).await
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn a_new_owner_takes_over_and_the_previous_owner_becomes_inactive() -> Result<()> {
    let server = start_server().await?;
    let root = std::env::temp_dir();
    let owner_a = clash_verge_service_ipc::test_owner_credentials_for_uid(
        &root.join(format!("service-ipc-owner-a-{}", std::process::id())),
        91_001,
    )?;
    let owner_b = clash_verge_service_ipc::test_owner_credentials_for_uid(
        &root.join(format!("service-ipc-owner-b-{}", std::process::id())),
        91_002,
    )?;

    let (_, session_a) = start(&owner_a, &"33".repeat(32)).await?;
    let (_, session_b) = start(&owner_b, &"44".repeat(32)).await?;

    assert!(!get_status(&owner_a).await?.data.context("no status")?.is_active);
    assert!(clash_verge_service_ipc::inspect_installation(&[]).await?.core_busy);
    assert!(clash_verge_service_ipc::execution::reserve_sidecar().await.is_err());
    assert!(get_status(&owner_b).await?.data.context("no status")?.is_active);
    assert_eq!(
        stop_clash(&owner_a, &session_a).await?.code,
        ServiceErrorCode::StaleOwnerSession as u16
    );
    assert_eq!(stop_clash(&owner_b, &session_b).await?.code, 0);

    stop_server(server).await
}

#[tokio::test]
#[serial]
async fn installation_query_reports_global_occupancy_and_guards_handoff() -> Result<()> {
    use clash_verge_service_ipc::execution::reserve_sidecar;
    use clash_verge_service_ipc::{CoreAvailability, CoreRequirement, inspect_installation};
    let server = start_server().await?;
    let name = format!("verge-mihomo-alpha{}", std::env::consts::EXE_SUFFIX);
    let directory = clash_verge_service_ipc::service_paths()?.core_dir();
    std::fs::create_dir_all(&directory)?;
    let core = directory.join(&name);
    let previous = std::fs::read(&core).ok();
    if core.exists() {
        std::fs::remove_file(&core)?;
    }
    let mut requirement = CoreRequirement { name, sha256: None };
    assert_eq!(
        inspect_installation(&[requirement.clone()]).await?.cores[0].availability,
        CoreAvailability::Missing
    );
    std::fs::write(&core, b"abc")?;
    requirement.sha256 = Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into());
    let status = inspect_installation(&[requirement.clone()]).await?;
    assert!(status.satisfies(&[requirement.clone()]));
    assert!(status.service_sha256.is_empty());
    requirement.sha256 = Some("00".repeat(32));
    let status = inspect_installation(&[requirement.clone()]).await?;
    assert_eq!(status.cores[0].availability, CoreAvailability::DigestMismatch);
    assert!(!status.satisfies(&[requirement]));
    if let Some(previous) = previous {
        std::fs::write(&core, previous)?;
    } else {
        std::fs::remove_file(&core)?;
    }

    let credentials = common::owner_credentials();
    let (_, session) = start(&credentials, &"aa".repeat(32)).await?;
    assert!(inspect_installation(&[]).await?.core_busy);
    assert!(reserve_sidecar().await.is_err());
    assert_eq!(stop_clash(&credentials, &session).await?.code, 0);
    let sidecar = reserve_sidecar().await?;
    assert!(inspect_installation(&[]).await?.core_busy);
    assert!(start(&credentials, &"bb".repeat(32)).await.is_err());
    drop(sidecar);
    let (_, session) = start(&credentials, &"cc".repeat(32)).await?;
    assert_eq!(stop_clash(&credentials, &session).await?.code, 0);
    stop_server(server).await
}

#[tokio::test]
#[serial]
async fn retrying_start_keeps_the_session_and_rejects_conflicting_token_reuse() -> Result<()> {
    let server = start_server().await?;
    let credentials = common::owner_credentials();
    let token = "71".repeat(32);
    let (first, session) = start(&credentials, &token).await?;
    let before = get_status(&credentials).await?.data.context("no status")?;
    let (replayed, _) = start(&credentials, &token).await?;
    assert_eq!(first, replayed);
    assert_eq!(
        get_status(&credentials).await?.data.context("no status")?.core_pid,
        before.core_pid
    );
    let mut changed = runtime_bundle();
    changed.yaml = "mode: direct\n".into();
    assert_eq!(
        start_clash(
            &credentials,
            &StartClashRequest {
                runtime: changed,
                proposed_session_token: token,
                macos_proxy: None,
            }
        )
        .await?
        .code,
        400
    );
    assert_eq!(
        get_status(&credentials)
            .await?
            .data
            .context("no status")?
            .active_generation,
        before.active_generation
    );
    assert_eq!(stop_clash(&credentials, &session).await?.code, 0);
    stop_server(server).await
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn start_replay_after_client_disconnect_does_not_replace_the_committed_core() -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let server = start_server().await?;
    let credentials = common::owner_credentials();
    let token = "72".repeat(32);
    let request = StartClashRequest {
        runtime: runtime_bundle(),
        proposed_session_token: token.clone(),
        macos_proxy: None,
    };
    let body = serde_json::to_vec(&clash_verge_service_ipc::AuthenticatedRequest {
        credentials: credentials.clone(),
        payload: request.clone(),
    })?;
    let mut stream = tokio::net::UnixStream::connect(clash_verge_service_ipc::IPC_PATH).await?;
    let headers = format!(
        "POST /clash/start HTTP/1.1\r\nHost: localhost\r\nX-IPC-Magic: {}\r\n{}: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        clash_verge_service_ipc::IPC_AUTH_EXPECT,
        clash_verge_service_ipc::SERVICE_PROTOCOL_HEADER,
        clash_verge_service_ipc::ProtocolVersion::current().header_value(),
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(&body).await?;
    common::wait_until("disconnected Start commit", async || {
        get_status(&credentials)
            .await
            .ok()
            .and_then(|reply| reply.data)
            .is_some_and(|status| status.core_pid.is_some())
    })
    .await?;
    drop(stream);
    let before = get_status(&credentials).await?.data.context("no status")?;
    let response = start_clash(&credentials, &request).await?;
    anyhow::ensure!(response.code == 0, "{}", response.message);
    let generation = response.data.context("no start result")?.session.generation;
    assert_eq!(before.active_generation, Some(generation));
    assert_eq!(
        get_status(&credentials).await?.data.context("no status")?.core_pid,
        before.core_pid
    );
    assert_eq!(
        stop_clash(&credentials, &OwnerSessionProof { generation, token })
            .await?
            .code,
        0
    );
    stop_server(server).await
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn failed_recovery_persistence_keeps_live_running_intent_visible() -> Result<()> {
    let server = start_server().await?;
    let app_root = std::env::temp_dir().join(format!("service-live-intent-{}", std::process::id()));
    let credentials = clash_verge_service_ipc::test_owner_credentials_for_uid(&app_root, 93_001)?;
    let path = clash_verge_service_ipc::service_paths()?
        .for_owner(&credentials.identity)
        .desired_state_path();
    if path.is_file() {
        std::fs::remove_file(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    let (_, session) = start(&credentials, &"73".repeat(32)).await?;
    let status = get_status(&credentials).await?.data.context("no status")?;
    assert!(status.core_pid.is_some());
    assert!(status.desired_core_should_be_running);
    assert!(status.desired_persistence_error.is_some());
    assert_eq!(stop_clash(&credentials, &session).await?.code, 0);
    std::fs::remove_dir(&path)?;
    std::fs::remove_dir_all(&app_root)?;
    stop_server(server).await
}
