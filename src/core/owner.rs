use crate::core::paths::{ServicePaths, service_paths};
use crate::core::process::{ProcessIdentity, is_process_alive, process_identity, terminate_process_if_identity};
use crate::{IPC_AUTH_EXPECT, IpcCommand};
use anyhow::{Context, Result, anyhow};
use kode_bridge::{ClientConfig, IpcHttpClient};
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Read;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

const OWNER_HEALTH_ATTEMPTS: usize = 20;
const OWNER_HEALTH_RETRY_DELAY: Duration = Duration::from_millis(100);
const OWNER_REACQUIRE_ATTEMPTS: usize = 10;
const OWNER_REACQUIRE_DELAY: Duration = Duration::from_millis(100);

pub struct ServiceOwnerGuard {
    _file: File,
    #[cfg(windows)]
    legacy_pid_file: std::path::PathBuf,
}

impl ServiceOwnerGuard {
    fn new(mut file: File, paths: ServicePaths) -> Result<Self> {
        let pid = std::process::id();
        write_owner_metadata(&mut file, pid)?;
        // Unix rollback can read lock metadata; old Windows readers cannot read the locked range.
        #[cfg(unix)]
        if let Err(error) = std::fs::remove_file(paths.pid_file_path())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!("Failed to retire legacy owner PID file: {error}");
        }
        #[cfg(windows)]
        std::fs::write(paths.pid_file_path(), pid.to_string())?;
        Ok(Self {
            _file: file,
            #[cfg(windows)]
            legacy_pid_file: paths.pid_file_path().to_path_buf(),
        })
    }
}

#[cfg(windows)]
impl Drop for ServiceOwnerGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.legacy_pid_file);
    }
}

pub async fn acquire_service_owner() -> Result<Option<ServiceOwnerGuard>> {
    let paths = service_paths()?;
    crate::core::paths::ensure_persistent_state_layout()?;
    #[cfg(unix)]
    crate::core::unix_security::ensure_service_directory(paths.runtime_dir(), 0o755)?;
    #[cfg(windows)]
    crate::core::windows_security::ensure_private_service_directory(paths.runtime_dir())?;

    if let Some(guard) = try_acquire_owner_once(&paths)? {
        info!("Acquired service owner lock: {:?}", paths.owner_lock_path());
        return Ok(Some(guard));
    }

    let old_owner = read_owner_metadata(paths.owner_lock_path());
    let old_identity = old_owner
        .as_ref()
        .and_then(|owner| process_identity(owner.pid).ok().flatten());
    warn!("Service owner lock is already held; checking its health");
    if wait_for_owner_health(&paths).await {
        info!("Existing service owner is reachable; current process will exit");
        return Ok(None);
    }
    if let Some(guard) = try_acquire_owner_once(&paths)? {
        return Ok(Some(guard));
    }
    let old_owner =
        old_owner.context("service owner lock is held but its PID is unavailable; refusing unsafe lock takeover")?;
    let old_identity = old_identity.context("service owner identity is unavailable; refusing unsafe lock takeover")?;
    anyhow::ensure!(
        old_owner.pid != std::process::id(),
        "current process already holds the service owner lock"
    );
    anyhow::ensure!(
        read_owner_metadata(paths.owner_lock_path()).as_ref() == Some(&old_owner),
        "service owner changed during health checks; refusing unsafe lock takeover"
    );
    verify_owner_identity(&old_owner, &old_identity)?;
    warn!("Existing service owner is not reachable; stopping the verified helper before lock takeover");
    terminate_process_if_identity(old_owner.pid, &old_identity, Duration::from_secs(1)).await?;
    anyhow::ensure!(
        !is_process_alive(old_owner.pid),
        "old service owner is still alive; refusing lock takeover"
    );

    for attempt in 1..=OWNER_REACQUIRE_ATTEMPTS {
        if let Some(guard) = try_acquire_owner_once(&paths)? {
            info!("Acquired service owner lock after cleanup on attempt {}", attempt);
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

        let result = unsafe { platform_lib::flock(file.as_raw_fd(), platform_lib::LOCK_EX | platform_lib::LOCK_NB) };

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
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, GetLastError};
        use windows_sys::Win32::Storage::FileSystem::LockFile;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.owner_lock_path())
            .with_context(|| format!("failed to open {:?}", paths.owner_lock_path()))?;
        if unsafe { LockFile(file.as_raw_handle(), 0, 0, u32::MAX, u32::MAX) } == 0 {
            if unsafe { GetLastError() } == ERROR_LOCK_VIOLATION {
                return Ok(None);
            }
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to lock {:?}", paths.owner_lock_path()));
        }

        ServiceOwnerGuard::new(file, paths.clone()).map(Some)
    }
}

