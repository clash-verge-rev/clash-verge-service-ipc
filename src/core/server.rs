use super::state::IpcState;
use crate::core::assets::stage_runtime;
use crate::core::auth::{
    AuthenticatedOwner, ServiceError, authenticate_owner, ipc_request_context_to_auth_context,
};
use crate::core::desired::{
    ActiveOwnerState, clear_active_owner, load_active_owner, persist_active_owner,
    persist_owner_core_started, persist_owner_core_stopped, persist_owner_core_stopped_by_key,
    persist_owner_writer_config,
};
use crate::core::legacy_cleanup::cleanup_legacy_owner_files;
use crate::core::logger::set_or_update_writer;
use crate::core::manager::{CORE_MANAGER, LOGGER_MANAGER};
use crate::core::paths::service_paths;
use crate::core::state::{set_core_lifecycle_state, set_service_lifecycle_state};
use crate::core::status::service_status_snapshot;
use crate::core::structure::{Response, ServiceLifecycleState};
use crate::{AuthenticatedRequest, IpcCommand, RuntimeBundle, VERSION, WriterConfig};
use anyhow::{Context as _, Result as AnyResult, anyhow};
use http::StatusCode;
use kode_bridge::{IpcHttpServer, Result, Router, ServerConfig, ipc_http_server::HttpResponse};
use once_cell::sync::Lazy;
use serde::Serialize;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, trace, warn};

const IPC_MAX_RESTARTS: u32 = 10;
const IPC_RESTART_WINDOW: Duration = Duration::from_secs(10);
const IPC_MAX_BACKOFF: Duration = Duration::from_millis(500);
const IPC_HANDLER_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(any(windows, test))]
const WINDOWS_CONTROL_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00000003;;;AU)";

async fn rollback_started_owner(owner: &AuthenticatedOwner) -> AnyResult<()> {
    if let Err(stop_error) = CORE_MANAGER.lock().await.stop_core().await {
        let recovery = persist_active_owner(owner).await;
        return match recovery {
            Ok(_) => Err(anyhow!(
                "failed to terminate owner core during rollback: {stop_error:#}; owner remains active"
            )),
            Err(active_error) => Err(anyhow!(
                "failed to terminate owner core during rollback: {stop_error:#}; failed to persist active owner: {active_error:#}"
            )),
        };
    }

    let desired_result = persist_owner_core_stopped(owner).await;
    let active_result = clear_active_owner().await;
    match (desired_result, active_result) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(desired_error), Ok(())) => Err(desired_error),
        (Ok(_), Err(active_error)) => Err(active_error),
        (Err(desired_error), Err(active_error)) => {
            set_core_lifecycle_state(ServiceLifecycleState::Fatal);
            Err(anyhow!(
                "failed to persist stopped owner state: {desired_error:#}; failed to clear active owner: {active_error:#}"
            ))
        }
    }
}

async fn commit_previous_owner_stopped(
    previous_owner: &ActiveOwnerState,
) -> std::result::Result<(), ServiceError> {
    if let Err(error) = clear_active_owner().await {
        if persist_owner_core_stopped_by_key(&previous_owner.owner_key)
            .await
            .is_err()
        {
            set_core_lifecycle_state(ServiceLifecycleState::Fatal);
        }
        return Err(ServiceError::owner_switch_failed(format!(
            "Previous owner core stopped but active owner could not be cleared: {error}"
        )));
    }
    if let Err(error) = persist_owner_core_stopped_by_key(&previous_owner.owner_key).await {
        let _ = clear_active_owner().await;
        return Err(ServiceError::owner_switch_failed(format!(
            "Failed to mark the previous owner stopped: {error}"
        )));
    }
    Ok(())
}

async fn commit_started_owner(
    owner: &AuthenticatedOwner,
    config: &crate::ClashConfig,
) -> AnyResult<()> {
    persist_owner_core_started(owner, config)
        .await
        .context("failed to persist owner desired state")?;
    persist_active_owner(owner)
        .await
        .context("failed to persist active owner")?;
    Ok(())
}

