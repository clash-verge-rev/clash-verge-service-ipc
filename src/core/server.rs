use super::state::IpcState;
use crate::core::auth::ipc_request_context_to_auth_context;
use crate::core::desired::{persist_core_started, persist_core_stopped, persist_writer_config};
use crate::core::logger::set_or_update_writer;
use crate::core::manager::{CORE_MANAGER, LOGGER_MANAGER};
use crate::core::paths::service_paths;
use crate::core::state::set_service_lifecycle_state;
use crate::core::status::service_status_snapshot;
use crate::core::structure::{Response, ServiceLifecycleState};
use crate::{ClashConfig, IpcCommand, VERSION, WriterConfig};
use anyhow::{Result as AnyResult, anyhow};
use http::StatusCode;
use kode_bridge::{IpcHttpServer, Result, Router, ipc_http_server::HttpResponse};
use serde::Serialize;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, trace, warn};

const IPC_MAX_RESTARTS: u32 = 5;
const IPC_RESTART_WINDOW: Duration = Duration::from_secs(60);
const IPC_MAX_BACKOFF: Duration = Duration::from_secs(5);

pub async fn run_ipc_server() -> Result<JoinHandle<Result<()>>> {
    make_ipc_dir().await?;
    cleanup_stale_ipc_socket().await?;
    init_ipc_state().await?;

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<()>();

    IpcState::global().set_sender(shutdown_tx).await;
    IpcState::global().set_done(done_rx).await;

    if let Some(mut server) = IpcState::global().take_server().await {
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
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            use tokio::fs;

            let paths = service_paths();
            let mut socket_ready = false;
            for _ in 0..20 {
                if paths.ipc_path().exists() {
                    socket_ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            if socket_ready {
                fs::set_permissions(paths.ipc_path(), Permissions::from_mode(0o777)).await?;
            } else {
                warn!(
                    "IPC socket {:?} did not appear before permission update timeout",
                    paths.ipc_path()
                );
            }

            spawn_socket_dir_watchdog();
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

    if let Some(sender) = IpcState::global().take_sender().await {
        let _ = sender.send(());
    }

    if let Some(done) = IpcState::global().take_done().await {
        let _ = done.await;
    }

    IpcState::global().shutdown_server().await;

    cleanup_ipc_path().await?;
    #[cfg(windows)]
    tokio::time::sleep(std::time::Duration::from_millis(70)).await;

    Ok(())
}

pub async fn run_ipc_supervisor_until_shutdown(
    shutdown: impl Future<Output = ()>,
) -> AnyResult<()> {
    set_service_lifecycle_state(ServiceLifecycleState::Starting);
    info!("Starting IPC server...");

    let mut server_handle = match run_ipc_server().await {
        Ok(handle) => handle,
        Err(error) => {
            set_service_lifecycle_state(ServiceLifecycleState::Fatal);
            return Err(anyhow!("failed to start IPC server: {}", error));
        }
    };
    set_service_lifecycle_state(ServiceLifecycleState::Running);
    info!("IPC server started successfully. Waiting for shutdown signal...");

    let mut restart_timestamps: Vec<Instant> = Vec::new();
    let mut consecutive_attempt = 0u32;
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("Shutdown signal received. Stopping IPC server...");
                break;
            }
            join_result = &mut server_handle => {
                let reason = match join_result {
                    Ok(Ok(())) => "IPC server exited cleanly".to_string(),
                    Ok(Err(error)) => format!("IPC server returned error: {error}"),
                    Err(error) => format!("IPC server task failed: {error}"),
                };
                warn!("{reason}; rebuilding IPC listener in-process");
                set_service_lifecycle_state(ServiceLifecycleState::RecoveringIpc);

                let now = Instant::now();
                restart_timestamps.retain(|t| now.duration_since(*t) < IPC_RESTART_WINDOW);
                if restart_timestamps.is_empty() {
                    consecutive_attempt = 0;
                }
                restart_timestamps.push(now);

                if restart_timestamps.len() as u32 > IPC_MAX_RESTARTS {
                    set_service_lifecycle_state(ServiceLifecycleState::Fatal);
                    return Err(anyhow!(
                        "IPC server restarted {} times in {}s",
                        restart_timestamps.len(),
                        IPC_RESTART_WINDOW.as_secs()
                    ));
                }

                let delay = ipc_backoff_delay(consecutive_attempt);
                consecutive_attempt += 1;
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }

                server_handle = match run_ipc_server().await {
                    Ok(handle) => handle,
                    Err(error) => {
                        set_service_lifecycle_state(ServiceLifecycleState::Fatal);
                        return Err(anyhow!("failed to rebuild IPC server: {}", error));
                    }
                };
                set_service_lifecycle_state(ServiceLifecycleState::Running);
                info!("IPC listener rebuilt successfully");
            }
        }
    }

    let _ = stop_ipc_server().await;
    server_handle.abort();
    Ok(())
}

