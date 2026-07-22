use super::state::IpcState;
use crate::core::assets::{StagedRuntime, stage_runtime};
use crate::core::auth::{
    AuthenticatedOwner, ServiceError, authenticate_owner, hash_session_token,
    ipc_request_context_to_auth_context,
};
use crate::core::desired::{
    ActiveOwnerState, clear_active_owner, commit_active_owner_session, load_active_owner,
    persist_owner_core_started, persist_owner_core_stopped, persist_owner_core_stopped_by_key,
    persist_owner_writer_config,
};
use crate::core::legacy_cleanup::cleanup_legacy_owner_files;
use crate::core::logger::set_or_update_writer;
use crate::core::manager::{CORE_MANAGER, LOGGER_MANAGER};
use crate::core::paths::service_paths;
use crate::core::state::{set_core_lifecycle_state, set_service_lifecycle_state};
use crate::core::status::service_status_snapshot;
use crate::core::structure::{OwnerSessionProof, Response, ServiceLifecycleState};
use crate::core::{apply_proxy, apply_proxy_or_direct, clear_proxy, validate_proxy_config};
use crate::{
    AuthenticatedRequest, AuthenticatedSessionRequest, ClashConfig, IpcCommand, MacosProxyConfig,
    OwnerSessionHandle, ProxyApplyOutcome, SERVICE_PROTOCOL_HEADER, StartClashRequest,
    StartClashResult, VERSION, WriterConfig,
};
use anyhow::{Context as _, Result as AnyResult, anyhow};
use http::StatusCode;
use kode_bridge::{IpcHttpServer, Result, Router, ServerConfig, ipc_http_server::HttpResponse};
use once_cell::sync::Lazy;
use serde::Serialize;
#[cfg(feature = "test")]
use std::sync::atomic::{AtomicU8, Ordering};
use std::{
    future::Future,
    time::{Duration, Instant},
};
#[cfg(feature = "test")]
use tokio::sync::Notify;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, trace, warn};

const IPC_MAX_RESTARTS: u32 = 10;
const IPC_RESTART_WINDOW: Duration = Duration::from_secs(10);
const IPC_MAX_BACKOFF: Duration = Duration::from_millis(500);
const IPC_HANDLER_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(any(windows, test))]
const WINDOWS_CONTROL_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00000003;;;AU)";

trait OwnerProxyTransition {
    async fn clear_previous_proxy(&mut self) -> AnyResult<()>;
    async fn compensate_direct(&mut self) -> AnyResult<()>;
    async fn stop_previous_core(&mut self) -> AnyResult<()>;
    async fn start_new_core(&mut self) -> AnyResult<()>;
    async fn commit_new_owner(&mut self) -> AnyResult<ActiveOwnerState>;
    async fn apply_new_proxy(&mut self) -> AnyResult<crate::ProxyApplyOutcome>;
}

async fn owner_proxy_transition(
    transition: &mut impl OwnerProxyTransition,
) -> std::result::Result<(ActiveOwnerState, crate::ProxyApplyOutcome), ServiceError> {
    if let Err(clear_error) = transition.clear_previous_proxy().await {
        let compensation = transition.compensate_direct().await;
        let message = match compensation {
            Ok(()) => format!("Failed to clear the previous owner's proxy: {clear_error:#}"),
            Err(compensation_error) => format!(
                "Failed to clear the previous owner's proxy: {clear_error:#}; direct compensation failed: {compensation_error:#}"
            ),
        };
        return Err(ServiceError::proxy_clear_failed(message));
    }

    transition.stop_previous_core().await.map_err(|error| {
        ServiceError::owner_switch_failed(format!(
            "Failed to stop the previous owner core: {error:#}"
        ))
    })?;
    transition.start_new_core().await.map_err(|error| {
        ServiceError::owner_switch_failed(format!("Failed to start owner core: {error:#}"))
    })?;
    let active = transition.commit_new_owner().await.map_err(|error| {
        ServiceError::owner_switch_failed(format!("Failed to commit owner state: {error:#}"))
    })?;
    let proxy_outcome = transition.apply_new_proxy().await.map_err(|error| {
        ServiceError::proxy_apply_failed(format!("Failed to apply owner proxy: {error:#}"))
    })?;
    Ok((active, proxy_outcome))
}

