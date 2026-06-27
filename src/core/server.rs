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

const IPC_MAX_RESTARTS: u32 = 10;
const IPC_RESTART_WINDOW: Duration = Duration::from_secs(10);
const IPC_MAX_BACKOFF: Duration = Duration::from_millis(500);

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

    Duration::from_millis(100u64 << (attempt - 1).min(3)).min(IPC_MAX_BACKOFF)
}

/// 解析 IPC 目录(`/tmp/verge`)应归属的组 GID。
///
/// launchd 下服务以 root:wheel 运行且 **没有 `SUDO_GID`**，若直接用 `getgid()`(=wheel)，
/// 非 root 的 GUI 用户会被 0o2770 目录挡在外面(issue #7333：重启后服务连接失败、TUN
/// 失效;而手动 `sudo` 因带 `SUDO_GID` 反而正常)。解析优先级：
/// 1. `SUDO_GID`(终端 sudo 安装)
/// 2. macOS 控制台登录用户的主组(运行期 GUI 在线时最准，覆盖非 staff 主组的账户)
/// 3. macOS `staff` 组(GUI 账户默认主组，开机早于登录时仍可用，修掉开机竞态)
/// 4. `getgid()` 兜底(保持原行为)
#[cfg(unix)]
fn resolve_ipc_dir_gid() -> platform_lib::gid_t {
    if let Some(gid) = std::env::var("SUDO_GID")
        .ok()
        .and_then(|s| s.parse::<platform_lib::gid_t>().ok())
    {
        return gid;
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(gid) = macos_console_user_gid() {
            return gid;
        }
        if let Some(gid) = macos_group_gid(c"staff") {
            return gid;
        }
    }

    unsafe { platform_lib::getgid() }
}

/// macOS：控制台(GUI 登录)用户的主组 GID。`/dev/console` 的属主即当前 GUI 用户，
/// 无人登录时其属主为 root，返回 `None`。
#[cfg(target_os = "macos")]
fn macos_console_user_gid() -> Option<platform_lib::gid_t> {
    use std::os::unix::fs::MetadataExt;

    let uid = std::fs::metadata("/dev/console").ok()?.uid();
    if uid == 0 {
        return None;
    }
    let pw = unsafe { platform_lib::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    Some(unsafe { (*pw).pw_gid })
}

/// macOS：按名解析组 GID(如 "staff")。
#[cfg(target_os = "macos")]
fn macos_group_gid(name: &std::ffi::CStr) -> Option<platform_lib::gid_t> {
    let grp = unsafe { platform_lib::getgrnam(name.as_ptr()) };
    if grp.is_null() {
        return None;
    }
    Some(unsafe { (*grp).gr_gid })
}

/// 确保 IPC 目录存在并设置属组与权限：属主保持 root、属组设为 GUI 用户可访问的组、
/// 模式 0o2770。`make_ipc_dir` 与看门狗共用,保证一致。
///
/// 见 issue #6149：SetGID(0o2000)让 socket 继承目录组；0o2770(rwxrws---)让 root 与
/// 目标用户组都能管理 socket 生命周期(GUI 进程以非 root 重建 socket / sidecar 回退时)。
///
/// 安全：`/tmp` 世界可写,攻击者可能预置 `/tmp/verge` 为指向任意目标的 symlink。故先用
/// `symlink_metadata`(lstat)确认是真实目录(否则删除重建),再用 `O_DIRECTORY|O_NOFOLLOW`
/// 打开后对 **fd** 做 `fchown`/`fchmod`：即便有人在创建后竞态替换成 symlink,`open` 也会
/// 因 ELOOP 失败,绝不会以 root 跟随 symlink 对任意目标改属/改权。
#[cfg(unix)]
fn ensure_ipc_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    // 1) 确保路径是真实目录(非 symlink/文件)
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            warn!(
                "Unexpected non-directory at {:?}; removing and recreating",
                dir
            );
            std::fs::remove_file(dir)?;
            std::fs::create_dir_all(dir)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
        }
        Err(e) => return Err(e),
    }

    // 2) no-follow 打开目录,再对 fd 改属/改权
    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let fd = unsafe {
        platform_lib::open(
            c_path.as_ptr(),
            platform_lib::O_DIRECTORY | platform_lib::O_NOFOLLOW | platform_lib::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let gid = resolve_ipc_dir_gid();
    // 以 root 运行(生产/launchd)时强制属主为 root:若攻击者预置 `/tmp/verge` 为其拥有的
    // 真实目录,必须夺回属主,否则其以 owner 身份保留对目录(及内部 socket)的管理权。
    // 非 root(测试/开发)既无权设 root 属主也无攻击面,跳过 chown,仅设权限位。
    // chown/chmod 失败即 fatal;`&&`/`||` 短路使 `last_os_error()` 为失败 syscall 的 errno。
    let chown_ok =
        unsafe { platform_lib::geteuid() } != 0 || unsafe { platform_lib::fchown(fd, 0, gid) } == 0;
    let ok = chown_ok && unsafe { platform_lib::fchmod(fd, 0o2770 as platform_lib::mode_t) } == 0;
    let result = if ok {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    };
    unsafe {
        platform_lib::close(fd);
    }
    result
}

async fn make_ipc_dir() -> Result<()> {
    #[cfg(unix)]
    {
        let paths = service_paths();
        let Some(dir_path) = paths.ipc_path().parent() else {
            return Ok(());
        };

        ensure_ipc_dir(dir_path)?;
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
            let Some(dir) = socket_path.parent() else {
                continue;
            };

            if !dir.exists() {
                warn!("IPC socket directory {:?} was deleted, recreating", dir);
                if let Err(e) = ensure_ipc_dir(dir) {
                    warn!("Failed to recreate IPC socket directory {:?}: {}", dir, e);
                    continue;
                }
                info!("IPC socket directory {:?} recreated", dir);
                continue;
            }

            // 目录已存在：组属可能过期(开机落 staff、之后控制台用户主组不同)，或被替换为
            // symlink/文件。用 lstat(no-follow)判断，必要时经 ensure_ipc_dir 安全收敛
            // (issue #7333 开机竞态尾部 + /tmp symlink 防护)。一致则跳过，避免抖动。
            use std::os::unix::fs::MetadataExt;
            let needs_fix = match std::fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_dir() => meta.gid() != resolve_ipc_dir_gid(),
                _ => true,
            };
            if needs_fix && let Err(e) = ensure_ipc_dir(dir) {
                warn!("Failed to re-apply ownership on {:?}: {}", dir, e);
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn staff_group_resolves_to_valid_gid() {
        // macOS GUI 账户默认主组 staff 必须能解析；开机竞态兜底依赖它，
        // 且其 gid(20)必须 != wheel(0)，否则又会把 GUI 用户挡在外面(issue #7333)。
        let gid = macos_group_gid(c"staff").expect("staff group must exist on macOS");
        assert_eq!(gid, 20, "staff 在 macOS 上固定为 gid 20");
        assert_ne!(gid, 0, "解析出的组不能是 wheel(0)");
    }

    #[test]
    fn unknown_group_resolves_to_none() {
        assert!(macos_group_gid(c"clash-verge-no-such-group-zzz").is_none());
    }
}
