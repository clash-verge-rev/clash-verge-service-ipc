//! Privileged maintenance shared by the installer and uninstaller binaries.

use anyhow::Error;

pub(crate) fn enter_repair_gate() -> Result<clash_verge_service_ipc::ServiceRepairGate, Error> {
    match clash_verge_service_ipc::acquire_service_repair_gate()? {
        Some(gate) => Ok(gate),
        None => {
            eprintln!("Service repair is already in progress");
            std::process::exit(clash_verge_service_ipc::REPAIR_IN_PROGRESS_EXIT_CODE);
        }
    }
}

pub(crate) fn run_maintenance_if_requested() -> Result<bool, Error> {
    if !std::env::args().any(|argument| argument == "--cleanup-stale-owners") {
        return Ok(false);
    }
    let removed = clash_verge_service_ipc::cleanup_stale_owner_state()?;
    println!("Removed {} stale owner state directories", removed.len());
    Ok(true)
}

#[cfg(all(target_os = "macos", not(feature = "development-channel")))]
pub fn uninstall_old_service() -> Result<(), Error> {
    use std::path::Path;

    let target_binary_path = "/Library/PrivilegedHelperTools/io.github.clashverge.helper";
    let plist_file = "/Library/LaunchDaemons/io.github.clashverge.helper.plist";

    run_command("launchctl", &["stop", "io.github.clashverge.helper"], false)?;
    run_command("launchctl", &["bootout", "system", plist_file], false)?;
    run_command("launchctl", &["disable", "system/io.github.clashverge.helper"], false)?;

    if Path::new(plist_file).exists() {
        std::fs::remove_file(plist_file).map_err(|e| anyhow::anyhow!("Failed to remove plist file: {}", e))?;
    }

    if Path::new(target_binary_path).exists() {
        std::fs::remove_file(target_binary_path)
            .map_err(|e| anyhow::anyhow!("Failed to remove service binary: {}", e))?;
    }

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// Names the Windows Firewall rule that admits the approved copy of `core`.
#[cfg(windows)]
pub(crate) fn core_firewall_rule_name(core: &std::path::Path) -> Result<String, Error> {
    use anyhow::Context as _;

    let name = core.file_name().context("core path has no file name")?;
    Ok(format!(
        "{} core ({})",
        clash_verge_service_ipc::SERVICE_DISPLAY_NAME,
        name.to_string_lossy()
    ))
}

/// Runs `netsh advfirewall firewall` with `arguments`, failing on a non-zero exit.
///
/// netsh prints localized text only, so callers can react to the status alone.
#[cfg(windows)]
pub(crate) fn netsh_firewall(arguments: &[&str]) -> Result<(), Error> {
    use anyhow::Context as _;
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Resolved absolutely: this process is elevated and must not let PATH choose what it runs.
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let netsh = std::path::Path::new(&system_root).join("System32").join("netsh.exe");
    let output = std::process::Command::new(&netsh)
        .args(["advfirewall", "firewall"])
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("failed to run {}", netsh.display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "netsh advfirewall firewall {} failed (status: {}): {} {}",
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}
