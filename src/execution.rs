//! Cooperative exclusion between Service supervision and local Sidecar execution.
use anyhow::{Context as _, Result, bail};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct CoreExecutionGuard {
    _file: File,
}

impl Drop for CoreExecutionGuard {
    fn drop(&mut self) {
        // fork can briefly inherit this open file description before exec closes it.
        let _ = self._file.unlock();
    }
}

impl CoreExecutionGuard {
    pub fn acquire() -> Result<Self> {
        Self::acquire_at(&coordination_path()?)
    }

    fn acquire_at(path: &Path) -> Result<Self> {
        let file = open_coordination_file(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => bail!("another Service or Sidecar owns core execution"),
            Err(std::fs::TryLockError::Error(error)) => Err(error).context("could not reserve core execution"),
        }
    }

    /// Keep the reservation until the OS confirms exit, even if killing or event delivery fails.
    pub fn release_after_exit(
        self,
        pid: u32,
        mut terminated: tokio::sync::oneshot::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut events_open = true;
            while !process_has_exited(pid) {
                tokio::select! {
                    result = &mut terminated, if events_open => {
                        if result.is_ok() { break; }
                        events_open = false;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                }
            }
            drop(self);
        })
    }

    pub fn is_held() -> Result<bool> {
        let file = open_coordination_file(&coordination_path()?)?;
        match file.try_lock() {
            Ok(()) => {
                file.unlock()?;
                Ok(false)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

#[cfg(unix)]
fn process_has_exited(pid: u32) -> bool {
    (unsafe { platform_lib::kill(pid as i32, 0) }) == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::ESRCH)
}

#[cfg(windows)]
fn process_has_exited(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject},
    };
    let raw = unsafe { OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32);
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    (unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) }) == WAIT_OBJECT_0
}

fn coordination_path() -> Result<PathBuf> {
    #[cfg(unix)]
    let root = PathBuf::from("/tmp");
    #[cfg(windows)]
    let root = {
        use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
        let mut buffer = vec![0u16; 32768];
        let size = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        anyhow::ensure!(
            size > 0 && size < buffer.len(),
            "could not locate Windows coordination directory"
        );
        PathBuf::from(String::from_utf16(&buffer[..size])?).join("Temp")
    };
    // Both channels may keep idle helpers, but core execution is shared.
    #[cfg(feature = "test")]
    let name = "clash-verge-service.core-execution-test.lock";
    #[cfg(not(feature = "test"))]
    let name = "clash-verge-service.core-execution.lock";
    Ok(root.join(name))
}

#[cfg(unix)]
fn open_coordination_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(path)
    {
        Ok(file) => {
            file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(platform_lib::O_NOFOLLOW | platform_lib::O_NONBLOCK)
                .open(path)?;
            anyhow::ensure!(file.metadata()?.is_file(), "coordination entry is not a regular file");
            Ok(file)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn open_coordination_file(path: &Path) -> Result<File> {
    use std::os::windows::{ffi::OsStrExt as _, io::FromRawHandle as _};
    use windows_sys::Win32::{
        Foundation::{INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
        },
    };
    let sddl: Vec<u16> = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)(A;;GR;;;AU)\0"
        .encode_utf16()
        .collect();
    let mut descriptor = std::ptr::null_mut();
    anyhow::ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } != 0,
        "could not create coordination permissions"
    );
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x80000000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_ALWAYS,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let error = std::io::Error::last_os_error();
    unsafe {
        LocalFree(descriptor);
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(error.into());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    anyhow::ensure!(
        file.metadata()?.is_file() && !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "coordination entry is not a regular file"
    );
    Ok(file)
}

