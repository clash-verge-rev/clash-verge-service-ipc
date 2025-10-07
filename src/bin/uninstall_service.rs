#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

use anyhow::Error;

#[cfg(target_os = "macos")]
fn main() -> Result<(), Error> {
    use std::env;
    use std::path::Path;

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
    use std::{thread, time::Duration};
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = service_manager.open_service("clash_verge_service", service_access)?;

    let service_status = service.query_status()?;
    if service_status.current_state != ServiceState::Stopped {
        if let Err(err) = service.stop() {
            eprintln!("{err}");
        }
        // Wait for service to stop
        thread::sleep(Duration::from_secs(1));
    }

    service.delete()?;
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
