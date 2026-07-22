#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

use anyhow::Error;

#[cfg(any(target_os = "macos", test))]
const LAUNCHD_SERVICE_TARGET: &str = "system/io.github.clash-verge-rev.clash-verge-rev.service";

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum LaunchdInstallPlan {
    SkipBootout,
    Bootout,
}

#[cfg(any(target_os = "macos", test))]
fn classify_launchd_service_probe(
    exit_code: Option<i32>,
    diagnostic: &str,
) -> Result<LaunchdInstallPlan, Error> {
    match exit_code {
        Some(0) => Ok(LaunchdInstallPlan::Bootout),
        Some(113) if diagnostic.contains("Could not find service") => {
            Ok(LaunchdInstallPlan::SkipBootout)
        }
        _ => Err(anyhow::anyhow!(
            "Unexpected launchctl service probe result (exit code: {:?}): {}",
            exit_code,
            diagnostic
        )),
    }
}

#[cfg(target_os = "macos")]
fn probe_launchd_service(debug: bool) -> Result<LaunchdInstallPlan, Error> {
    if debug {
        println!("Executing: launchctl print {}", LAUNCHD_SERVICE_TARGET);
    }

    let output = std::process::Command::new("launchctl")
        .args(["print", LAUNCHD_SERVICE_TARGET])
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to probe launchd service: {}", e))?;
    let diagnostic = format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    classify_launchd_service_probe(output.status.code(), &diagnostic)
}

fn run_maintenance_if_requested() -> Result<bool, Error> {
    if !std::env::args().any(|argument| argument == "--cleanup-stale-owners") {
        return Ok(false);
    }
    let removed = clash_verge_service_ipc::cleanup_stale_owner_state()?;
    println!("Removed {} stale owner state directories", removed.len());
    Ok(true)
}

#[cfg(unix)]
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.parse().ok()
}

#[cfg(unix)]
fn resolve_service_group_name() -> String {
    use nix::unistd::{Gid, Group, Uid, User};

    if let Some(gid) = env_u32("CLASH_VERGE_SERVICE_GID")
        && let Ok(Some(group)) = Group::from_gid(Gid::from_raw(gid))
    {
        return group.name;
    }

    if let Some(uid) = env_u32("SUDO_UID").or_else(|| env_u32("PKEXEC_UID"))
        && let Ok(Some(user)) = User::from_uid(Uid::from_raw(uid))
        && let Ok(Some(group)) = Group::from_gid(user.gid)
    {
        return group.name;
    }

    if let Some(gid) = env_u32("SUDO_GID")
        && let Ok(Some(group)) = Group::from_gid(Gid::from_raw(gid))
    {
        return group.name;
    }

    panic!("Please use sudo or pkexec to install service.");
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Error> {
    use std::env;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let debug = env::args().any(|arg| arg == "--debug");
    let launchd_install_plan = probe_launchd_service(debug)?;
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

    let launchd_plist_content = format!(
        include_str!("../../resources/launchd.plist.tmpl"),
        group_name = resolve_service_group_name()
    );

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
    match launchd_install_plan {
        LaunchdInstallPlan::SkipBootout => {
            if debug {
                println!("No loaded Clash Verge Service exists; skipping launchctl bootout.");
            }
        }
        LaunchdInstallPlan::Bootout => run_command(
            "launchctl",
            &["bootout", "system", plist_file.to_str().unwrap()],
            debug,
        )?,
    }
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

    if run_maintenance_if_requested()? {
        return Ok(());
    }
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
        exec_start = service_binary_path.to_str().unwrap(),
        group = resolve_service_group_name()
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
    use std::env;
    use std::ffi::{OsStr, OsString};

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    const SERVICE_NAME: &str = "clash_verge_service";

    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let service_access =
        ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG;
    if let Ok(service) = service_manager.open_service(SERVICE_NAME, service_access) {
        configure_windows_service_recovery(&service)?;
        let status = service.query_status()?;
        match status.current_state {
            ServiceState::StopPending
            | ServiceState::Stopped
            | ServiceState::PausePending
            | ServiceState::Paused => {
                service.start(&Vec::<&OsStr>::new())?;
            }
            _ => {}
        };

        return Ok(());
    }

    let service_binary_path = env::current_exe()
        .unwrap()
        .with_file_name("clash-verge-service.exe");

    if !service_binary_path.exists() {
        eprintln!("clash-verge-service.exe not found");
        std::process::exit(2);
    }

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("Clash Verge Service"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: service_binary_path,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None, // run as System
        account_password: None,
    };

    let start_access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
    let service = service_manager.create_service(&service_info, start_access)?;

    service.set_description("Clash Verge Service helps to launch clash core")?;
    configure_windows_service_recovery(&service)?;
    service.start(&Vec::<&OsStr>::new())?;

    Ok(())
}

#[cfg(windows)]
fn configure_windows_service_recovery(
    service: &platform_lib::service::Service,
) -> platform_lib::Result<()> {
    use platform_lib::service::{
        ServiceAction, ServiceActionType, ServiceFailureActions, ServiceFailureResetPeriod,
    };
    use std::time::Duration;

    let actions = [5, 10, 30]
        .into_iter()
        .map(|delay_secs| ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(delay_secs),
        })
        .collect();

    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(actions),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;

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
    use super::*;

    #[test]
    fn missing_launchd_service_skips_bootout() {
        let plan = classify_launchd_service_probe(
            Some(113),
            "Could not find service \"io.github.clash-verge-rev.clash-verge-rev.service\" in domain for system",
        )
        .unwrap();

        assert_eq!(plan, LaunchdInstallPlan::SkipBootout);
    }

    #[test]
    fn loaded_launchd_service_runs_bootout() {
        let plan = classify_launchd_service_probe(Some(0), "").unwrap();

        assert_eq!(plan, LaunchdInstallPlan::Bootout);
    }

    #[test]
    fn unexpected_launchd_exit_is_an_error() {
        let result = classify_launchd_service_probe(Some(5), "Could not find service");

        assert!(result.is_err());
    }

    #[test]
    fn unexpected_launchd_diagnostic_is_an_error() {
        let result = classify_launchd_service_probe(Some(113), "Operation not permitted");

        assert!(result.is_err());
    }
}