#[cfg(feature = "client")]
pub async fn check_sidecar_available() -> Result<()> {
    let ipc_failed = match crate::inspect_installation(&[]).await {
        Ok(status) => {
            anyhow::ensure!(
                status
                    .protocol
                    .supports_client(crate::ProtocolVersion::current(), crate::MIN_REQUIRED_SERVICE_REVISION),
                "service upgrade is required before Sidecar handoff"
            );
            anyhow::ensure!(
                !status.core_busy,
                "the service has an active or recovering core session"
            );
            false
        }
        Err(_) => true,
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        #[cfg(windows)]
        {
            if ipc_failed {
                windows_fallback::require_stopped_service()?;
            }
            windows_fallback::require_no_core_process(false)?;
        }
        #[cfg(unix)]
        {
            let mut command = std::process::Command::new("ps");
            #[cfg(target_os = "linux")]
            command.args(["-axo", "comm=,args="]);
            #[cfg(not(target_os = "linux"))]
            command.args(["-axo", "comm="]);
            let output = command.output().context("could not inspect remaining cores")?;
            anyhow::ensure!(output.status.success(), "could not inspect remaining cores");
            require_no_unix_core_processes(&String::from_utf8_lossy(&output.stdout), ipc_failed)?;
        }
        Ok(())
    })
    .await
    .context("core process inspection failed")?
}

#[cfg(all(unix, feature = "client"))]
fn require_no_unix_core_processes(processes: &str, include_service: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    let other_helper = PathBuf::from("/Library/PrivilegedHelperTools")
        .join(format!(
            "{}.bundle",
            if cfg!(feature = "development-channel") {
                "io.github.clash-verge-rev.clash-verge-rev.service"
            } else {
                "io.github.clash-verge-rev.clash-verge-rev.dev.service"
            }
        ))
        .join("Contents/MacOS/clash-verge-service");
    #[cfg(target_os = "linux")]
    let other_helper = PathBuf::from("/var/lib")
        .join(if cfg!(feature = "development-channel") {
            "clash-verge-service"
        } else {
            "clash-verge-service-dev"
        })
        .join("bin/clash-verge-service");
    for process in processes.lines() {
        #[cfg(target_os = "linux")]
        let (process, arguments) = process
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((process.trim(), ""));
        let executable = Path::new(process.trim());
        let name = executable.file_name().and_then(|name| name.to_str()).unwrap_or("");
        anyhow::ensure!(
            !["verge-mihomo", "verge-mihomo-alpha", "verge-mihomo-al"].contains(&name),
            "process {name} remains after IPC failure; refusing a second core"
        );
        if include_service && ["clash-verge-service", "clash-verge-ser"].contains(&name) {
            // Linux comm is truncated; argv[0] retains the installed helper's channel path.
            #[cfg(target_os = "linux")]
            let executable = Path::new(arguments.split_whitespace().next().unwrap_or(""));
            anyhow::ensure!(
                executable == other_helper,
                "process {name} remains after IPC failure; refusing a second core"
            );
        }
    }
    Ok(())
}

#[cfg(feature = "client")]
pub async fn reserve_sidecar() -> Result<CoreExecutionGuard> {
    check_sidecar_available().await?;
    CoreExecutionGuard::acquire()
}

