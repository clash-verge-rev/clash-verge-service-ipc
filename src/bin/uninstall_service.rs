#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

mod shared;

use anyhow::Error;
#[cfg(target_os = "linux")]
use shared::SystemdManager;
#[cfg(target_os = "macos")]
use shared::run_command;
#[cfg(all(target_os = "macos", not(feature = "development-channel")))]
use shared::uninstall_old_service;
use shared::{enter_repair_gate, run_maintenance_if_requested};

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
    let _gate = enter_repair_gate()?;
    let debug = env::args().any(|arg| arg == "--debug");

    #[cfg(not(feature = "development-channel"))]
    let _ = uninstall_old_service();
    let bundle_path = format!(
        "/Library/PrivilegedHelperTools/{}.bundle",
        clash_verge_service_ipc::MACOS_SERVICE_ID
    );
    let plist_file = format!(
        "/Library/LaunchDaemons/{}.plist",
        clash_verge_service_ipc::MACOS_SERVICE_ID
    );
    let service_id = clash_verge_service_ipc::MACOS_SERVICE_ID;

    let _ = run_command("launchctl", &["stop", service_id], debug);
    let _ = run_command(
        "launchctl",
        &["disable", &format!("system/{}", service_id)],
        debug,
    );
    let _ = run_command("launchctl", &["bootout", "system", &plist_file], debug);

    if Path::new(&plist_file).exists() {
        std::fs::remove_file(&plist_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove plist file: {}", e))?;
    }

    if Path::new(&bundle_path).exists() {
        std::fs::remove_dir_all(&bundle_path)
            .map_err(|e| anyhow::anyhow!("Failed to remove bundle directory: {}", e))?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Error> {
    use std::env;

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let debug = env::args().any(|arg| arg == "--debug");
    let service_name = clash_verge_service_ipc::SERVICE_SLUG;
    let unit_name = format!("{}.service", service_name);
    let systemd = SystemdManager::connect()?;

    if debug {
        println!("Connected to systemd via system bus");
        println!("Stopping and disabling systemd unit {unit_name}");
    }
    systemd.stop(&unit_name)?;
    systemd.disable(&unit_name)?;

    let unit_file = format!("/etc/systemd/system/{}.service", service_name);
    if std::path::Path::new(&unit_file).exists() {
        std::fs::remove_file(&unit_file)
            .map_err(|e| anyhow::anyhow!("Failed to remove service file: {}", e))?;
    }

    systemd.reload()?;
    let target =
        clash_verge_service_ipc::prepare_service_install_directory()?.join("clash-verge-service");
    if target.exists() {
        std::fs::remove_file(&target).map_err(|error| {
            anyhow::anyhow!("Failed to remove service binary {target:?}: {error}")
        })?;
    }

    Ok(())
}

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

    fn has_raw_error(error: &WindowsServiceError, code: i32) -> bool {
        matches!(error, WindowsServiceError::Winapi(error) if error.raw_os_error() == Some(code))
    }

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = service_manager.open_service(
        clash_verge_service_ipc::WINDOWS_SERVICE_NAME,
        service_access,
    )?;

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
        || match service_manager.open_service(
            clash_verge_service_ipc::WINDOWS_SERVICE_NAME,
            ServiceAccess::QUERY_STATUS,
        ) {
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
    let target = clash_verge_service_ipc::prepare_service_install_directory()?
        .join("clash-verge-service.exe");
    if target.exists() {
        std::fs::remove_file(&target).map_err(|error| {
            anyhow::anyhow!("Failed to remove service binary {target:?}: {error}")
        })?;
    }
    println!("Service uninstalled successfully. Resource cleanup warnings can be ignored.");
    Ok(())
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
