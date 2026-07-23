#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

use anyhow::Error;
use anyhow::{Context as _, bail};
use sha2::{Digest as _, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Read as _;
#[cfg(unix)]
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn enter_repair_gate() -> Result<clash_verge_service_ipc::ServiceRepairGate, Error> {
    match clash_verge_service_ipc::acquire_service_repair_gate()? {
        Some(gate) => Ok(gate),
        None => {
            eprintln!("Service repair is already in progress");
            std::process::exit(clash_verge_service_ipc::REPAIR_IN_PROGRESS_EXIT_CODE);
        }
    }
}

fn bundled_service_binary() -> Result<PathBuf, Error> {
    let source = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "clash-verge-service.exe"
    } else {
        "clash-verge-service"
    });
    let metadata = std::fs::symlink_metadata(&source)
        .with_context(|| format!("failed to inspect bundled service binary {source:?}"))?;
    if !metadata.file_type().is_file() {
        bail!("bundled service binary is not an ordinary file: {source:?}");
    }
    Ok(source)
}

fn sha256(path: &Path) -> Result<[u8; 32], Error> {
    let mut file = File::open(path).with_context(|| format!("failed to open {path:?}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {path:?}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn remove_ordinary_file_if_exists(path: &Path) -> Result<(), Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {path:?}"));
        }
    };
    if !metadata.file_type().is_file() {
        bail!("refusing to replace non-file service entry {path:?}");
    }
    std::fs::remove_file(path).with_context(|| format!("failed to remove {path:?}"))
}

fn stage_service_binary(source: &Path, target: &Path) -> Result<PathBuf, Error> {
    let parent = target
        .parent()
        .context("protected service target has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create protected service directory {parent:?}"))?;
    let staged = target.with_extension(if cfg!(windows) { "exe.next" } else { "next" });
    remove_ordinary_file_if_exists(&staged)?;

    let mut source_file = File::open(source)
        .with_context(|| format!("failed to open service candidate {source:?}"))?;
    let mut staged_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .with_context(|| format!("failed to create staged service binary {staged:?}"))?;
    std::io::copy(&mut source_file, &mut staged_file)
        .with_context(|| format!("failed to stage service binary at {staged:?}"))?;
    staged_file
        .sync_all()
        .with_context(|| format!("failed to sync staged service binary {staged:?}"))?;
    drop(staged_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o550))
            .with_context(|| format!("failed to secure staged service binary {staged:?}"))?;
    }

    if sha256(source)? != sha256(&staged)? {
        let _ = std::fs::remove_file(&staged);
        bail!("staged service binary hash does not match its bundled source");
    }
    Ok(staged)
}

fn publish_staged_binary(staged: &Path, target: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!("refusing to replace non-file service entry {target:?}");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {target:?}"));
        }
    }

    #[cfg(unix)]
    {
        std::fs::rename(staged, target)
            .with_context(|| format!("failed to publish service binary {staged:?} at {target:?}"))
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        let wide = |path: &Path| {
            let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
            value.push(0);
            value
        };
        let staged_wide = wide(staged);
        let target_wide = wide(target);
        if unsafe {
            MoveFileExW(
                staged_wide.as_ptr(),
                target_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("failed to publish service binary {staged:?} at {target:?}")
            });
        }
        Ok(())
    }
}

fn wait_for_service_ready() -> Result<(), Error> {
    const READY_TIMEOUT: Duration = Duration::from_secs(20);
    const READY_INTERVAL: Duration = Duration::from_millis(250);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create service readiness runtime")?;
    runtime.block_on(async {
        clash_verge_service_ipc::set_config(Some(clash_verge_service_ipc::IpcConfig {
            default_timeout: Duration::from_millis(250),
            max_retries: 1,
            retry_delay: Duration::from_millis(25),
        }))
        .await;

        let deadline = Instant::now() + READY_TIMEOUT;
        let result = loop {
            if let Ok(response) = clash_verge_service_ipc::get_version().await
                && response.code == 0
                && response.data.is_some_and(|info| {
                    info.supports_client(
                        clash_verge_service_ipc::ProtocolVersion::current(),
                        clash_verge_service_ipc::MIN_REQUIRED_SERVICE_REVISION,
                    )
                })
            {
                break Ok(());
            }
            if Instant::now() >= deadline {
                break Err(anyhow::anyhow!(
                    "service IPC did not become protocol-ready within {READY_TIMEOUT:?}"
                ));
            }
            tokio::time::sleep(READY_INTERVAL).await;
        };

        clash_verge_service_ipc::set_config(None).await;
        result
    })
}

