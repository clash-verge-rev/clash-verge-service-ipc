#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    IpcCommand, OwnerCredentials, OwnerSessionProof, RuntimeBundle, ServiceErrorCode, StartClashRequest,
    StartClashResult, StopOwnedClashRequest, TunGuardConfig, connect, get_status, restore_desired_state, start_clash,
    stop_clash, stop_owned_clash,
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
            tun_guard: None,
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

#[tokio::test]
#[serial]
async fn rejected_tun_guard_preserves_the_current_owner_core() -> Result<()> {
    let server = start_server().await?;
    let credentials = common::owner_credentials();
    let (_, session) = start(&credentials, &"6a".repeat(32)).await?;
    let before = get_status(&credentials)
        .await?
        .data
        .context("status omitted its data")?;

    let mut protected = runtime_bundle();
    protected.yaml =
        "interface-name: test-no-native-network-access\ntun: { enable: true, auto-detect-interface: false }\n".into();
    let rejected = start_clash(
        &credentials,
        &StartClashRequest {
            runtime: protected,
            proposed_session_token: "6b".repeat(32),
            macos_proxy: None,
            tun_guard: Some(TunGuardConfig {
                interface_name: "test-no-native-network-access".into(),
            }),
        },
    )
    .await?;
    assert_eq!(rejected.code, ServiceErrorCode::TunGuardFailed as u16);
    let after = get_status(&credentials)
        .await?
        .data
        .context("status omitted its data")?;
    assert_eq!(after.core_pid, before.core_pid);
    assert_eq!(after.active_generation, before.active_generation);
    assert_eq!(stop_clash(&credentials, &session).await?.code, 0);
    stop_server(server).await?;
    Ok(())
}

#[tokio::test]
#[serial]
async fn a_fresh_client_stops_its_restored_generation_without_a_session() -> Result<()> {
    let server = start_server().await?;
    let credentials = common::owner_credentials();
    let (started, _) = start(&credentials, &"51".repeat(32)).await?;
    stop_server(server).await?;

    let server = start_server().await?;
    restore_desired_state().await?;
    let restored = get_status(&credentials).await?.data.context("status omitted data")?;
    assert_eq!(restored.active_generation, Some(started.session.generation));
    assert!(restored.core_pid.is_some());
    assert_eq!(
        stop_owned_clash(
            &credentials,
            &StopOwnedClashRequest {
                expected_generation: started.session.generation + 1,
            },
        )
        .await?
        .code,
        ServiceErrorCode::StaleOwnerSession as u16,
    );
    let unchanged = get_status(&credentials).await?.data.context("status omitted data")?;
    assert_eq!(unchanged.core_pid, restored.core_pid);
    assert_eq!(unchanged.active_generation, restored.active_generation);
    assert_eq!(unchanged.desired_generation, restored.desired_generation);
    assert!(unchanged.desired_core_should_be_running);

    assert_eq!(
        stop_owned_clash(
            &credentials,
            &StopOwnedClashRequest {
                expected_generation: started.session.generation,
            },
        )
        .await?
        .code,
        0,
    );
    let stopped = get_status(&credentials).await?.data.context("status omitted data")?;
    assert!(!stopped.is_active);
    assert!(stopped.core_pid.is_none());
    assert!(!stopped.desired_core_should_be_running);
    assert!(!clash_verge_service_ipc::inspect_installation(&[]).await?.core_busy);
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
    let owned_before = get_status(&owner_b).await?.data.context("no status")?;
    assert_eq!(
        stop_owned_clash(
            &owner_a,
            &StopOwnedClashRequest {
                expected_generation: owned_before.active_generation.context("no active generation")?,
            },
        )
        .await?
        .code,
        ServiceErrorCode::NotActive as u16,
    );
    let owned_after = get_status(&owner_b).await?.data.context("no status")?;
    assert_eq!(owned_after.core_pid, owned_before.core_pid);
    assert_eq!(owned_after.active_generation, owned_before.active_generation);
    assert_eq!(owned_after.desired_generation, owned_before.desired_generation);
    assert!(owned_after.desired_core_should_be_running);
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