// 防止旧 listener 的清理删除 supervisor 刚创建的新 socket。
static IPC_LIFECYCLE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub async fn run_ipc_server() -> Result<JoinHandle<Result<()>>> {
    let _lifecycle_guard = IPC_LIFECYCLE_LOCK.lock().await;

    make_ipc_dir().await?;
    cleanup_stale_ipc_socket().await?;
    init_ipc_state().await?;

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<()>();

    IpcState::global().set_sender(shutdown_tx).await;
    IpcState::global().set_done(done_rx).await;

    if let Some(mut server) = IpcState::global().take_server().await {
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
                fs::set_permissions(paths.ipc_path(), Permissions::from_mode(0o666)).await?;
            } else {
                warn!(
                    "IPC socket {:?} did not appear before permission update timeout",
                    paths.ipc_path()
                );
            }
        }
        Ok(handle)
    } else {
        Err(kode_bridge::KodeBridgeError::configuration(
            "IPC server not initialized".to_string(),
        ))
    }
}

pub async fn stop_ipc_server() -> Result<()> {
    let _lifecycle_guard = IPC_LIFECYCLE_LOCK.lock().await;

    CORE_MANAGER
        .lock()
        .await
        .stop_core()
        .await
        .map_err(|error| kode_bridge::KodeBridgeError::custom(error.to_string()))?;

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

    stop_ipc_server().await?;
    server_handle.abort();
    Ok(())
}

fn ipc_backoff_delay(attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }

    Duration::from_millis(100u64 << (attempt - 1).min(3)).min(IPC_MAX_BACKOFF)
}

