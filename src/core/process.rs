#[cfg(unix)]
use std::time::Duration;
use tracing::warn;

pub(super) fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { platform_lib::kill(pid as i32, 0) };
        if result == 0 {
            return true;
        }

        std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::EPERM)
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

pub(super) async fn terminate_process(pid: u32) {
    #[cfg(unix)]
    {
        warn!("Terminating process {}", pid);
        unsafe {
            platform_lib::kill(pid as i32, platform_lib::SIGTERM);
        }

        for _ in 0..10 {
            if !is_process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        warn!("Process {} did not exit, sending SIGKILL", pid);
        unsafe {
            platform_lib::kill(pid as i32, platform_lib::SIGKILL);
        }
    }

    #[cfg(windows)]
    {
        warn!("Terminating process {}", pid);
        let pid_arg = pid.to_string();
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", pid_arg.as_str(), "/T", "/F"])
            .status();
    }
}
