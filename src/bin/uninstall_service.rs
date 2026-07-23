#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

use anyhow::Error;

fn run_maintenance_if_requested() -> Result<bool, Error> {
    if !std::env::args().any(|argument| argument == "--cleanup-stale-owners") {
        return Ok(false);
    }
    let removed = clash_verge_service_ipc::cleanup_stale_owner_state()?;
    println!("Removed {} stale owner state directories", removed.len());
    Ok(true)
}

#[cfg(any(windows, test))]
fn poll_until<T>(
    max_attempts: usize,
    mut probe: impl FnMut() -> Result<Option<T>, Error>,
    mut pause: impl FnMut(),
    timeout_message: &str,
) -> Result<T, Error> {
    for attempt in 0..max_attempts {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        if attempt + 1 < max_attempts {
            pause();
        }
    }
    Err(anyhow::anyhow!("{timeout_message}"))
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Error> {
    use std::env;
    use std::path::Path;

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let debug = env::args().any(|arg| arg == "--debug");

    let _ = uninstall_old_service();
    // 定义路径
    let bundle_path =
        "/Library/PrivilegedHelperTools/io.github.clash-verge-rev.clash-verge-rev.service.bundle";
    let plist_file =
        "/Library/LaunchDaemons/io.github.clash-verge-rev.clash-verge-rev.service.plist";
    let service_id = "io.github.clash-verge-rev.clash-verge-rev.service";

    // 停止并卸载服务
    let _ = run_command("launchctl", &["stop", service_id], debug);
    let _ = run_command(
        "launchctl",
        &["disable", &format!("system/{}", service_id)],
        debug,
    );
    let _ = run_command("launchctl", &["bootout", "system", plist_file], debug);

    // 删除文件
    if Path::new(plist_file).exists() {
        std::fs::remove_file(plist_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove plist file: {}", e))?;
    }

    // 删除整个 bundle 目录
    if Path::new(bundle_path).exists() {
        std::fs::remove_dir_all(bundle_path)
            .map_err(|e| anyhow::anyhow!("Failed to remove bundle directory: {}", e))?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Error> {
    const SERVICE_NAME: &str = "clash-verge-service";
    use std::env;

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let debug = env::args().any(|arg| arg == "--debug");

    // Stop and disable service
    let _ = run_command(
        "systemctl",
        &["stop", &format!("{}.service", SERVICE_NAME)],
        debug,
    );
    let _ = run_command(
        "systemctl",
        &["disable", &format!("{}.service", SERVICE_NAME)],
        debug,
    );

    // Remove service file
    let unit_file = format!("/etc/systemd/system/{}.service", SERVICE_NAME);
    if std::path::Path::new(&unit_file).exists() {
        std::fs::remove_file(&unit_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove service file: {}", e))?;
    }

    // Reload systemd
    let _ = run_command("systemctl", &["daemon-reload"], debug);

    Ok(())
}

/// stop and uninstall the service
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use platform_lib::{
        Error as WindowsServiceError,
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use std::{thread, time::Duration};

    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
    const POLL_ATTEMPTS: usize = 200;
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    const SERVICE_NAME: &str = "clash_verge_service";

    fn has_raw_error(error: &WindowsServiceError, code: i32) -> bool {
        matches!(error, WindowsServiceError::Winapi(error) if error.raw_os_error() == Some(code))
    }

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = service_manager.open_service(SERVICE_NAME, service_access)?;

    let service_status = service.query_status()?;
    if service_status.current_state != ServiceState::Stopped {
        if let Err(error) = service.stop()
            && !has_raw_error(&error, ERROR_SERVICE_NOT_ACTIVE)
        {
            return Err(error.into());
        }
        poll_until(
            POLL_ATTEMPTS,
            || {
                let status = service.query_status()?;
                Ok((status.current_state == ServiceState::Stopped).then_some(()))
            },
            || thread::sleep(POLL_INTERVAL),
            "timed out waiting for service to stop",
        )?;
    }

    service.delete()?;
    drop(service);
    poll_until(
        POLL_ATTEMPTS,
        || match service_manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Ok(service) => {
                drop(service);
                Ok(None)
            }
            Err(error) if has_raw_error(&error, ERROR_SERVICE_DOES_NOT_EXIST) => Ok(Some(())),
            Err(error) => Err(error.into()),
        },
        || thread::sleep(POLL_INTERVAL),
        "timed out waiting for service deletion",
    )?;
    println!("Service uninstalled successfully. Resource cleanup warnings can be ignored.");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall_old_service() -> Result<(), Error> {
    use std::path::Path;

    let target_binary_path = "/Library/PrivilegedHelperTools/io.github.clashverge.helper";
    let plist_file = "/Library/LaunchDaemons/io.github.clashverge.helper.plist";

    // Stop and unload service
    run_command("launchctl", &["stop", "io.github.clashverge.helper"], false)?;
    run_command("launchctl", &["bootout", "system", plist_file], false)?;
    run_command(
        "launchctl",
        &["disable", "system/io.github.clashverge.helper"],
        false,
    )?;

    // Remove files
    if Path::new(plist_file).exists() {
        std::fs::remove_file(plist_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove plist file: {}", e))?;
    }

    if Path::new(target_binary_path).exists() {
        std::fs::remove_file(target_binary_path)
            .map_err(|e| anyhow::anyhow!("Failed to remove service binary: {}", e))?;
    }

    Ok(())
}

pub fn run_command(cmd: &str, args: &[&str], debug: bool) -> Result<(), Error> {
    if debug {
        println!("Executing: {} {}", cmd, args.join(" "));
    }

    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to execute '{}': {}", cmd, e))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if debug {
        eprintln!(
            "Command failed (status: {}):\nstdout: {}\nstderr: {}",
            output.status, stdout, stderr
        );
    }

    Err(anyhow::anyhow!(
        "Command '{}' failed (status: {}):\nstdout: {}\nstderr: {}",
        cmd,
        output.status,
        stdout,
        stderr
    ))
}

#[cfg(test)]
mod tests {
    use super::poll_until;
    use std::cell::Cell;

    #[test]
    fn poll_until_retries_transient_state_before_success() -> anyhow::Result<()> {
        let attempts = Cell::new(0);
        let pauses = Cell::new(0);

        let result = poll_until(
            3,
            || {
                let next = attempts.get() + 1;
                attempts.set(next);
                Ok((next == 3).then_some("deleted"))
            },
            || pauses.set(pauses.get() + 1),
            "service deletion timed out",
        )?;

        assert_eq!(result, "deleted");
        assert_eq!(attempts.get(), 3);
        assert_eq!(pauses.get(), 2);
        Ok(())
    }
}