#[cfg(any(target_os = "macos", test))]
fn launchd_service_target() -> String {
    format!("system/{}", clash_verge_service_ipc::MACOS_SERVICE_ID)
}

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
        println!("Executing: launchctl print {}", launchd_service_target());
    }

    let output = std::process::Command::new("launchctl")
        .args(["print", &launchd_service_target()])
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
fn resolve_service_group_name() -> Result<String, Error> {
    use nix::unistd::{Gid, Group, Uid, User};

    if let Some(gid) = env_u32("CLASH_VERGE_SERVICE_GID")
        && let Ok(Some(group)) = Group::from_gid(Gid::from_raw(gid))
    {
        return Ok(group.name);
    }

    if let Some(uid) = env_u32("SUDO_UID").or_else(|| env_u32("PKEXEC_UID"))
        && let Ok(Some(user)) = User::from_uid(Uid::from_raw(uid))
        && let Ok(Some(group)) = Group::from_gid(user.gid)
    {
        return Ok(group.name);
    }

    if let Some(gid) = env_u32("SUDO_GID")
        && let Ok(Some(group)) = Group::from_gid(Gid::from_raw(gid))
    {
        return Ok(group.name);
    }

    bail!("unable to resolve the invoking user's service group; use sudo or pkexec")
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Error> {
    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let debug = std::env::args().any(|arg| arg == "--debug");
    let launchd_install_plan = probe_launchd_service(debug)?;
    let service_binary_path = bundled_service_binary()?;

    // 定义 bundle 路径
    let bundle_path = PathBuf::from("/Library/PrivilegedHelperTools").join(format!(
        "{}.bundle",
        clash_verge_service_ipc::MACOS_SERVICE_ID
    ));
    let contents_path = bundle_path.join("Contents");
    let macos_path = contents_path.join("MacOS");

    // 创建 bundle 目录结构
    std::fs::create_dir_all(&macos_path)
        .map_err(|e| anyhow::anyhow!("Failed to create bundle directories: {}", e))?;

    // 复制二进制文件到 bundle 的 MacOS 目录
    let target_binary_path = macos_path.join("clash-verge-service");
    let staged = stage_service_binary(&service_binary_path, &target_binary_path)?;

    // 创建并写入 Info.plist
    let info_plist_path = contents_path.join("Info.plist");

    // 创建 LaunchDaemons 目录（如果不存在）
    let plist_dir = PathBuf::from("/Library/LaunchDaemons");
    if !plist_dir.exists() {
        std::fs::create_dir(&plist_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create plist directory: {}", e))?;
    }

    // 创建并写入 launchd plist
    let plist_file = plist_dir.join(format!(
        "{}.plist",
        clash_verge_service_ipc::MACOS_SERVICE_ID
    ));

    let launchd_plist_content = format!(
        include_str!("../../resources/launchd.plist.tmpl"),
        group_name = resolve_service_group_name()?,
        service_id = clash_verge_service_ipc::MACOS_SERVICE_ID,
        app_bundle_id = clash_verge_service_ipc::MACOS_APP_BUNDLE_ID,
        service_binary = target_binary_path.to_string_lossy(),
    );
    let info_plist_content = format!(
        include_str!("../../resources/info.plist.tmpl"),
        display_name = clash_verge_service_ipc::SERVICE_DISPLAY_NAME,
        service_id = clash_verge_service_ipc::MACOS_SERVICE_ID,
    );
    let plist_path = plist_file.to_string_lossy().into_owned();
    let target_path = target_binary_path.to_string_lossy().into_owned();
    let bundle_path_string = bundle_path.to_string_lossy().into_owned();

    if launchd_install_plan == LaunchdInstallPlan::Bootout {
        run_command("launchctl", &["bootout", "system", &plist_path], debug)?;
    }
    publish_staged_binary(&staged, &target_binary_path)?;
    std::fs::write(&info_plist_path, info_plist_content)
        .with_context(|| format!("failed to write Info.plist {info_plist_path:?}"))?;
    File::create(&plist_file)
        .and_then(|mut file| file.write_all(launchd_plist_content.as_bytes()))
        .map_err(|e| anyhow::anyhow!("Failed to write plist file: {}", e))?;

    // 设置权限
    // 设置 LaunchDaemons plist 权限
    run_command("chmod", &["644", &plist_path], debug)?;
    run_command("chown", &["root:wheel", &plist_path], debug)?;

    // 设置二进制文件权限
    run_command("chmod", &["544", &target_path], debug)?;
    run_command("chown", &["root:wheel", &target_path], debug)?;

    // 设置 bundle 目录及其内容的权限
    run_command("chmod", &["755", &bundle_path_string], debug)?;
    run_command("chown", &["-R", "root:wheel", &bundle_path_string], debug)?;

    // 加载和启动服务
    let launchd_target = launchd_service_target();
    run_command("launchctl", &["enable", &launchd_target], debug)?;
    run_command("launchctl", &["bootstrap", "system", &plist_path], debug)?;
    run_command(
        "launchctl",
        &["start", clash_verge_service_ipc::MACOS_SERVICE_ID],
        debug,
    )?;
    wait_for_service_ready()?;
    #[cfg(not(feature = "development-channel"))]
    let _ = uninstall_old_service();

    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Error> {
    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let debug = std::env::args().any(|arg| arg == "--debug");
    let source = bundled_service_binary()?;
    let install_dir = clash_verge_service_ipc::prepare_service_install_directory()?;
    let target = install_dir.join("clash-verge-service");
    let staged = stage_service_binary(&source, &target)?;
    let unit_name = format!("{}.service", clash_verge_service_ipc::SERVICE_SLUG);
    let unit_path = PathBuf::from("/etc/systemd/system").join(&unit_name);

    let _ = run_command("systemctl", &["stop", &unit_name], debug);
    publish_staged_binary(&staged, &target)?;

    let unit_file_content = format!(
        include_str!("../../resources/systemd_service_unit.tmpl"),
        exec_start = target.to_string_lossy(),
        group = resolve_service_group_name()?,
        runtime_directory = clash_verge_service_ipc::SERVICE_SLUG,
    );

    let mut unit_file = File::create(&unit_path)
        .with_context(|| format!("failed to create systemd unit {unit_path:?}"))?;
    unit_file
        .write_all(unit_file_content.as_bytes())
        .with_context(|| format!("failed to write systemd unit {unit_path:?}"))?;
    unit_file
        .sync_all()
        .with_context(|| format!("failed to sync systemd unit {unit_path:?}"))?;

    run_command("systemctl", &["daemon-reload"], debug)?;
    run_command("systemctl", &["enable", &unit_name], debug)?;
    run_command("systemctl", &["start", &unit_name], debug)?;
    wait_for_service_ready()?;

    Ok(())
}

/// install and start the service
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use platform_lib::{
        Error as WindowsServiceError,
        service::{
            ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
            ServiceType,
        },
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use std::ffi::{OsStr, OsString};
    use std::{thread, time::Duration};

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let source = bundled_service_binary()?;
    let install_dir = clash_verge_service_ipc::prepare_service_install_directory()?;
    let target = install_dir.join("clash-verge-service.exe");
    let staged = stage_service_binary(&source, &target)?;

    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;
    let start_type = if cfg!(feature = "development-channel") {
        ServiceStartType::OnDemand
    } else {
        ServiceStartType::AutoStart
    };
    let service_info = ServiceInfo {
        name: OsString::from(clash_verge_service_ipc::WINDOWS_SERVICE_NAME),
        display_name: OsString::from(clash_verge_service_ipc::SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type,
        error_control: ServiceErrorControl::Normal,
        executable_path: target.clone(),
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    let service_access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::CHANGE_CONFIG;
    match service_manager.open_service(
        clash_verge_service_ipc::WINDOWS_SERVICE_NAME,
        service_access,
    ) {
        Ok(service) => {
            const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
            let status = service.query_status()?;
            if status.current_state != ServiceState::Stopped {
                if let Err(error) = service.stop()
                    && !matches!(
                        &error,
                        WindowsServiceError::Winapi(error)
                            if error.raw_os_error() == Some(ERROR_SERVICE_NOT_ACTIVE)
                    )
                {
                    return Err(error.into());
                }
                for _ in 0..200 {
                    if service.query_status()?.current_state == ServiceState::Stopped {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if service.query_status()?.current_state != ServiceState::Stopped {
                    bail!("timed out waiting for service to stop before replacement");
                }
            }

            publish_staged_binary(&staged, &target)?;
            service.change_config(&service_info)?;
            configure_windows_service_recovery(&service)?;
            service.start(&Vec::<&OsStr>::new())?;
            wait_for_service_ready()?;
            return Ok(());
        }
        Err(WindowsServiceError::Winapi(error)) if error.raw_os_error() == Some(1060) => {}
        Err(error) => return Err(error.into()),
    }

    publish_staged_binary(&staged, &target)?;
    let start_access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
    let service = service_manager.create_service(&service_info, start_access)?;

    service.set_description("Clash Verge Service helps to launch Clash Core")?;
    configure_windows_service_recovery(&service)?;
    service.start(&Vec::<&OsStr>::new())?;
    wait_for_service_ready()?;

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
