use super::state::IpcState;
use crate::core::auth::ipc_request_context_to_auth_context;
use crate::core::logger::set_or_update_writer;
use crate::core::manager::{CORE_MANAGER, LOGGER_MANAGER};
use crate::core::structure::Response;
use crate::{ClashConfig, IpcCommand, VERSION, WriterConfig};
use http::StatusCode;
use kode_bridge::{IpcHttpServer, Result, Router, ipc_http_server::HttpResponse};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, trace};

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
        drop(guard);
    }

    let server_arc = IpcState::global().lock().await.get_server();
    let mut guard = server_arc.lock().await;

    if let Some(mut server) = guard.take() {
        let handle = tokio::spawn(async move {
            #[cfg(unix)]
            let res = tokio::select! {
                res = unsafe{ platform_lib::umask(0o007); server.serve() } => res,
                _ = &mut shutdown_rx => Ok(()),
            };
            #[cfg(not(unix))]
            let res = tokio::select! {
                res = server.serve() => res,
                _ = &mut shutdown_rx => Ok(()),
            };

            let _ = done_tx.send(());
            res
        });
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
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;

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

        let Some(dir_path) = Path::new(IPC_PATH).parent() else {
            return Ok(());
        };

        if !dir_path.exists() {
            fs::create_dir_all(dir_path).await?;
        }

        let gid = std::env::var("SUDO_GID")
            .ok()
            .and_then(|v| v.parse::<platform_lib::gid_t>().ok())
            .unwrap_or_else(|| {
                #[cfg(target_os = "macos")]
                {
                    use std::os::unix::fs::MetadataExt;
                    std::fs::metadata("/dev/console")
                        .map(|m| m.gid())
                        .unwrap_or_else(|_| unsafe { platform_lib::getgid() })
                }
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::fs::MetadataExt;
                    std::fs::read_dir("/run/user")
                        .ok()
                        .and_then(|mut entries| {
                            entries.find_map(|entry| {
                                let entry = entry.ok()?;
                                let name = entry.file_name().into_string().ok()?;
                                let uid = name.parse::<u32>().ok()?;
                                if uid >= 1000 && uid < 65534 {
                                    entry.metadata().ok().map(|m| m.gid())
                                } else {
                                    None
                                }
                            })
                        })
                        .unwrap_or_else(|_| unsafe { platform_lib::getgid() })
                }
            });

        if let Ok(c_path) = std::ffi::CString::new(dir_path.to_string_lossy().as_bytes()) {
            unsafe {
                if platform_lib::chown(c_path.as_ptr(), platform_lib::uid_t::MAX, gid) != 0 {
                    let err = std::io::Error::last_os_error();
                    log::warn!(
                        "Failed to chown directory {:?} to gid {}: {}",
                        dir_path,
                        gid,
                        err
                    );
                }
            }
        }

        // Set the setgid bit so that created sockets inherit the group and remain accessible to admins.
        fs::set_permissions(dir_path, Permissions::from_mode(0o2770)).await?;
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
        use platform_lib::{S_IRGRP, S_IRUSR, S_IWGRP, S_IWUSR, mode_t};

        let mode: mode_t = platform_lib::mode_t::from(S_IRUSR | S_IWUSR | S_IRGRP | S_IWGRP);
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
        .get(IpcCommand::Magic.as_ref(), |ctx| async move {
            trace!("Received Magic command");
            ipc_request_context_to_auth_context(&ctx)?;
            Ok(HttpResponse::builder().text("Tunglies!").build())
        })
        .get(IpcCommand::GetVersion.as_ref(), |ctx| async move {
            ipc_request_context_to_auth_context(&ctx)?;
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
        .post(IpcCommand::StartClash.as_ref(), |ctx| async move {
            trace!("Received StartClash command");
            ipc_request_context_to_auth_context(&ctx)?;
            match ctx.json::<ClashConfig>() {
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
        .get(IpcCommand::GetClashLogs.as_ref(), |ctx| async move {
            trace!("Received GetClashLogs command");
            ipc_request_context_to_auth_context(&ctx)?;
            let json_value = Response {
                code: 0,
                message: "Success".to_string(),
                data: Some(LOGGER_MANAGER.get_logs().await),
            };
            Ok(HttpResponse::builder()
                .status(StatusCode::OK)
                .json(&json_value)?
                .build())
        })
        .delete(IpcCommand::StopClash.as_ref(), |ctx| async move {
            trace!("Received StopClash command");
            ipc_request_context_to_auth_context(&ctx)?;
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
        })
        .put(IpcCommand::UpdateWriter.as_ref(), |ctx| async move {
            trace!("Received UpdateWriter command");
            ipc_request_context_to_auth_context(&ctx)?;
            match ctx.json::<WriterConfig>() {
                Ok(writer_config) => {
                    match set_or_update_writer(&writer_config).await {
                        Ok(_) => info!("Update writer successfully"),
                        Err(e) => {
                            let json_value: Response<()> = Response {
                                code: 1,
                                message: format!("Failed to update writer: {}", e),
                                data: None,
                            };
                            return Ok(HttpResponse::builder()
                                .status(StatusCode::SERVICE_UNAVAILABLE)
                                .json(&json_value)?
                                .build());
                        }
                    };
                    let json_value: Response<()> = Response {
                        code: 0,
                        message: "Update Writer successfully".to_string(),
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
        });
    Ok(router)
}
