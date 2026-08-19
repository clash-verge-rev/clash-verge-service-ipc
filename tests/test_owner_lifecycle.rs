#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    IpcCommand, OwnerCredentials, OwnerSessionProof, RuntimeBundle, ServiceErrorCode,
    StartClashRequest, StartClashResult, connect, get_status, run_ipc_server, start_clash,
    stop_clash, stop_ipc_server,
};
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
        core_path: common::test_bin_path("mock_binary")
            .to_string_lossy()
            .into_owned(),
    }
}

async fn start(
    credentials: &OwnerCredentials,
    token: &str,
) -> Result<(StartClashResult, OwnerSessionProof)> {
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

async fn start_server() -> Result<tokio::task::JoinHandle<kode_bridge::Result<()>>> {
    let _ = stop_ipc_server().await;
    let server = run_ipc_server().await?;
    common::wait_for_ipc().await?;
    Ok(server)
}

async fn stop_server(server: tokio::task::JoinHandle<kode_bridge::Result<()>>) -> Result<()> {
    stop_ipc_server().await?;
    server.await??;
    Ok(())
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
    let status = get_status(&credentials)
        .await?
        .data
        .context("status omitted data")?;
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

    assert!(
        !get_status(&owner_a)
            .await?
            .data
            .context("no status")?
            .is_active
    );
    assert!(
        get_status(&owner_b)
            .await?
            .data
            .context("no status")?
            .is_active
    );
    assert_eq!(
        stop_clash(&owner_a, &session_a).await?.code,
        ServiceErrorCode::StaleOwnerSession as u16
    );
    assert_eq!(stop_clash(&owner_b, &session_b).await?.code, 0);

    stop_server(server).await
}
