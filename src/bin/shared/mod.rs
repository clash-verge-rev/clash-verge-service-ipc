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
    run_command(
        "launchctl",
        &["disable", "system/io.github.clashverge.helper"],
        false,
    )?;

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