struct StartOwnerTransition<'a> {
    previous_owner: Option<ActiveOwnerState>,
    owner: &'a AuthenticatedOwner,
    staged_runtime: Option<StagedRuntime>,
    clash_config: Option<ClashConfig>,
    proposed_session_token: &'a str,
    macos_proxy: Option<&'a MacosProxyConfig>,
}

impl OwnerProxyTransition for StartOwnerTransition<'_> {
    async fn clear_previous_proxy(&mut self) -> AnyResult<()> {
        clear_service_proxy().await
    }

    async fn compensate_direct(&mut self) -> AnyResult<()> {
        compensate_service_proxy().await
    }

    async fn stop_previous_core(&mut self) -> AnyResult<()> {
        CORE_MANAGER.lock().await.stop_core().await?;
        if let Some(previous_owner) = self.previous_owner.as_ref() {
            persist_owner_core_stopped_by_key(&previous_owner.owner_key)
                .await
                .context("failed to persist the previous owner stopped state")?;
        }
        clear_active_owner()
            .await
            .context("failed to clear the previous active owner")?;
        Ok(())
    }

    async fn start_new_core(&mut self) -> AnyResult<()> {
        let staged_runtime = self
            .staged_runtime
            .take()
            .context("staged runtime was already consumed")?;
        let clash_config = staged_runtime
            .activate()
            .await
            .map_err(anyhow::Error::new)?
            .clash_config;
        if let Err(error) = CORE_MANAGER
            .lock()
            .await
            .start_core(clash_config.clone(), self.owner.identity.clone())
            .await
        {
            let _ = persist_owner_core_stopped(self.owner).await;
            return Err(error);
        }
        self.clash_config = Some(clash_config);
        Ok(())
    }

    async fn commit_new_owner(&mut self) -> AnyResult<ActiveOwnerState> {
        let clash_config = self
            .clash_config
            .as_ref()
            .context("new core configuration is unavailable")?;
        if let Err(error) = persist_owner_core_started(self.owner, clash_config).await {
            return rollback_commit_failure(self.owner, error).await;
        }
        match commit_active_owner_session(self.owner, self.proposed_session_token).await {
            Ok(active) => Ok(active),
            Err(error) => rollback_commit_failure(self.owner, error).await,
        }
    }

    async fn apply_new_proxy(&mut self) -> AnyResult<ProxyApplyOutcome> {
        apply_service_proxy_or_direct(self.macos_proxy).await
    }
}

async fn rollback_commit_failure<T>(
    owner: &AuthenticatedOwner,
    error: anyhow::Error,
) -> AnyResult<T> {
    match rollback_started_owner(owner).await {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(anyhow!(
            "{error:#}; failed to roll back uncommitted owner core: {rollback_error:#}"
        )),
    }
}

#[cfg(all(target_os = "macos", not(feature = "test")))]
async fn clear_service_proxy() -> AnyResult<()> {
    clear_proxy().await
}

#[cfg(any(not(target_os = "macos"), feature = "test"))]
async fn clear_service_proxy() -> AnyResult<()> {
    let _ = clear_proxy;
    Ok(())
}

#[cfg(all(target_os = "macos", not(feature = "test")))]
async fn compensate_service_proxy() -> AnyResult<()> {
    apply_proxy(&MacosProxyConfig::Disabled).await
}

#[cfg(any(not(target_os = "macos"), feature = "test"))]
async fn compensate_service_proxy() -> AnyResult<()> {
    let _ = apply_proxy;
    Ok(())
}