fn ipc_backoff_delay(attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }

    Duration::from_secs(1u64 << (attempt - 1).min(3)).min(IPC_MAX_BACKOFF)
}

async fn make_ipc_dir() -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        use tokio::fs;

        let paths = service_paths();
        let Some(dir_path) = paths.ipc_path().parent() else {
            return Ok(());
        };

        if !dir_path.exists() {
            fs::create_dir_all(dir_path).await?;
        }

        // We need to ensure compatibility with sudo GID through the terminal
        let gid = std::env::var("SUDO_GID")
            .ok()
            .and_then(|s| s.parse::<platform_lib::gid_t>().ok())
            .unwrap_or_else(|| unsafe { platform_lib::getgid() });

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

        // See issues in https://github.com/clash-verge-rev/clash-verge-rev/issues/6149
        // Apply the SetGID bit (0o2000) to ensure the IPC socket inherits the directory's group ID.
        // The mode is set to 0o2770 (rwxrws---) to permit the designated user group (e.g., staff
        // on macOS or the primary group on Linux) to manage the socket's lifecycle. This prevents
        // permission denied errors when the GUI process, running with non-root privileges,
        // attempts to recreate the socket during service initialization or sidecar fallbacks.
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
        use tokio::fs;

        let paths = service_paths();
        if paths.ipc_path().exists() {
            fs::remove_file(paths.ipc_path()).await?;
        }
    }
    #[cfg(windows)]
    {
        // Named pipes on Windows are automatically cleaned up when the last handle is closed
        // No manual cleanup needed
    }
    Ok(())
}

async fn cleanup_stale_ipc_socket() -> Result<()> {
    #[cfg(unix)]
    {
        let paths = service_paths();
        let socket_path = paths.ipc_path();
        if !socket_path.exists() {
            return Ok(());
        }

        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::net::UnixStream::connect(socket_path),
        )
        .await
        {
            Ok(Ok(_stream)) => {
                warn!(
                    "IPC socket {:?} is reachable; leaving it in place",
                    socket_path
                );
            }
            _ => {
                info!("Cleaning up stale IPC socket: {:?}", socket_path);
                tokio::fs::remove_file(socket_path).await?;
            }
        }
    }
    #[cfg(windows)]
    {}
    Ok(())
}

#[cfg(unix)]
pub fn spawn_socket_dir_watchdog() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);
    if WATCHDOG_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }

    tokio::spawn(async move {
        let paths = service_paths();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;

            let socket_path = paths.ipc_path();
            if let Some(dir) = socket_path.parent()
                && !dir.exists()
            {
                warn!("IPC socket directory {:?} was deleted, recreating", dir);
                if let Err(e) = tokio::fs::create_dir_all(dir).await {
                    warn!("Failed to recreate IPC socket directory: {}", e);
                    continue;
                }
                use std::fs::Permissions;
                use std::os::unix::fs::PermissionsExt;
                let _ = tokio::fs::set_permissions(dir, Permissions::from_mode(0o777)).await;
                info!("IPC socket directory {:?} recreated", dir);
            }
        }
    });
}

async fn init_ipc_state() -> Result<()> {
    let server = create_ipc_server()?;
    let router = create_ipc_router()?;
    let server = server.router(router);
    IpcState::global().set_server(server).await;
    Ok(())
}