#[cfg(all(windows, feature = "client"))]
#[path = "execution_windows.rs"]
mod windows_fallback;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn termination_event_releases_reservation_even_when_pid_still_exists() -> Result<()> {
        let path = std::env::temp_dir().join(format!("execution-reused-pid-test-{}", std::process::id()));
        let guard = CoreExecutionGuard::acquire_at(&path).context("initial reservation")?;
        let (terminated, receiver) = tokio::sync::oneshot::channel();
        let waiter = guard.release_after_exit(std::process::id(), receiver);
        assert!(CoreExecutionGuard::acquire_at(&path).is_err());
        terminated
            .send(())
            .map_err(|_| anyhow::anyhow!("exit observer dropped"))?;
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter).await??;
        drop(CoreExecutionGuard::acquire_at(&path).context("reservation after termination event")?);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "client"))]
    #[test]
    fn linux_idle_helper_from_other_channel_does_not_block_fallback() -> Result<()> {
        let other = if cfg!(feature = "development-channel") {
            "clash-verge-service"
        } else {
            "clash-verge-service-dev"
        };
        let current = crate::SERVICE_SLUG;
        require_no_unix_core_processes(
            &format!("clash-verge-ser /var/lib/{other}/bin/clash-verge-service"),
            true,
        )?;
        assert!(
            require_no_unix_core_processes(
                &format!("clash-verge-ser /var/lib/{current}/bin/clash-verge-service"),
                true
            )
            .is_err()
        );
        assert!(require_no_unix_core_processes("clash-verge-ser /tmp/clash-verge-service", true).is_err());
        assert!(
            require_no_unix_core_processes(
                &format!(
                    "clash-verge-ser /var/lib/{other}/bin/clash-verge-service\nverge-mihomo-al /tmp/verge-mihomo-alpha"
                ),
                true
            )
            .is_err()
        );
        Ok(())
    }
    #[cfg(all(unix, feature = "client"))]
    #[test]
    fn linux_truncated_alpha_core_blocks_fallback() {
        for ipc_failed in [false, true] {
            assert!(require_no_unix_core_processes("verge-mihomo-al", ipc_failed).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_coordination_entry_does_not_block() -> Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::env::temp_dir().join(format!("execution-fifo-test-{}", std::process::id()));
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        anyhow::ensure!(unsafe { platform_lib::mkfifo(name.as_ptr(), 0o600) } == 0);
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "execution::tests::child_lock_probe"])
            .env("CLASH_VERGE_TEST_LOCK", &path)
            .spawn()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let result = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        if result.is_none() {
            child.kill()?;
            child.wait()?;
        }
        std::fs::remove_file(path)?;
        assert!(
            result.is_some_and(|status| status.success()),
            "opening a FIFO must fail without waiting for a writer"
        );
        Ok(())
    }
    #[cfg(all(target_os = "macos", feature = "client"))]
    #[test]
    fn an_idle_helper_from_the_other_channel_does_not_block_fallback() -> Result<()> {
        let production = "/Library/PrivilegedHelperTools/io.github.clash-verge-rev.clash-verge-rev.service.bundle/Contents/MacOS/clash-verge-service";
        let development = "/Library/PrivilegedHelperTools/io.github.clash-verge-rev.clash-verge-rev.dev.service.bundle/Contents/MacOS/clash-verge-service";
        let (current, other) = if cfg!(feature = "development-channel") {
            (development, production)
        } else {
            (production, development)
        };
        require_no_unix_core_processes(other, true)?;
        for residual in [
            current.to_owned(),
            "clash-verge-service".into(),
            format!("/tmp{other}"),
            format!("{other}\n/Library/Application Support/clash-verge-service/cores/verge-mihomo"),
        ] {
            assert!(
                require_no_unix_core_processes(&residual, true).is_err(),
                "unconfirmed idle state must remain blocked: {residual}"
            );
        }
        Ok(())
    }

    #[test]
    fn child_lock_probe() -> Result<()> {
        if let Some(path) = std::env::var_os("CLASH_VERGE_TEST_LOCK") {
            assert!(CoreExecutionGuard::acquire_at(Path::new(&path)).is_err());
        }
        Ok(())
    }

    #[test]
    fn reservation_excludes_other_processes_until_drop() -> Result<()> {
        let root = std::env::temp_dir().join(format!("execution-lock-test-{}", std::process::id()));
        let first = CoreExecutionGuard::acquire_at(&root)?;
        assert!(CoreExecutionGuard::acquire_at(&root).is_err());
        let child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "execution::tests::child_lock_probe"])
            .env("CLASH_VERGE_TEST_LOCK", &root)
            .status()?;
        assert!(child.success());
        drop(first);
        drop(CoreExecutionGuard::acquire_at(&root)?);
        std::fs::remove_file(root)?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reservation_survives_stop_request_until_process_exit() -> Result<()> {
        let root = std::env::temp_dir().join(format!("execution-exit-test-{}", std::process::id()));
        let mut child = std::process::Command::new("sleep").arg("30").spawn()?;
        let guard = CoreExecutionGuard::acquire_at(&root)?;
        let (_sender, receiver) = tokio::sync::oneshot::channel();
        let waiter = guard.release_after_exit(child.id(), receiver);
        assert!(CoreExecutionGuard::acquire_at(&root).is_err());
        child.kill()?;
        child.wait()?;
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await??;
        drop(CoreExecutionGuard::acquire_at(&root)?);
        std::fs::remove_file(root)?;
        Ok(())
    }
}