#[cfg(all(target_os = "macos", not(feature = "test")))]
async fn apply_service_proxy_or_direct(
    config: Option<&MacosProxyConfig>,
) -> AnyResult<ProxyApplyOutcome> {
    apply_proxy_or_direct(config).await
}

#[cfg(feature = "test")]
async fn apply_service_proxy_or_direct(
    config: Option<&MacosProxyConfig>,
) -> AnyResult<ProxyApplyOutcome> {
    let _ = apply_proxy_or_direct;
    test_proxy_barrier_block_if_armed().await;
    Ok(if config.is_some() {
        ProxyApplyOutcome::Applied
    } else {
        ProxyApplyOutcome::NotRequested
    })
}

#[cfg(all(not(target_os = "macos"), not(feature = "test")))]
async fn apply_service_proxy_or_direct(
    config: Option<&MacosProxyConfig>,
) -> AnyResult<ProxyApplyOutcome> {
    apply_proxy_or_direct(config).await
}

async fn clear_proxy_with_direct_compensation() -> std::result::Result<(), ServiceError> {
    let Err(clear_error) = clear_service_proxy().await else {
        return Ok(());
    };
    let compensation = compensate_service_proxy().await;
    let message = match compensation {
        Ok(()) => format!("Failed to clear the active owner's proxy: {clear_error:#}"),
        Err(compensation_error) => format!(
            "Failed to clear the active owner's proxy: {clear_error:#}; direct compensation failed: {compensation_error:#}"
        ),
    };
    Err(ServiceError::proxy_clear_failed(message))
}

