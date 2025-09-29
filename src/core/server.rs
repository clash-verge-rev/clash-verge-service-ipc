use super::state::IpcState;
use crate::core::manager::CORE_MANAGER;
use crate::{IpcCommand, StartClash, VERSION};
use http::StatusCode;
use kode_bridge::{IpcHttpServer, Result, Router, ipc_http_server::HttpResponse};
use tokio::sync::oneshot;
use tracing::info;

pub async fn run_ipc_server() -> Result<()> {
    make_ipc_dir()?;
    cleanup_ipc_path()?;
    init_ipc_state().await?;

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();

    let state = IpcState::global();
    let guard = state.lock().await;
    guard.set_sender(shutdown_tx).await;
    guard.set_done(done_rx).await;

    let server_arc = guard.get_server();
    drop(guard);

    let mut guard = server_arc.lock().await;
    if let Some(server) = guard.as_mut() {
        let res = tokio::select! {
            res = server.serve() => res,
            _ = &mut shutdown_rx => Ok(()),
        };
        let _ = done_tx.send(());
        res
    } else {
        Err(kode_bridge::KodeBridgeError::configuration(
            "IPC server not initialized".to_string(),
        ))
    }
}

pub async fn stop_ipc_server() -> Result<()> {
    if let Some(sender) = IpcState::global().lock().await.take_sender().await {
        let _ = sender.send(());
    }

    if let Some(done) = IpcState::global().lock().await.take_done().await {
        let _ = done.await;
    }

    {
        let server_arc = IpcState::global().lock().await.get_server();
        let mut guard = server_arc.lock().await;
        if let Some(server) = guard.as_mut() {
            server.shutdown();
        }
        *guard = None;
    }

    cleanup_ipc_path()?;
    Ok(())
}

fn make_ipc_dir() -> Result<()> {
    #[cfg(unix)]
    {
        use crate::IPC_PATH;
        use std::fs;
        #[cfg(unix)]
        use std::fs::set_permissions;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        if let Some(dir_path) = Path::new(IPC_PATH).parent() {
            if !dir_path.exists() {
                fs::create_dir_all(dir_path)?;
            }
            set_permissions(dir_path, fs::Permissions::from_mode(0o777))?;
        }
    }
    #[cfg(windows)]
    {
        // No directory creation needed for Windows named pipes
    }
    Ok(())
}

fn cleanup_ipc_path() -> Result<()> {
    #[cfg(unix)]
    {
        use crate::IPC_PATH;
        use std::{fs, path::Path};

        if Path::new(IPC_PATH).exists() {
            fs::remove_file(IPC_PATH)?;
        }
    }
    #[cfg(windows)]
    {
        // Named pipes on Windows are automatically cleaned up when the last handle is closed
        // No manual cleanup needed
    }
    Ok(())
}

async fn init_ipc_state() -> Result<()> {
    let server = create_ipc_server()?;
    let router = create_ipc_router()?;
    let server = server.router(router);
    IpcState::global().lock().await.set_server(server).await;
    Ok(())
}

fn create_ipc_server() -> Result<IpcHttpServer> {
    use crate::IPC_PATH;
    let server = IpcHttpServer::new(IPC_PATH)?;
    #[cfg(unix)]
    let server = server.with_listener_mode(0o777);
    #[cfg(windows)]
    let server = server.with_listener_security_descriptor("D:(A;;GA;;;WD)");

    Ok(server)
}

fn create_ipc_router() -> Result<Router> {
    let router = Router::new()
        .get(IpcCommand::Magic.as_ref(), |_| async move {
            Ok(HttpResponse::builder().text("Tunglies!").build())
        })
        .get(IpcCommand::GetVersion.as_ref(), |_| async move {
            let json_value = serde_json::json!({
                "code": 0,
                "version": VERSION
            });
            Ok(HttpResponse::builder()
                .status(StatusCode::OK)
                .json(&json_value)?
                .build())
        })
        .post(IpcCommand::StartClash.as_ref(), |payload| async move {
            match payload.json::<StartClash>() {
                Ok(start_clash) => {
                    match CORE_MANAGER.lock().await.start_core(start_clash).await {
                        Ok(_) => info!("Core started successfully"),
                        Err(e) => {
                            let json_value = serde_json::json!({
                                "code": 1,
                                "msg": format!("Failed to start core: {}", e)
                            });
                            return Ok(HttpResponse::builder()
                                .status(StatusCode::SERVICE_UNAVAILABLE)
                                .json(&json_value)?
                                .build());
                        }
                    }

                    let json_value = serde_json::json!({
                        "code": 0,
                        "msg": "Core started successfully"
                    });
                    Ok(HttpResponse::builder()
                        .status(StatusCode::OK)
                        .json(&json_value)?
                        .build())
                }
                Err(e) => Ok(HttpResponse::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .text(format!("Invalid JSON: {}", e))
                    .build()),
            }
        })
        .delete(IpcCommand::StopClash.as_ref(), |_| async move {
            match CORE_MANAGER.lock().await.stop_core().await {
                Ok(_) => info!("Core stopped successfully"),
                Err(e) => {
                    let json_value = serde_json::json!({
                        "code": 1,
                        "msg": format!("Failed to stop core: {}", e)
                    });
                    return Ok(HttpResponse::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .json(&json_value)?
                        .build());
                }
            }

            let json_value = serde_json::json!({
                "code": 0,
                "msg": "Core stopped successfully"
            });

            Ok(HttpResponse::builder()
                .status(StatusCode::OK)
                .json(&json_value)?
                .build())
        });
    Ok(router)
}