/// Creates the root-owned machine-wide control runtime directory.
async fn make_ipc_dir() -> Result<()> {
    #[cfg(unix)]
    {
        let paths = service_paths();
        let Some(dir_path) = paths.ipc_path().parent() else {
            return Ok(());
        };

        ensure_control_runtime_dir(dir_path)?;
    }
    #[cfg(windows)]
    {
        // No directory creation needed for Windows named pipes
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_control_runtime_dir(dir: &std::path::Path) -> std::io::Result<()> {
    crate::core::unix_security::ensure_service_directory(dir, 0o755)
        .map_err(|error| std::io::Error::other(error.to_string()))
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

async fn init_ipc_state() -> Result<()> {
    let server = create_ipc_server()?;
    let router = create_ipc_router()?;
    let server = server.router(router);
    IpcState::global().set_server(server).await;
    Ok(())
}

fn create_ipc_server() -> Result<IpcHttpServer> {
    let paths = service_paths();

    let server = IpcHttpServer::with_config(
        paths.ipc_path(),
        ServerConfig {
            write_timeout: IPC_HANDLER_TIMEOUT,
            ..ServerConfig::default()
        },
    )?;

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use platform_lib::{S_IRGRP, S_IROTH, S_IRUSR, S_IWGRP, S_IWOTH, S_IWUSR, mode_t};

        let mode: mode_t =
            platform_lib::mode_t::from(S_IRUSR | S_IWUSR | S_IRGRP | S_IWGRP | S_IROTH | S_IWOTH);
        let server = server.with_listener_mode(mode);
        Ok(server)
    }

    #[cfg(all(unix, target_os = "macos"))]
    {
        Ok(server)
    }

    #[cfg(windows)]
    {
        let server = server.with_listener_security_descriptor(WINDOWS_CONTROL_PIPE_SDDL);
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
            let request = match ctx.json::<AuthenticatedRequest<()>>() {
                Ok(request) => request,
                Err(error) => return bad_request(format!("Invalid JSON: {error}")),
            };
            let owner = match authenticate_owner(&ctx, &request.credentials) {
                Ok(owner) => owner,
                Err(error) => return service_error(error),
            };
            let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
            match service_status_snapshot(&owner).await {
                Ok(status) => ok_json(status),
                Err(error) => {
                    service_unavailable(format!("Failed to collect service status: {}", error))
                }
            }
        })
        .post(IpcCommand::StartClash.as_ref(), |ctx| async move {
            trace!("Received StartClash command");
            match ctx.json::<AuthenticatedRequest<RuntimeBundle>>() {
                Ok(request) => {
                    let owner = match authenticate_owner(&ctx, &request.credentials) {
                        Ok(owner) => owner,
                        Err(error) => return service_error(error),
                    };
                    let runtime_bundle = request.payload;
                    let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
                    let staged_runtime = match stage_runtime(&owner, &runtime_bundle).await {
                        Ok(staged) => staged,
                        Err(error) => return service_error(error),
                    };
                    let previous_owner = match load_active_owner().await {
                        Ok(owner) => owner,
                        Err(error) => {
                            return service_unavailable(format!(
                                "Failed to load active owner: {error}"
                            ));
                        }
                    };
                    if let Err(error) = CORE_MANAGER.lock().await.stop_core().await {
                        return service_error(ServiceError::owner_switch_failed(format!(
                            "Failed to stop the previous owner core: {error}"
                        )));
                    }
                    if let Some(previous_owner) = previous_owner.as_ref()
                        && let Err(error) = commit_previous_owner_stopped(previous_owner).await
                    {
                        return service_error(error);
                    }
                    let start_clash = match staged_runtime.activate().await {
                        Ok(prepared) => prepared.clash_config,
                        Err(error) => return service_error(error),
                    };
                    let start_result = {
                        let manager = CORE_MANAGER.lock().await;
                        manager
                            .start_core(start_clash.clone(), owner.identity.clone())
                            .await
                    };
                    match start_result {
                        Ok(_) => info!("Core started successfully"),
                        Err(error) => {
                            if let Err(rollback_error) = rollback_started_owner(&owner).await {
                                return service_error(ServiceError::owner_switch_failed(format!(
                                    "Failed to start owner core: {error}; rollback failed: {rollback_error:#}"
                                )));
                            }
                            return service_error(ServiceError::owner_switch_failed(format!(
                                "Failed to start owner core: {error}"
                            )));
                        }
                    }
                    if let Err(error) = commit_started_owner(&owner, &start_clash).await {
                        if let Err(rollback_error) = rollback_started_owner(&owner).await {
                            return service_error(ServiceError::owner_switch_failed(format!(
                                "Failed to commit owner state: {error:#}; rollback failed: {rollback_error:#}"
                            )));
                        }
                        return service_error(ServiceError::owner_switch_failed(format!(
                            "Failed to commit owner state: {error:#}"
                        )));
                    }
                    if let Err(error) = cleanup_legacy_owner_files(&owner).await {
                        warn!(
                            "Core start committed, but legacy owner cleanup will be retried later: {error}"
                        );
                    }
                    ok_empty("Core started successfully")
                }
                Err(error) => bad_request(format!("Invalid JSON: {error}")),
            }
        })
        .get(IpcCommand::GetClashLogs.as_ref(), |ctx| async move {
            trace!("Received GetClashLogs command");
            let request = match ctx.json::<AuthenticatedRequest<()>>() {
                Ok(request) => request,
                Err(error) => return bad_request(format!("Invalid JSON: {error}")),
            };
            let owner = match authenticate_owner(&ctx, &request.credentials) {
                Ok(owner) => owner,
                Err(error) => return service_error(error),
            };
            let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
            if let Err(error) = require_active_owner(&owner).await {
                return service_error(error);
            }
            ok_json(LOGGER_MANAGER.get_logs().await)
        })
        .get(IpcCommand::GetClashLogSnapshot.as_ref(), |ctx| async move {
            trace!("Received GetClashLogSnapshot command");
            let request = match ctx.json::<AuthenticatedRequest<()>>() {
                Ok(request) => request,
                Err(error) => return bad_request(format!("Invalid JSON: {error}")),
            };
            let owner = match authenticate_owner(&ctx, &request.credentials) {
                Ok(owner) => owner,
                Err(error) => return service_error(error),
            };
            let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
            if let Err(error) = require_active_owner(&owner).await {
                return service_error(error);
            }
            let path = service_paths()
                .for_owner(&owner.identity)
                .logs_dir()
                .join("service_latest.log");
            match read_log_snapshot(&path).await {
                Ok(snapshot) => ok_json(snapshot),
                Err(error) => service_unavailable(format!("Failed to read core log snapshot: {error}")),
            }
        })
        .delete(IpcCommand::StopClash.as_ref(), |ctx| async move {
            trace!("Received StopClash command");
            let request = match ctx.json::<AuthenticatedRequest<()>>() {
                Ok(request) => request,
                Err(error) => return bad_request(format!("Invalid JSON: {error}")),
            };
            let owner = match authenticate_owner(&ctx, &request.credentials) {
                Ok(owner) => owner,
                Err(error) => return service_error(error),
            };
            let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
            if let Err(error) = require_active_owner(&owner).await {
                return service_error(error);
            }
            match CORE_MANAGER.lock().await.stop_core().await {
                Ok(_) => info!("Core stopped successfully"),
                Err(e) => {
                    return service_unavailable(format!("Failed to stop core: {}", e));
                }
            }
            if let Err(e) = persist_owner_core_stopped(&owner).await {
                set_core_lifecycle_state(ServiceLifecycleState::Fatal);
                return service_unavailable(format!("Failed to persist desired state: {}", e));
            }
            ok_empty("Core stopped successfully")
        })
        .put(IpcCommand::UpdateWriter.as_ref(), |ctx| async move {
            trace!("Received UpdateWriter command");
            match ctx.json::<AuthenticatedRequest<WriterConfig>>() {
                Ok(request) => {
                    let owner = match authenticate_owner(&ctx, &request.credentials) {
                        Ok(owner) => owner,
                        Err(error) => return service_error(error),
                    };
                    let mut writer_config = request.payload;
                    let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
                    if let Err(error) = require_active_owner(&owner).await {
                        return service_error(error);
                    }
                    writer_config.directory = service_paths()
                        .for_owner(&owner.identity)
                        .logs_dir()
                        .to_string_lossy()
                        .into_owned();
                    match set_or_update_writer(&writer_config).await {
                        Ok(_) => info!("Update writer successfully"),
                        Err(e) => {
                            return service_unavailable(format!("Failed to update writer: {}", e));
                        }
                    };
                    if let Err(e) = persist_owner_writer_config(&owner, &writer_config).await {
                        return service_unavailable(format!(
                            "Failed to persist writer config: {}",
                            e
                        ));
                    }
                    ok_empty("Update Writer successfully")
                }
                Err(error) => bad_request(format!("Invalid JSON: {error}")),
            }
        });
    Ok(router)
}

async fn read_log_snapshot(path: &std::path::Path) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    // The kode-bridge in-memory response limit is 10 MiB. Hex keeps the JSON payload
    // bounded and avoids content-dependent escaping expansion.
    const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    if length > MAX_SNAPSHOT_BYTES {
        file.seek(std::io::SeekFrom::Start(length - MAX_SNAPSHOT_BYTES))
            .await?;
    }
    let mut content = Vec::with_capacity(length.min(MAX_SNAPSHOT_BYTES) as usize);
    file.read_to_end(&mut content).await?;
    if length > MAX_SNAPSHOT_BYTES
        && let Some(first_newline) = content.iter().position(|byte| *byte == b'\n')
    {
        content.drain(..=first_newline);
    }
    let mut encoded = String::with_capacity(content.len() * 2);
    for byte in content {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
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

fn bad_request(message: impl Into<String>) -> Result<HttpResponse> {
    json_response::<()>(
        StatusCode::BAD_REQUEST,
        StatusCode::BAD_REQUEST.as_u16(),
        message,
        None,
    )
}

fn service_error(error: ServiceError) -> Result<HttpResponse> {
    let status = match error.code {
        crate::ServiceErrorCode::UnauthorizedOwner => StatusCode::UNAUTHORIZED,
        crate::ServiceErrorCode::NotActive => StatusCode::CONFLICT,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    json_response::<()>(status, error.code as u16, error.message, None)
}

async fn require_active_owner(
    owner: &crate::core::auth::AuthenticatedOwner,
) -> std::result::Result<(), ServiceError> {
    if load_active_owner()
        .await
        .map_err(|_| ServiceError::not_active())?
        .is_some_and(|active| active.owner_key == owner.key)
    {
        Ok(())
    } else {
        Err(ServiceError::not_active())
    }
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

static OWNER_LIFECYCLE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[cfg(test)]
mod owner_lifecycle_tests {
    use super::{
        OWNER_LIFECYCLE_LOCK, WINDOWS_CONTROL_PIPE_SDDL, commit_previous_owner_stopped,
        commit_started_owner, require_active_owner,
    };
    use crate::ServiceErrorCode;
    use crate::core::auth::AuthenticatedOwner;
    use crate::core::desired::{
        clear_active_owner, load_active_owner, load_owner_desired_state, persist_active_owner,
        persist_owner_core_started,
    };
    use crate::{ClashConfig, OwnerIdentity};
    use serial_test::serial;

    fn owner(uid: u32) -> AuthenticatedOwner {
        AuthenticatedOwner {
            key: uid.to_string(),
            identity: OwnerIdentity::Unix { uid, gid: 20 },
            app_data_root: std::env::temp_dir(),
        }
    }

    #[test]
    fn windows_control_pipe_allows_authenticated_users_not_everyone() {
        assert!(WINDOWS_CONTROL_PIPE_SDDL.contains(";;;AU)"));
        assert!(!WINDOWS_CONTROL_PIPE_SDDL.contains(";;;WD)"));
        assert!(!WINDOWS_CONTROL_PIPE_SDDL.contains("GRGW;;;AU"));
        assert!(WINDOWS_CONTROL_PIPE_SDDL.contains("0x00000003;;;AU"));
    }

    #[tokio::test]
    #[serial]
    async fn non_active_owner_receives_stable_error() -> anyhow::Result<()> {
        let active = owner(92_001);
        let inactive = owner(92_002);
        persist_active_owner(&active).await?;

        let error = require_active_owner(&inactive)
            .await
            .expect_err("non-active owner must be rejected");

        assert_eq!(error.code, ServiceErrorCode::NotActive);
        clear_active_owner().await?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn owner_takeover_marks_previous_stopped_before_committing_new_owner()
    -> anyhow::Result<()> {
        let owner_a = owner(93_001);
        let owner_b = owner(93_002);
        let config = ClashConfig::default();
        persist_owner_core_started(&owner_a, &config).await?;
        persist_active_owner(&owner_a).await?;

        let previous = load_active_owner()
            .await?
            .expect("owner A should be active");
        commit_previous_owner_stopped(&previous).await?;

        assert!(load_active_owner().await?.is_none());
        assert!(
            !load_owner_desired_state(&owner_a.key)
                .await?
                .core_should_be_running
        );

        persist_owner_core_started(&owner_b, &config).await?;
        persist_active_owner(&owner_b).await?;
        assert_eq!(
            load_active_owner().await?.map(|owner| owner.owner_key),
            Some(owner_b.key.clone())
        );
        assert_eq!(
            require_active_owner(&owner_a)
                .await
                .expect_err("owner A must lose control after owner B commits")
                .code,
            ServiceErrorCode::NotActive
        );

        clear_active_owner().await?;
        for key in [&owner_a.key, &owner_b.key] {
            let _ = std::fs::remove_dir_all(crate::service_paths().for_owner_key(key).root());
        }
        Ok(())
    }

    async fn commit_test_transition(
        owner: &AuthenticatedOwner,
        config: &ClashConfig,
    ) -> anyhow::Result<()> {
        let _guard = OWNER_LIFECYCLE_LOCK.lock().await;
        if let Some(previous) = load_active_owner().await? {
            commit_previous_owner_stopped(&previous).await?;
        }
        commit_started_owner(owner, config).await
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_owner_state_commits_leave_exactly_one_active_owner() -> anyhow::Result<()> {
        clear_active_owner().await?;
        let owner_a = owner(94_001);
        let owner_b = owner(94_002);
        let config = ClashConfig::default();

        let (left, right) = tokio::join!(
            commit_test_transition(&owner_a, &config),
            commit_test_transition(&owner_b, &config)
        );
        left?;
        right?;

        let active = load_active_owner()
            .await?
            .expect("one owner must be active");
        let desired_a = load_owner_desired_state(&owner_a.key)
            .await?
            .core_should_be_running;
        let desired_b = load_owner_desired_state(&owner_b.key)
            .await?
            .core_should_be_running;
        assert_ne!(desired_a, desired_b);
        assert_eq!(active.owner_key == owner_a.key, desired_a);
        assert_eq!(active.owner_key == owner_b.key, desired_b);

        clear_active_owner().await?;
        for key in [&owner_a.key, &owner_b.key] {
            let _ = std::fs::remove_dir_all(crate::service_paths().for_owner_key(key).root());
        }
        Ok(())
    }
}