async fn rollback_started_owner(owner: &AuthenticatedOwner) -> AnyResult<()> {
    if let Err(stop_error) = CORE_MANAGER.lock().await.stop_core().await {
        set_core_lifecycle_state(ServiceLifecycleState::Fatal);
        return Err(anyhow!(
            "failed to terminate owner core during rollback: {stop_error:#}"
        ));
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

    #[cfg(unix)]
    {
        use platform_lib::{S_IRGRP, S_IROTH, S_IRUSR, S_IWGRP, S_IWOTH, S_IWUSR, mode_t};

        let mode: mode_t =
            platform_lib::mode_t::from(S_IRUSR | S_IWUSR | S_IRGRP | S_IWGRP | S_IROTH | S_IWOTH);
        let server = server.with_listener_mode(mode);
        Ok(server)
    }

    #[cfg(windows)]
    {
        let server = server.with_listener_security_descriptor(WINDOWS_CONTROL_PIPE_SDDL);
        Ok(server)
    }
}

fn require_protocol_version(
    ctx: &kode_bridge::RequestContext,
) -> std::result::Result<(), ServiceError> {
    let supplied = ctx
        .headers
        .get(SERVICE_PROTOCOL_HEADER)
        .and_then(|value| value.to_str().ok());
    if supplied == Some(VERSION) {
        Ok(())
    } else {
        Err(ServiceError::protocol_mismatch())
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
            if let Err(error) = require_protocol_version(&ctx) {
                return service_error(error);
            }
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
            if let Err(error) = require_protocol_version(&ctx) {
                return service_error(error);
            }
            let request = match ctx.json::<AuthenticatedRequest<StartClashRequest>>() {
                Ok(request) => request,
                Err(error) => return bad_request(format!("Invalid JSON: {error}")),
            };
            let owner = match authenticate_owner(&ctx, &request.credentials) {
                Ok(owner) => owner,
                Err(error) => return service_error(error),
            };
            let start_request = request.payload;
            if hash_session_token(&start_request.proposed_session_token).is_err() {
                return bad_request("Invalid proposed owner session token");
            }
            if let Some(proxy) = start_request.macos_proxy.as_ref()
                && let Err(error) = validate_proxy_config(proxy)
            {
                return service_error(ServiceError::invalid_proxy_config(error.to_string()));
            }
            #[cfg(feature = "test")]
            test_proxy_barrier_note_start_waiting();
            let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
            let staged_runtime = match stage_runtime(&owner, &start_request.runtime).await {
                Ok(staged) => staged,
                Err(error) => return service_error(error),
            };
            let previous_owner = match load_active_owner().await {
                Ok(owner) => owner,
                Err(error) => {
                    return service_unavailable(format!("Failed to load active owner: {error}"));
                }
            };
            let mut transition = StartOwnerTransition {
                previous_owner,
                owner: &owner,
                staged_runtime: Some(staged_runtime),
                clash_config: None,
                proposed_session_token: &start_request.proposed_session_token,
                macos_proxy: start_request.macos_proxy.as_ref(),
            };
            let (active, proxy_outcome) = match owner_proxy_transition(&mut transition).await {
                Ok(result) => result,
                Err(error) => return service_error(error),
            };
            if let Err(error) = cleanup_legacy_owner_files(&owner).await {
                warn!(
                    "Core start committed, but legacy owner cleanup will be retried later: {error}"
                );
            }
            info!("Core started successfully");
            ok_json(StartClashResult {
                session: OwnerSessionHandle {
                    generation: active.generation,
                },
                proxy_outcome,
            })
        })
        .get(IpcCommand::GetClashLogs.as_ref(), |ctx| async move {
            trace!("Received GetClashLogs command");
            if let Err(error) = require_protocol_version(&ctx) {
                return service_error(error);
            }
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
            if let Err(error) = require_protocol_version(&ctx) {
                return service_error(error);
            }
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
                Err(error) => {
                    service_unavailable(format!("Failed to read core log snapshot: {error}"))
                }
            }
        })
        .delete(IpcCommand::StopClash.as_ref(), |ctx| async move {
            trace!("Received StopClash command");
            if let Err(error) = require_protocol_version(&ctx) {
                return service_error(error);
            }
            let request = match ctx.json::<AuthenticatedSessionRequest<()>>() {
                Ok(request) => request,
                Err(error) => return bad_request(format!("Invalid JSON: {error}")),
            };
            let owner = match authenticate_owner(&ctx, &request.credentials) {
                Ok(owner) => owner,
                Err(error) => return service_error(error),
            };
            let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
            if let Err(error) = require_active_session(&owner, &request.session).await {
                return service_error(error);
            }
            if let Err(error) = clear_proxy_with_direct_compensation().await {
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
            if let Err(e) = clear_active_owner().await {
                set_core_lifecycle_state(ServiceLifecycleState::Fatal);
                return service_unavailable(format!("Failed to clear active owner: {}", e));
            }
            ok_empty("Core stopped successfully")
        })
        .put(IpcCommand::UpdateWriter.as_ref(), |ctx| async move {
            trace!("Received UpdateWriter command");
            if let Err(error) = require_protocol_version(&ctx) {
                return service_error(error);
            }
            match ctx.json::<AuthenticatedSessionRequest<WriterConfig>>() {
                Ok(request) => {
                    let owner = match authenticate_owner(&ctx, &request.credentials) {
                        Ok(owner) => owner,
                        Err(error) => return service_error(error),
                    };
                    let mut writer_config = request.payload;
                    let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
                    if let Err(error) = require_active_session(&owner, &request.session).await {
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
        })
        .put(IpcCommand::SetSystemProxy.as_ref(), |ctx| async move {
            trace!("Received SetSystemProxy command");
            if let Err(error) = require_protocol_version(&ctx) {
                return service_error(error);
            }
            let request = match ctx.json::<AuthenticatedSessionRequest<MacosProxyConfig>>() {
                Ok(request) => request,
                Err(error) => return bad_request(format!("Invalid JSON: {error}")),
            };
            let owner = match authenticate_owner(&ctx, &request.credentials) {
                Ok(owner) => owner,
                Err(error) => return service_error(error),
            };
            let _lifecycle_guard = OWNER_LIFECYCLE_LOCK.lock().await;
            if let Err(error) = require_active_session(&owner, &request.session).await {
                return service_error(error);
            }
            if let Err(error) = validate_proxy_config(&request.payload) {
                return service_error(ServiceError::invalid_proxy_config(error.to_string()));
            }
            match apply_service_proxy_or_direct(Some(&request.payload)).await {
                Ok(outcome) => ok_json(outcome),
                Err(error) => service_error(ServiceError::proxy_apply_failed(error.to_string())),
            }
        });
    #[cfg(feature = "test")]
    let router = router
        .post("/__test/proxy-barrier/arm", |_ctx| async move {
            test_proxy_barrier_arm();
            ok_empty("Proxy barrier armed")
        })
        .get("/__test/proxy-barrier/proxy-entered", |_ctx| async move {
            test_proxy_barrier_wait(TEST_PROXY_ENTERED, &TEST_PROXY_ENTERED_NOTIFY).await;
            ok_empty("Proxy operation entered")
        })
        .get("/__test/proxy-barrier/start-waiting", |_ctx| async move {
            test_proxy_barrier_wait(TEST_START_WAITING, &TEST_START_WAITING_NOTIFY).await;
            ok_empty("Start is waiting")
        })
        .post("/__test/proxy-barrier/release", |_ctx| async move {
            test_proxy_barrier_release();
            ok_empty("Proxy barrier released")
        })
        .post("/__test/proxy-barrier/reset", |_ctx| async move {
            test_proxy_barrier_reset();
            ok_empty("Proxy barrier reset")
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

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

pub async fn require_active_session(
    owner: &AuthenticatedOwner,
    proof: &OwnerSessionProof,
) -> std::result::Result<ActiveOwnerState, ServiceError> {
    let active = load_active_owner()
        .await
        .map_err(|_| ServiceError::stale_owner_session())?
        .ok_or_else(ServiceError::stale_owner_session)?;
    let supplied_hash =
        hash_session_token(&proof.token).map_err(|_| ServiceError::stale_owner_session())?;
    if active.owner_key != owner.key
        || active.generation != proof.generation
        || !constant_time_eq(
            active.session_token_hash.as_bytes(),
            supplied_hash.as_bytes(),
        )
    {
        return Err(ServiceError::stale_owner_session());
    }
    Ok(active)
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

#[cfg(feature = "test")]
const TEST_PROXY_ARMED: u8 = 1 << 0;
#[cfg(feature = "test")]
const TEST_PROXY_ENTERED: u8 = 1 << 1;
#[cfg(feature = "test")]
const TEST_START_WAITING: u8 = 1 << 2;
#[cfg(feature = "test")]
const TEST_PROXY_RELEASED: u8 = 1 << 3;
#[cfg(feature = "test")]
static TEST_PROXY_BARRIER_STATE: AtomicU8 = AtomicU8::new(0);
#[cfg(feature = "test")]
static TEST_PROXY_ENTERED_NOTIFY: Lazy<Notify> = Lazy::new(Notify::new);
#[cfg(feature = "test")]
static TEST_START_WAITING_NOTIFY: Lazy<Notify> = Lazy::new(Notify::new);
#[cfg(feature = "test")]
static TEST_PROXY_RELEASE_NOTIFY: Lazy<Notify> = Lazy::new(Notify::new);

#[cfg(feature = "test")]
fn test_proxy_barrier_arm() {
    TEST_PROXY_BARRIER_STATE.store(TEST_PROXY_ARMED, Ordering::Release);
}

#[cfg(feature = "test")]
async fn test_proxy_barrier_block_if_armed() {
    if TEST_PROXY_BARRIER_STATE
        .compare_exchange(
            TEST_PROXY_ARMED,
            TEST_PROXY_ARMED | TEST_PROXY_ENTERED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    TEST_PROXY_ENTERED_NOTIFY.notify_waiters();
    test_proxy_barrier_wait(TEST_PROXY_RELEASED, &TEST_PROXY_RELEASE_NOTIFY).await;
}

#[cfg(feature = "test")]
fn test_proxy_barrier_note_start_waiting() {
    let state = TEST_PROXY_BARRIER_STATE.load(Ordering::Acquire);
    if state & TEST_PROXY_ENTERED == 0 || state & TEST_PROXY_RELEASED != 0 {
        return;
    }
    if OWNER_LIFECYCLE_LOCK.try_lock().is_err() {
        TEST_PROXY_BARRIER_STATE.fetch_or(TEST_START_WAITING, Ordering::AcqRel);
        TEST_START_WAITING_NOTIFY.notify_waiters();
    }
}

#[cfg(feature = "test")]
async fn test_proxy_barrier_wait(required: u8, notify: &Notify) {
    loop {
        let notified = notify.notified();
        if TEST_PROXY_BARRIER_STATE.load(Ordering::Acquire) & required == required {
            return;
        }
        notified.await;
    }
}

#[cfg(feature = "test")]
fn test_proxy_barrier_release() {
    TEST_PROXY_BARRIER_STATE.fetch_or(TEST_PROXY_RELEASED, Ordering::AcqRel);
    TEST_PROXY_RELEASE_NOTIFY.notify_waiters();
}

#[cfg(feature = "test")]
fn test_proxy_barrier_reset() {
    TEST_PROXY_BARRIER_STATE.store(0, Ordering::Release);
}

#[cfg(test)]
mod owner_lifecycle_tests {
    use super::{
        OwnerProxyTransition, WINDOWS_CONTROL_PIPE_SDDL, owner_proxy_transition,
        require_active_owner, require_active_session,
    };
    use crate::ServiceErrorCode;
    use crate::core::auth::AuthenticatedOwner;
    use crate::core::desired::{
        ActiveOwnerState, clear_active_owner, commit_active_owner_session, persist_active_owner,
    };
    use crate::{OwnerIdentity, OwnerSessionProof, ProxyApplyOutcome};
    use serial_test::serial;

    fn owner(uid: u32) -> AuthenticatedOwner {
        AuthenticatedOwner {
            key: uid.to_string(),
            identity: OwnerIdentity::Unix { uid, gid: 20 },
            app_data_root: std::env::temp_dir(),
        }
    }

    struct RecordingTransition {
        events: Vec<&'static str>,
        active_owner: ActiveOwnerState,
        running_pid: u32,
        next_owner: ActiveOwnerState,
        clear_fails: bool,
        apply_falls_back: bool,
    }

    impl OwnerProxyTransition for RecordingTransition {
        async fn clear_previous_proxy(&mut self) -> anyhow::Result<()> {
            self.events.push("clear_proxy");
            if self.clear_fails {
                anyhow::bail!("clear failed");
            }
            Ok(())
        }

        async fn compensate_direct(&mut self) -> anyhow::Result<()> {
            self.events.push("compensate_direct");
            Ok(())
        }

        async fn stop_previous_core(&mut self) -> anyhow::Result<()> {
            self.events.push("stop_a");
            self.running_pid = 0;
            Ok(())
        }

        async fn start_new_core(&mut self) -> anyhow::Result<()> {
            self.events.push("start_b");
            self.running_pid = 202;
            Ok(())
        }

        async fn commit_new_owner(&mut self) -> anyhow::Result<ActiveOwnerState> {
            self.events.push("commit_b");
            self.active_owner = self.next_owner.clone();
            Ok(self.active_owner.clone())
        }

        async fn apply_new_proxy(&mut self) -> anyhow::Result<ProxyApplyOutcome> {
            self.events.push("apply_b");
            if self.apply_falls_back {
                self.events.push("compensate_direct");
                return Ok(ProxyApplyOutcome::DirectFallback {
                    message: "apply failed".to_owned(),
                });
            }
            Ok(ProxyApplyOutcome::Applied)
        }
    }

    fn recording_transition() -> RecordingTransition {
        RecordingTransition {
            events: Vec::new(),
            active_owner: ActiveOwnerState::from(&owner(96_001)),
            running_pid: 101,
            next_owner: ActiveOwnerState::from(&owner(96_002)),
            clear_fails: false,
            apply_falls_back: false,
        }
    }

    #[tokio::test]
    async fn owner_proxy_transition_successful_takeover_has_exact_order() -> anyhow::Result<()> {
        let mut transition = recording_transition();

        let (_, outcome) = owner_proxy_transition(&mut transition).await?;

        assert_eq!(
            transition.events,
            ["clear_proxy", "stop_a", "start_b", "commit_b", "apply_b"]
        );
        assert_eq!(transition.active_owner.owner_key, "96002");
        assert_eq!(transition.running_pid, 202);
        assert_eq!(outcome, ProxyApplyOutcome::Applied);
        Ok(())
    }

    #[tokio::test]
    async fn owner_proxy_transition_clear_failure_preserves_old_owner_and_core() {
        let mut transition = recording_transition();
        transition.clear_fails = true;

        let error = owner_proxy_transition(&mut transition)
            .await
            .expect_err("proxy clear failure must abort takeover");

        assert_eq!(error.code, ServiceErrorCode::ProxyClearFailed);
        assert_eq!(transition.events, ["clear_proxy", "compensate_direct"]);
        assert_eq!(transition.active_owner.owner_key, "96001");
        assert_eq!(transition.running_pid, 101);
    }

    #[tokio::test]
    async fn owner_proxy_transition_apply_failure_keeps_new_owner_and_core() -> anyhow::Result<()> {
        let mut transition = recording_transition();
        transition.apply_falls_back = true;

        let (active, outcome) = owner_proxy_transition(&mut transition).await?;

        assert_eq!(active.owner_key, "96002");
        assert_eq!(transition.active_owner.owner_key, "96002");
        assert_eq!(transition.running_pid, 202);
        assert_eq!(
            outcome,
            ProxyApplyOutcome::DirectFallback {
                message: "apply failed".to_owned(),
            }
        );
        Ok(())
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
        commit_active_owner_session(&active, &"10".repeat(32)).await?;

        let error = require_active_owner(&inactive)
            .await
            .expect_err("non-active owner must be rejected");

        assert_eq!(error.code, ServiceErrorCode::NotActive);
        clear_active_owner().await?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn same_owner_new_session_invalidates_old_proof() -> anyhow::Result<()> {
        clear_active_owner().await?;
        let owner = owner(95_001);
        let first = commit_active_owner_session(&owner, &"11".repeat(32)).await?;
        let first_proof = OwnerSessionProof {
            generation: first.generation,
            token: "11".repeat(32),
        };
        require_active_session(&owner, &first_proof).await?;
        let second = commit_active_owner_session(&owner, &"22".repeat(32)).await?;
        assert!(second.generation > first.generation);
        assert_eq!(
            require_active_session(&owner, &first_proof)
                .await
                .expect_err("old proof must be stale")
                .code,
            ServiceErrorCode::StaleOwnerSession,
        );
        clear_active_owner().await?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn legacy_active_owner_session_fails_closed() -> anyhow::Result<()> {
        clear_active_owner().await?;
        let owner = owner(95_002);
        persist_active_owner(&owner).await?;
        let proof = OwnerSessionProof {
            generation: 0,
            token: "55".repeat(32),
        };

        assert_eq!(
            require_active_session(&owner, &proof)
                .await
                .expect_err("legacy owner must not authenticate a session")
                .code,
            ServiceErrorCode::StaleOwnerSession,
        );
        clear_active_owner().await?;
        Ok(())
    }
}
