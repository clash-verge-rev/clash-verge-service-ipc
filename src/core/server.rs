use super::state::IpcState;
use crate::core::manager::{CORE_MANAGER, ClashLogger};
use crate::core::structure::Response;
use crate::{ClashConfig, IpcCommand, VERSION};
use http::StatusCode;
use kode_bridge::{IpcHttpServer, Result, Router, ipc_http_server::HttpResponse};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;

pub async fn run_ipc_server() -> Result<JoinHandle<Result<()>>> {
    make_ipc_dir().await?;
    cleanup_ipc_path().await?;
    init_ipc_state().await?;

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<()>();

    {
        let guard = IpcState::global().lock().await;
        guard.set_sender(shutdown_tx).await;
        guard.set_done(done_rx).await;
    }

    let server_arc = IpcState::global().lock().await.get_server();
    let mut guard = server_arc.lock().await;

    if let Some(mut server) = guard.take() {
        let handle = tokio::spawn(async move {
            let res = tokio::select! {
                res = server.serve() => res,
                _ = &mut shutdown_rx => Ok(()),
            };

            let _ = done_tx.send(());
            res
        });
        #[cfg(unix)]
        {
            use crate::IPC_PATH;
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            use std::time::Duration;
            use tokio::fs;

            tokio::time::sleep(Duration::from_millis(50)).await;
            fs::set_permissions(IPC_PATH, Permissions::from_mode(0o777)).await?;
        }
        Ok(handle)
    } else {
        Err(kode_bridge::KodeBridgeError::configuration(
            "IPC server not initialized".to_string(),
        ))
    }
}

pub async fn stop_ipc_server() -> Result<()> {
    CORE_MANAGER.lock().await.stop_core().await.ok();

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

    cleanup_ipc_path().await?;

    #[cfg(windows)]
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    Ok(())
}

async fn make_ipc_dir() -> Result<()> {
    #[cfg(unix)]
    {
        use crate::IPC_PATH;
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use tokio::fs;

        if let Some(dir_path) = Path::new(IPC_PATH).parent() {
            if !dir_path.exists() {
                fs::create_dir_all(dir_path).await?;
            }
            fs::set_permissions(dir_path, Permissions::from_mode(0o777)).await?;
        }
    }
    #[cfg(windows)]
    {
        // No directory creation needed for Windows named pipes
    }
    Ok(())
}

async fn cleanup_ipc_path() -> Result<()> {
    #[cfg(unix)]
    {
        use crate::IPC_PATH;
        use std::path::Path;
        use tokio::fs;

        if Path::new(IPC_PATH).exists() {
            fs::remove_file(IPC_PATH).await?;
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
    {
        use platform_lib::{S_IRWXG, S_IRWXO, S_IRWXU, mode_t};
        let mode: mode_t = platform_lib::mode_t::from(S_IRWXU | S_IRWXG | S_IRWXO);
        let server = server.with_listener_mode(mode);
        Ok(server)
    }

    #[cfg(windows)]
    {
        let server = server.with_listener_security_descriptor("D:(A;;GA;;;WD)");
        Ok(server)
    }
}
fn create_ipc_router() -> Result<Router> {
    let router = Router::new()
        .get(IpcCommand::Magic.as_ref(), |_| async move {
            Ok(HttpResponse::builder().text("Tunglies!").build())
        })
        .get(IpcCommand::GetVersion.as_ref(), |_| async move {
            let json_value = Response {
                code: 0,
                message: "Success".to_string(),
                data: Some(VERSION.to_string()),
            };
            Ok(HttpResponse::builder()
                .status(StatusCode::OK)
                .json(&json_value)?
                .build())
        })
        .post(IpcCommand::StartClash.as_ref(), |payload| async move {
            match payload.json::<ClashConfig>() {
                Ok(start_clash) => {
                    match CORE_MANAGER.lock().await.start_core(start_clash).await {
                        Ok(_) => info!("Core started successfully"),
                        Err(e) => {
                            let json_value: Response<()> = Response {
                                code: 1,
                                message: format!("Failed to start core: {}", e),
                                data: None,
                            };
                            return Ok(HttpResponse::builder()
                                .status(StatusCode::SERVICE_UNAVAILABLE)
                                .json(&json_value)?
                                .build());
                        }
                    }
                    let json_value: Response<()> = Response {
                        code: 0,
                        message: "Core started successfully".to_string(),
                        data: None,
                    };
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
        .get(IpcCommand::GetClashLogs.as_ref(), |_| async move {
            let json_value = Response {
                code: 0,
                message: "Success".to_string(),
                data: Some(ClashLogger::global().get_logs().await.clone()),
            };
            Ok(HttpResponse::builder()
                .status(StatusCode::OK)
                .json(&json_value)?
                .build())
        })
        .delete(IpcCommand::StopClash.as_ref(), |_| async move {
            match CORE_MANAGER.lock().await.stop_core().await {
                Ok(_) => info!("Core stopped successfully"),
                Err(e) => {
                    let json_value: Response<()> = Response {
                        code: 1,
                        message: format!("Failed to stop core: {}", e),
                        data: None,
                    };
                    return Ok(HttpResponse::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .json(&json_value)?
                        .build());
                }
            }
            let json_value: Response<()> = Response {
                code: 0,
                message: "Core stopped successfully".to_string(),
                data: None,
            };
            Ok(HttpResponse::builder()
                .status(StatusCode::OK)
                .json(&json_value)?
                .build())
        });
    Ok(router)
}
