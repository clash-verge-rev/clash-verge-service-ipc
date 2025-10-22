#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

use anyhow::Error;

#[cfg(target_os = "macos")]
fn main() -> Result<(), Error> {
    use std::env;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    let debug = env::args().any(|arg| arg == "--debug");
    let _ = uninstall_old_service();

    let service_binary_path = env::current_exe()
        .unwrap()
        .with_file_name("clash-verge-service");

    if !service_binary_path.exists() {
        return Err(anyhow::anyhow!("clash-verge-service binary not found"));
    }

    // 定义 bundle 路径
    let bundle_path =
        "/Library/PrivilegedHelperTools/io.github.clash-verge-rev.clash-verge-rev.service.bundle";
    let contents_path = format!("{}/Contents", bundle_path);
    let macos_path = format!("{}/MacOS", contents_path);

    // 创建 bundle 目录结构
    std::fs::create_dir_all(&macos_path)
        .map_err(|e| anyhow::anyhow!("Failed to create bundle directories: {}", e))?;

    // 复制二进制文件到 bundle 的 MacOS 目录
    let target_binary_path = format!("{}/clash-verge-service", macos_path);
    std::fs::copy(&service_binary_path, &target_binary_path)
        .map_err(|e| anyhow::anyhow!("Failed to copy service file: {}", e))?;

    // 创建并写入 Info.plist
    let info_plist_path = format!("{}/Info.plist", contents_path);
    let info_plist_content = include_str!("../../resources/info.plist.tmpl");

    std::fs::write(&info_plist_path, info_plist_content)
        .map_err(|e| anyhow::anyhow!("Failed to write Info.plist: {}", e))?;

    // 创建 LaunchDaemons 目录（如果不存在）
    let plist_dir = Path::new("/Library/LaunchDaemons");
    if !plist_dir.exists() {
        std::fs::create_dir(plist_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create plist directory: {}", e))?;
    }

    // 创建并写入 launchd plist
    let plist_file =
        "/Library/LaunchDaemons/io.github.clash-verge-rev.clash-verge-rev.service.plist";
    let plist_file = Path::new(plist_file);

    let launchd_plist_content = include_str!("../../resources/launchd.plist.tmpl");

    File::create(plist_file)
        .and_then(|mut file| file.write_all(launchd_plist_content.as_bytes()))
        .map_err(|e| anyhow::anyhow!("Failed to write plist file: {}", e))?;

    // 设置权限
    // 设置 LaunchDaemons plist 权限
    let _ = run_command("chmod", &["644", plist_file.to_str().unwrap()], debug);
    let _ = run_command(
        "chown",
        &["root:wheel", plist_file.to_str().unwrap()],
        debug,
    );

    // 设置二进制文件权限
    let _ = run_command("chmod", &["544", &target_binary_path], debug);
    let _ = run_command("chown", &["root:wheel", &target_binary_path], debug);

    // 设置 bundle 目录及其内容的权限
    let _ = run_command("chmod", &["755", bundle_path], debug);
    let _ = run_command("chown", &["-R", "root:wheel", bundle_path], debug);

    // 加载和启动服务
    let _ = run_command(
        "launchctl",
        &[
            "enable",
            "system/io.github.clash-verge-rev.clash-verge-rev.service",
        ],
        debug,
    );
    let _ = run_command(
        "launchctl",
        &["bootout", "system", plist_file.to_str().unwrap()],
        debug,
    );
    let _ = run_command(
        "launchctl",
        &["bootstrap", "system", plist_file.to_str().unwrap()],
        debug,
    );
    let _ = run_command(
        "launchctl",
        &["start", "io.github.clash-verge-rev.clash-verge-rev.service"],
        debug,
    );

    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Error> {
    const SERVICE_NAME: &str = "clash-verge-service";
    use std::env;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    let debug = env::args().any(|arg| arg == "--debug");

    let service_binary_path = env::current_exe()
        .unwrap()
        .with_file_name("clash-verge-service");

    if !service_binary_path.exists() {
        return Err(anyhow::anyhow!("clash-verge-service binary not found"));
    }

    // Check service status
    let status_output = std::process::Command::new("systemctl")
        .args(["status", &format!("{}.service", SERVICE_NAME), "--no-pager"])
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to check service status: {}", e))?;

    match status_output.status.code() {
        Some(0) => return Ok(()), // Service is running
        Some(1) | Some(2) | Some(3) => {
            run_command(
                "systemctl",
                &["start", &format!("{}.service", SERVICE_NAME)],
                debug,
            )?;
            return Ok(());
        }
        Some(4) => {} // Service not found, continue with installation
        _ => return Err(anyhow::anyhow!("Unexpected systemctl status code")),
    }

    // Create and write unit file
    let unit_file = format!("/etc/systemd/system/{}.service", SERVICE_NAME);
    let unit_file = Path::new(&unit_file);

    let unit_file_content = format!(
        include_str!("../../resources/systemd_service_unit.tmpl"),
        service_binary_path.to_str().unwrap()
    );

    File::create(unit_file)
        .and_then(|mut file| file.write_all(unit_file_content.as_bytes()))
        .map_err(|e| anyhow::anyhow!("Failed to write unit file: {}", e))?;

    // Reload and start service
    let _ = run_command("systemctl", &["daemon-reload"], debug);
    let _ = run_command("systemctl", &["enable", SERVICE_NAME, "--now"], debug);

    Ok(())
}

/// install and start the service
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use platform_lib::{
        service::{
            ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
            ServiceType,
        },
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use std::{
        env,
        ffi::{OsStr, OsString},
        thread,
        time::Duration,
    };

    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::START;
    if let Ok(service) = service_manager.open_service("clash_verge_service", service_access)
        && let Ok(status) = service.query_status()
    {
        match status.current_state {
            ServiceState::StopPending
            | ServiceState::Stopped
            | ServiceState::PausePending
            | ServiceState::Paused => {
                service.start(&Vec::<&OsStr>::new())?;
            }
            _ => {}
        }

        return Ok(());
    }

    let service_binary_path = env::current_exe()?.with_file_name("clash-verge-service.exe");
    if !service_binary_path.exists() {
        eprintln!("clash-verge-service.exe not found");
        std::process::exit(2);
    }

    let service_info = ServiceInfo {
        name: OsString::from("clash_verge_service"),
        display_name: OsString::from("Clash Verge Service"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: service_binary_path,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    let start_access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
    let service = service_manager.create_service(&service_info, start_access)?;
    service.set_description("Clash Verge Service helps to launch clash core")?;
    drop(service);

    let service = service_manager.open_service("clash_verge_service", start_access)?;

    let mut last_err = None;
    for _ in 0..3 {
        match service.start(&[] as &[&OsStr]) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    Err(last_err.unwrap().into())
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
