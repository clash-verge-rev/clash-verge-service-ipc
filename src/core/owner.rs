use crate::core::paths::{ServicePaths, service_paths};
use crate::core::process::{is_process_alive, terminate_process};
use crate::{IPC_AUTH_EXPECT, IpcCommand};
use anyhow::{Context, Result, anyhow};
use kode_bridge::{ClientConfig, IpcHttpClient};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

const OWNER_HEALTH_ATTEMPTS: usize = 20;
const OWNER_HEALTH_RETRY_DELAY: Duration = Duration::from_millis(100);
const OWNER_REACQUIRE_ATTEMPTS: usize = 10;
const OWNER_REACQUIRE_DELAY: Duration = Duration::from_millis(100);

pub struct ServiceOwnerGuard {
    _file: File,
    paths: ServicePaths,
}

impl ServiceOwnerGuard {
    fn new(mut file: File, paths: ServicePaths) -> Result<Self> {
        let pid = std::process::id();
        write_owner_metadata(&mut file, &paths, pid)?;
        Ok(Self { _file: file, paths })
    }
}

impl Drop for ServiceOwnerGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            unsafe {
                platform_lib::flock(self._file.as_raw_fd(), platform_lib::LOCK_UN);
            }
        }

        let _ = std::fs::remove_file(self.paths.pid_file_path());

        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(self.paths.owner_lock_path());
        }

        info!(
            "Released service owner lock: {:?}",
            self.paths.owner_lock_path()
        );
    }
}

pub async fn acquire_service_owner() -> Result<Option<ServiceOwnerGuard>> {
    let paths = service_paths();
    std::fs::create_dir_all(paths.runtime_dir()).with_context(|| {
        format!(
            "failed to create runtime directory {:?}",
            paths.runtime_dir()
        )
    })?;

    if let Some(guard) = try_acquire_owner_once(&paths)? {
        info!("Acquired service owner lock: {:?}", paths.owner_lock_path());
        return Ok(Some(guard));
    }

    let old_pid = read_owner_pid(&paths);
    warn!(
        "Service owner lock is already held; inspecting old owner: {:?}",
        old_pid
    );

    if wait_for_owner_health(&paths).await {
        info!("Existing service owner is reachable; current process will exit");
        return Ok(None);
    }

    warn!("Existing service owner is not reachable; cleaning old owner state");
    if let Some(pid) = old_pid
        && pid != std::process::id()
        && is_process_alive(pid)
    {
        terminate_process(pid).await;
    }
    cleanup_runtime_artifacts(&paths);

    for attempt in 1..=OWNER_REACQUIRE_ATTEMPTS {
        if let Some(guard) = try_acquire_owner_once(&paths)? {
            info!(
                "Acquired service owner lock after cleanup on attempt {}",
                attempt
            );
            return Ok(Some(guard));
        }

        tokio::time::sleep(OWNER_REACQUIRE_DELAY).await;
    }

    Err(anyhow!(
        "failed to acquire service owner lock after stale owner cleanup"
    ))
}

fn try_acquire_owner_once(paths: &ServicePaths) -> Result<Option<ServiceOwnerGuard>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.owner_lock_path())
            .with_context(|| format!("failed to open owner lock {:?}", paths.owner_lock_path()))?;

        let result = unsafe {
            platform_lib::flock(
                file.as_raw_fd(),
                platform_lib::LOCK_EX | platform_lib::LOCK_NB,
            )
        };

        if result == 0 {
            return ServiceOwnerGuard::new(file, paths.clone()).map(Some);
        }

        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
        ) {
            return Ok(None);
        }

        Err(error).with_context(|| format!("failed to lock {:?}", paths.owner_lock_path()))
    }

    #[cfg(windows)]
    {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(paths.owner_lock_path())
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {:?}", paths.owner_lock_path()));
            }
        };

        ServiceOwnerGuard::new(file, paths.clone()).map(Some)
    }
}

fn write_owner_metadata(file: &mut File, paths: &ServicePaths, pid: u32) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={pid}")?;
    writeln!(file, "version={}", crate::VERSION)?;
    file.sync_data()?;

    std::fs::write(paths.pid_file_path(), pid.to_string())
        .with_context(|| format!("failed to write pid file {:?}", paths.pid_file_path()))?;

    Ok(())
}

fn read_owner_pid(paths: &ServicePaths) -> Option<u32> {
    read_pid_file(paths.pid_file_path()).or_else(|| read_owner_lock_pid(paths.owner_lock_path()))
}

fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u32>().ok())
}

fn read_owner_lock_pid(path: &Path) -> Option<u32> {
    let mut content = String::new();
    File::open(path).ok()?.read_to_string(&mut content).ok()?;

    content.lines().find_map(|line| {
        let pid = line.strip_prefix("pid=")?;
        pid.trim().parse::<u32>().ok()
    })
}

async fn wait_for_owner_health(paths: &ServicePaths) -> bool {
    for attempt in 1..=OWNER_HEALTH_ATTEMPTS {
        if is_ipc_healthy(paths).await {
            return true;
        }

        if attempt < OWNER_HEALTH_ATTEMPTS {
            tokio::time::sleep(OWNER_HEALTH_RETRY_DELAY).await;
        }
    }

    false
}

async fn is_ipc_healthy(paths: &ServicePaths) -> bool {
    let client = match IpcHttpClient::with_config(
        paths.ipc_path(),
        ClientConfig {
            default_timeout: Duration::from_millis(150),
            max_retries: 1,
            retry_delay: Duration::from_millis(25),
            enable_pooling: false,
            ..Default::default()
        },
    ) {
        Ok(client) => client,
        Err(error) => {
            warn!("Failed to create IPC health client: {}", error);
            return false;
        }
    };

    match client
        .get(IpcCommand::Magic.as_ref())
        .header("X-IPC-Magic", IPC_AUTH_EXPECT)
        .send()
        .await
    {
        Ok(response) => response.is_success(),
        Err(error) => {
            warn!("IPC owner health probe failed: {}", error);
            false
        }
    }
}

fn cleanup_runtime_artifacts(paths: &ServicePaths) {
    let _ = std::fs::remove_file(paths.pid_file_path());

    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(paths.ipc_path());
    }

    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(paths.owner_lock_path());
    }
}