fn create_ipc_server() -> Result<IpcHttpServer> {
    let paths = service_paths();

    let server = IpcHttpServer::new(paths.ipc_path())?;

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use platform_lib::{S_IRGRP, S_IRUSR, S_IWGRP, S_IWUSR, mode_t};

        let mode: mode_t = platform_lib::mode_t::from(S_IRUSR | S_IWUSR | S_IRGRP | S_IWGRP);
        let server = server.with_listener_mode(mode);
        Ok(server)
    }

    #[cfg(all(unix, target_os = "macos"))]
    {
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
            ok_json(VERSION.to_string())
        })
        .get(IpcCommand::Status.as_ref(), |ctx| async move {
            trace!("Received Status command");
            ipc_request_context_to_auth_context(&ctx)?;
            match service_status_snapshot().await {
                Ok(status) => ok_json(status),
                Err(error) => {
                    service_unavailable(format!("Failed to collect service status: {}", error))
                }
            }
        })
        .post(IpcCommand::StartClash.as_ref(), |ctx| async move {
            trace!("Received StartClash command");
            ipc_request_context_to_auth_context(&ctx)?;
            match ctx.json::<ClashConfig>() {
                Ok(start_clash) => {
                    match CORE_MANAGER
                        .lock()
                        .await
                        .start_core(start_clash.clone())
                        .await
                    {
                        Ok(_) => info!("Core started successfully"),
                        Err(e) => {
                            return service_unavailable(format!("Failed to start core: {}", e));
                        }
                    }
                    if let Err(e) = persist_core_started(&start_clash).await {
                        return service_unavailable(format!(
                            "Failed to persist desired state: {}",
                            e
                        ));
                    }
                    ok_empty("Core started successfully")
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
            ok_json(LOGGER_MANAGER.get_logs().await)
        })
        .delete(IpcCommand::StopClash.as_ref(), |ctx| async move {
            trace!("Received StopClash command");
            ipc_request_context_to_auth_context(&ctx)?;
            match CORE_MANAGER.lock().await.stop_core().await {
                Ok(_) => info!("Core stopped successfully"),
                Err(e) => {
                    return service_unavailable(format!("Failed to stop core: {}", e));
                }
            }
            if let Err(e) = persist_core_stopped().await {
                return service_unavailable(format!("Failed to persist desired state: {}", e));
            }
            ok_empty("Core stopped successfully")
        })
        .put(IpcCommand::UpdateWriter.as_ref(), |ctx| async move {
            trace!("Received UpdateWriter command");
            ipc_request_context_to_auth_context(&ctx)?;
            match ctx.json::<WriterConfig>() {
                Ok(writer_config) => {
                    match set_or_update_writer(&writer_config).await {
                        Ok(_) => info!("Update writer successfully"),
                        Err(e) => {
                            return service_unavailable(format!("Failed to update writer: {}", e));
                        }
                    };
                    if let Err(e) = persist_writer_config(&writer_config).await {
                        return service_unavailable(format!(
                            "Failed to persist writer config: {}",
                            e
                        ));
                    }
                    ok_empty("Update Writer successfully")
                }
                Err(e) => Ok(HttpResponse::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .text(format!("Invalid JSON: {}", e))
                    .build()),
            }
        });
    Ok(router)
}

fn ok_json<T: Serialize>(data: T) -> Result<HttpResponse> {
    json_response(StatusCode::OK, 0, "Success", Some(data))
}

fn ok_empty(message: impl Into<String>) -> Result<HttpResponse> {
    json_response::<()>(StatusCode::OK, 0, message, None)
}

fn service_unavailable(message: impl Into<String>) -> Result<HttpResponse> {
    json_response::<()>(StatusCode::SERVICE_UNAVAILABLE, 1, message, None)
}

fn json_response<T: Serialize>(
    status: StatusCode,
    code: u16,
    message: impl Into<String>,
    data: Option<T>,
) -> Result<HttpResponse> {
    let json_value = Response {
        code,
        message: message.into(),
        data,
    };
    Ok(HttpResponse::builder()
        .status(status)
        .json(&json_value)?
        .build())
}
