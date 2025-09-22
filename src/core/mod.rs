pub mod command;
mod manager;
mod state;

pub use command::IpcCommand;
use http::StatusCode;
use state::IpcState;

use kode_bridge::{IpcHttpServer, Result, Router, ipc_http_server::HttpResponse};
use tracing::info;

use crate::core::manager::{CORE_MANAGER, CoreConfig};

pub async fn run_ipc_server() -> Result<()> {
    cleanup_ipc_path()?;
    init_ipc_state().await?;
    {
        let server_arc = IpcState::get_server().await;
        let mut guard = server_arc.write().await;
        if let Some(server) = guard.as_mut() {
            server.serve().await
        } else {
            Err(kode_bridge::KodeBridgeError::configuration(
                "IPC server not initialized".to_string(),
            ))
        }
    }
}

pub async fn stop_ipc_server() -> Result<()> {
    {
        let server_arc = IpcState::get_server().await;
        let mut guard = server_arc.write().await;
        if let Some(server) = guard.as_mut() {
            server.shutdown();
        }
        *guard = None;
    }
    cleanup_ipc_path()?;
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

pub async fn init_ipc_state() -> Result<()> {
    let server = create_ipc_server()?;
    let router = create_ipc_router()?;
    let server = server.router(router);
    IpcState::set_server(server).await;
    Ok(())
}

fn create_ipc_server() -> Result<IpcHttpServer> {
    use crate::IPC_PATH;
    IpcHttpServer::new(IPC_PATH)
}

fn create_ipc_router() -> Result<Router> {
    let router = Router::new()
        .get(IpcCommand::Magic.as_ref(), |_| async move {
            Ok(HttpResponse::builder().text("Tunglies!").build())
        })
        .get(IpcCommand::GetVersion.as_ref(), |_| async move {
            Ok(HttpResponse::builder()
                .text(env!("CARGO_PKG_VERSION"))
                .build())
        })
        .post(IpcCommand::StartClash.as_ref(), |payload| async move {
            match payload.json::<CoreConfig>() {
                Ok(config) => {
                    match CORE_MANAGER.lock().unwrap().start_core(config) {
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
                    return Ok(HttpResponse::builder()
                        .status(StatusCode::OK)
                        .json(&json_value)?
                        .build());
                }
                Err(e) => {
                    return Ok(HttpResponse::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .text(format!("Invalid JSON: {}", e))
                        .build());
                }
            }
        })
        .delete(IpcCommand::StopClash.as_ref(), |_| async move {
            match CORE_MANAGER.lock().unwrap().stop_core() {
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
