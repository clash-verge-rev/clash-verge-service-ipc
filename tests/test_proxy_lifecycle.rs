#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    AuthenticatedRequest, IpcCommand, MacosProxyConfig, OwnerSessionProof, RuntimeBundle,
    ServiceErrorCode, StartClashRequest, StartClashResult, connect, run_ipc_server,
    set_system_proxy, start_clash, stop_clash, stop_ipc_server,
};
use serde::Deserialize;
use serial_test::serial;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
struct WireResponse<T> {
    code: u16,
    data: Option<T>,
}

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

async fn proxy_barrier_post(action: &str) -> Result<()> {
    let client = connect().await?;
    let response = client
        .post(&format!("/__test/proxy-barrier/{action}"))
        .send()
        .await?;
    anyhow::ensure!(
        response.is_success(),
        "proxy barrier {action} returned HTTP {}",
        response.status()
    );
    Ok(())
}

async fn proxy_barrier_wait(event: &str) -> Result<()> {
    let client = connect().await?;
    let response = client
        .get(&format!("/__test/proxy-barrier/{event}"))
        .send()
        .await?;
    anyhow::ensure!(
        response.is_success(),
        "proxy barrier {event} returned HTTP {}",
        response.status()
    );
    Ok(())
}

#[tokio::test]
#[serial]
async fn client_uses_versioned_session_aware_proxy_lifecycle() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let server_handle = run_ipc_server().await?;
    wait_for_ipc().await?;

    let credentials = common::owner_credentials();
    let bundle = RuntimeBundle {
        yaml: "mode: rule\n".to_owned(),
        assets: vec![],
        core_path: test_bin_path("mock_binary").to_string_lossy().into_owned(),
    };
    let proposed_session_token = "11".repeat(32);
    let start = start_clash(
        &credentials,
        &StartClashRequest {
            runtime: bundle,
            proposed_session_token: proposed_session_token.clone(),
            macos_proxy: Some(MacosProxyConfig::Disabled),
        },
    )
    .await?;
    assert_eq!(start.code, 0, "{}", start.message);
    let generation = start
        .data
        .context("start omitted session")?
        .session
        .generation;
    let session = OwnerSessionProof {
        generation,
        token: proposed_session_token,
    };
    assert_eq!(
        set_system_proxy(&credentials, &session, &MacosProxyConfig::Disabled)
            .await?
            .code,
        0
    );
    assert_eq!(stop_clash(&credentials, &session).await?.code, 0);

    let client = connect().await?;
    let payload = serde_json::to_value(AuthenticatedRequest {
        credentials,
        payload: StartClashRequest {
            runtime: RuntimeBundle {
                yaml: "mode: rule\n".to_owned(),
                assets: vec![],
                core_path: test_bin_path("mock_binary").to_string_lossy().into_owned(),
            },
            proposed_session_token: "22".repeat(32),
            macos_proxy: None,
        },
    })?;
    let response = client
        .post(IpcCommand::StartClash.as_ref())
        .json_body(&payload)
        .send()
        .await?
        .json::<WireResponse<StartClashResult>>()?;
    assert_eq!(response.code, ServiceErrorCode::ProtocolMismatch as u16);
    assert!(response.data.is_none());

    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn owner_b_cannot_overtake_owner_a_proxy_operation() -> Result<()> {
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let server_handle = run_ipc_server().await?;
    wait_for_ipc().await?;

    let owner_a = clash_verge_service_ipc::test_owner_credentials_for_uid(
        &std::env::temp_dir().join(format!("service-ipc-proxy-race-{}-a", std::process::id())),
        97_001,
    )?;
    let owner_b = clash_verge_service_ipc::test_owner_credentials_for_uid(
        &std::env::temp_dir().join(format!("service-ipc-proxy-race-{}-b", std::process::id())),
        97_002,
    )?;
    let bundle = RuntimeBundle {
        yaml: "mode: rule\n".to_owned(),
        assets: vec![],
        core_path: test_bin_path("mock_binary").to_string_lossy().into_owned(),
    };

    let token_a = "aa".repeat(32);
    let start_a = start_clash(
        &owner_a,
        &StartClashRequest {
            runtime: bundle.clone(),
            proposed_session_token: token_a.clone(),
            macos_proxy: None,
        },
    )
    .await?;
    assert_eq!(start_a.code, 0, "{}", start_a.message);
    let session_a = OwnerSessionProof {
        generation: start_a
            .data
            .context("owner A start omitted session")?
            .session
            .generation,
        token: token_a,
    };

    proxy_barrier_post("arm").await?;
    let proxy_owner = owner_a.clone();
    let proxy_session = session_a.clone();
    let proxy_task = tokio::spawn(async move {
        set_system_proxy(&proxy_owner, &proxy_session, &MacosProxyConfig::Disabled).await
    });
    proxy_barrier_wait("proxy-entered").await?;

    let token_b = "bb".repeat(32);
    let start_owner = owner_b.clone();
    let start_bundle = bundle.clone();
    let start_task = tokio::spawn(async move {
        start_clash(
            &start_owner,
            &StartClashRequest {
                runtime: start_bundle,
                proposed_session_token: token_b,
                macos_proxy: None,
            },
        )
        .await
    });
    proxy_barrier_wait("start-waiting").await?;
    assert!(
        !start_task.is_finished(),
        "owner B completed while owner A held the lifecycle lock"
    );

    proxy_barrier_post("release").await?;
    assert_eq!(proxy_task.await??.code, 0);
    let start_b = start_task.await??;
    assert_eq!(start_b.code, 0, "{}", start_b.message);
    let session_b = OwnerSessionProof {
        generation: start_b
            .data
            .context("owner B start omitted session")?
            .session
            .generation,
        token: "bb".repeat(32),
    };

    assert_eq!(
        set_system_proxy(&owner_a, &session_a, &MacosProxyConfig::Disabled)
            .await?
            .code,
        ServiceErrorCode::StaleOwnerSession as u16
    );
    assert_eq!(
        stop_clash(&owner_a, &session_a).await?.code,
        ServiceErrorCode::StaleOwnerSession as u16
    );

    proxy_barrier_post("reset").await?;
    assert_eq!(stop_clash(&owner_b, &session_b).await?.code, 0);
    stop_ipc_server().await?;
    server_handle.await??;
    Ok(())
}
