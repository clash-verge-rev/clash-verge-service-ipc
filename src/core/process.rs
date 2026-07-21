use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ProcessIdentity {
    pub(super) executable: String,
    pub(super) started_at: u64,
}

pub(super) fn process_identity(pid: u32) -> Result<Option<ProcessIdentity>> {
    if !is_process_alive(pid) {
        return Ok(None);
    }

    #[cfg(target_os = "linux")]
    {
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))?
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields = stat
            .rsplit_once(')')
            .ok_or_else(|| anyhow::anyhow!("invalid /proc stat for process {pid}"))?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        let started_at = fields
            .get(19)
            .ok_or_else(|| anyhow::anyhow!("missing start time for process {pid}"))?
            .parse()?;
        Ok(Some(ProcessIdentity {
            executable,
            started_at,
        }))
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStringExt as _;

        let mut path = vec![0u8; platform_lib::PROC_PIDPATHINFO_MAXSIZE as usize];
        let path_len = unsafe {
            platform_lib::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32)
        };
        if path_len <= 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        path.truncate(path_len as usize);
        let executable = std::path::PathBuf::from(std::ffi::OsString::from_vec(path))
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let mut info = unsafe { std::mem::zeroed::<platform_lib::proc_bsdinfo>() };
        let info_len = unsafe {
            platform_lib::proc_pidinfo(
                pid as i32,
                platform_lib::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut platform_lib::proc_bsdinfo).cast(),
                std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32,
            )
        };
        if info_len != std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Some(ProcessIdentity {
            executable,
            started_at: info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
        }))
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for ProcessHandle {
            fn drop(&mut self) {
                unsafe { CloseHandle(self.0) };
            }
        }
        let handle = ProcessHandle(handle);
        let mut path = vec![0u16; 32_768];
        let mut path_len = path.len() as u32;
        if unsafe { QueryFullProcessImageNameW(handle.0, 0, path.as_mut_ptr(), &mut path_len) } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        path.truncate(path_len as usize);
        let executable = std::path::PathBuf::from(String::from_utf16(&path)?)
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) }
            == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let started_at =
            (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        Ok(Some(ProcessIdentity {
            executable,
            started_at,
        }))
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-o", "comm=", "-p", &pid.to_string()])
            .output()?;
        if !output.status.success() {
            return Ok(None);
        }
        let identity = String::from_utf8(output.stdout)?;
        Ok(Some(ProcessIdentity {
            executable: identity.trim().to_string(),
            started_at: 0,
        }))
    }
}

pub(super) fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { platform_lib::kill(pid as i32, 0) };
        let exists = result == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::EPERM);
        if !exists {
            return false;
        }
        // A zombie has exited and no longer owns files or locks, even though kill(pid, 0)
        // continues to report it until its parent reaps it.
        let zombie = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim_start()
                    .starts_with('Z')
            });
        !zombie
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

pub(super) async fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        warn!("Terminating process {}", pid);
        if unsafe { platform_lib::kill(pid as i32, platform_lib::SIGTERM) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(platform_lib::ESRCH)
        {
            return Err(std::io::Error::last_os_error().into());
        }

        for _ in 0..10 {
            if !is_process_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        warn!("Process {} did not exit, sending SIGKILL", pid);
        if unsafe { platform_lib::kill(pid as i32, platform_lib::SIGKILL) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(platform_lib::ESRCH)
        {
            return Err(std::io::Error::last_os_error().into());
        }
        for _ in 0..10 {
            if !is_process_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("process {pid} is still alive after SIGKILL");
    }

    #[cfg(windows)]
    {
        warn!("Terminating process {}", pid);
        let pid_arg = pid.to_string();
        let status = std::process::Command::new("taskkill")
            .args(["/PID", pid_arg.as_str(), "/T", "/F"])
            .status()?;
        if !status.success() && is_process_alive(pid) {
            bail!("taskkill failed for process {pid} with status {status}");
        }
        for _ in 0..20 {
            if !is_process_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        bail!("process {pid} is still alive after taskkill");
    }
}