fn write_owner_metadata(file: &mut File, pid: u32) -> Result<()> {
    let identity = process_identity(pid)?.context("service process identity is unavailable")?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={pid}")?;
    writeln!(file, "version={}", crate::VERSION)?;
    writeln!(file, "identity={}", serde_json::to_string(&identity)?)?;
    file.sync_data()?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct OwnerMetadata {
    pid: u32,
    identity: Option<ProcessIdentity>,
}

fn read_owner_metadata(path: &Path) -> Option<OwnerMetadata> {
    let file = File::open(path).ok()?;
    #[cfg(unix)]
    let content = {
        let mut content = String::new();
        file.take(16 * 1024 + 1).read_to_string(&mut content).ok()?;
        if content.len() > 16 * 1024 {
            return None;
        }
        content
    };
    #[cfg(windows)]
    let content = read_locked_owner_metadata(&file)?;
    let pid = content
        .lines()
        .find_map(|line| line.strip_prefix("pid=")?.parse().ok())?;
    let identity = content
        .lines()
        .find_map(|line| line.strip_prefix("identity="))
        .map(serde_json::from_str)
        .transpose()
        .ok()?;
    Some(OwnerMetadata { pid, identity })
}

#[cfg(windows)]
fn read_locked_owner_metadata(file: &File) -> Option<String> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY, UnmapViewOfFile,
    };
    let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
    if len == 0 || len > 16 * 1024 {
        return None;
    }
    // LockFile's byte-range exclusion permits mapped reads, including old helpers' full-range lock.
    let mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        )
    };
    if mapping.is_null() {
        return None;
    }
    let mapping = unsafe { OwnedHandle::from_raw_handle(mapping) };
    let view = unsafe { MapViewOfFile(mapping.as_raw_handle(), FILE_MAP_READ, 0, 0, len) };
    if view.Value.is_null() {
        return None;
    }
    let content = unsafe { std::slice::from_raw_parts(view.Value.cast::<u8>(), len) }.to_vec();
    unsafe {
        UnmapViewOfFile(view);
    }
    String::from_utf8(content).ok()
}

fn verify_owner_identity(owner: &OwnerMetadata, captured: &ProcessIdentity) -> Result<()> {
    if let Some(recorded) = &owner.identity {
        anyhow::ensure!(
            recorded == captured,
            "owner PID start identity does not match lock metadata"
        );
    } else {
        let helper = std::env::current_exe()?.canonicalize()?;
        anyhow::ensure!(
            Path::new(&captured.executable) == helper,
            "legacy owner PID does not name this helper executable; refusing unsafe lock takeover"
        );
    }
    anyhow::ensure!(
        process_identity(owner.pid)?.as_ref() == Some(captured),
        "service owner identity changed during health checks"
    );
    Ok(())
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
            require_windows_server_system: cfg!(windows),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn metadata_remains_readable_under_the_legacy_full_range_lock() -> Result<()> {
        use std::os::windows::io::AsRawHandle as _;
        let path = std::env::temp_dir().join(format!("service-owner-mapping-{}", std::process::id()));
        let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path)?;
        anyhow::ensure!(
            unsafe {
                windows_sys::Win32::Storage::FileSystem::LockFile(file.as_raw_handle(), 0, 0, u32::MAX, u32::MAX)
            } != 0,
            "fixture lock failed"
        );
        writeln!(file, "pid={}\nversion=2.7.7", std::process::id())?;
        file.flush()?;
        let legacy = read_owner_metadata(&path).context("legacy locked metadata was unreadable")?;
        assert_eq!(legacy.pid, std::process::id());
        assert_eq!(legacy.identity, None);
        write_owner_metadata(&mut file, std::process::id())?;
        let current = read_owner_metadata(&path).context("current locked metadata was unreadable")?;
        assert_eq!(current.identity, process_identity(std::process::id())?);
        drop(file);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn a_reused_owner_pid_cannot_authorize_termination() -> Result<()> {
        let pid = std::process::id();
        let identity = process_identity(pid)?.context("no test process identity")?;
        let mut stale = identity.clone();
        stale.started_at = stale.started_at.wrapping_add(1);
        assert!(
            verify_owner_identity(
                &OwnerMetadata {
                    pid,
                    identity: Some(stale)
                },
                &identity
            )
            .is_err()
        );
        let foreign = ProcessIdentity {
            executable: "/unrelated/fixture".into(),
            ..identity.clone()
        };
        assert!(verify_owner_identity(&OwnerMetadata { pid, identity: None }, &foreign).is_err());
        assert!(verify_owner_identity(&OwnerMetadata { pid, identity: None }, &identity).is_ok());
        Ok(())
    }
}
