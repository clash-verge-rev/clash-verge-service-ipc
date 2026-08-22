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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "linux")]
pub struct SystemdManager {
    runtime: tokio::runtime::Runtime,
    connection: zbus::Connection,
}

#[cfg(target_os = "linux")]
// This module is compiled separately into the installer and uninstaller, so each
// binary intentionally leaves some operations unused.
#[allow(dead_code)]
impl SystemdManager {
    const MANAGER_PATH: &'static str = "/org/freedesktop/systemd1";
    const MANAGER_INTERFACE: &'static str = "org.freedesktop.systemd1.Manager";
    const UNIT_INTERFACE: &'static str = "org.freedesktop.systemd1.Unit";
    const PROPERTIES_INTERFACE: &'static str = "org.freedesktop.DBus.Properties";
    const SYSTEMD_DESTINATION: &'static str = "org.freedesktop.systemd1";
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const METHOD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
    const STATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    fn is_missing_unit(error: &zbus::Error) -> bool {
        matches!(
            error,
            zbus::Error::MethodError(name, _, _)
                if matches!(
                    name.as_str(),
                    "org.freedesktop.systemd1.NoSuchUnit"
                        | "org.freedesktop.systemd1.LoadFailed"
                )
        )
    }

    pub fn connect() -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let connection = runtime.block_on(async {
            tokio::time::timeout(Self::CONNECT_TIMEOUT, async {
                zbus::connection::Builder::system()?
                    .method_timeout(Self::METHOD_TIMEOUT)
                    .build()
                    .await
            })
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "system bus connection timed out after {:?}",
                    Self::CONNECT_TIMEOUT
                )
            })?
            .map_err(|error| {
                anyhow::anyhow!("failed to connect to systemd via system bus: {error}")
            })
        })?;
        Ok(Self {
            runtime,
            connection,
        })
    }

    pub fn stop(&self, unit: &str) -> Result<(), Error> {
        self.runtime.block_on(async {
            if let Err(error) = self
                .connection
                .call_method(
                    Some(Self::SYSTEMD_DESTINATION),
                    Self::MANAGER_PATH,
                    Some(Self::MANAGER_INTERFACE),
                    "StopUnit",
                    &(unit, "replace"),
                )
                .await
            {
                return if Self::is_missing_unit(&error) {
                    Ok(())
                } else {
                    Err(error.into())
                };
            }
            let reply = match self
                .connection
                .call_method(
                    Some(Self::SYSTEMD_DESTINATION),
                    Self::MANAGER_PATH,
                    Some(Self::MANAGER_INTERFACE),
                    "GetUnit",
                    &(unit,),
                )
                .await
            {
                Ok(reply) => reply,
                Err(error) if Self::is_missing_unit(&error) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            let unit_path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
            let deadline = std::time::Instant::now() + Self::STATE_TIMEOUT;
            loop {
                let reply = self
                    .connection
                    .call_method(
                        Some(Self::SYSTEMD_DESTINATION),
                        unit_path.as_str(),
                        Some(Self::PROPERTIES_INTERFACE),
                        "Get",
                        &(Self::UNIT_INTERFACE, "ActiveState"),
                    )
                    .await?;
                let state: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
                let state = String::try_from(state)?;
                if matches!(state.as_str(), "inactive" | "failed") {
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "systemd unit did not stop within {:?}",
                        Self::STATE_TIMEOUT
                    ));
                }
                tokio::time::sleep(Self::STATE_INTERVAL).await;
            }
        })
    }

    pub fn reload(&self) -> Result<(), Error> {
        self.runtime.block_on(async {
            self.connection
                .call_method(
                    Some(Self::SYSTEMD_DESTINATION),
                    Self::MANAGER_PATH,
                    Some(Self::MANAGER_INTERFACE),
                    "Reload",
                    &(),
                )
                .await?;
            Ok(())
        })
    }

    pub fn enable(&self, unit_path: &str) -> Result<(), Error> {
        self.runtime.block_on(async {
            let unit_paths: &[&str] = &[unit_path];
            self.connection
                .call_method(
                    Some(Self::SYSTEMD_DESTINATION),
                    Self::MANAGER_PATH,
                    Some(Self::MANAGER_INTERFACE),
                    "EnableUnitFiles",
                    &(unit_paths, false, false),
                )
                .await?;
            Ok(())
        })
    }

    pub fn disable(&self, unit: &str) -> Result<(), Error> {
        self.runtime.block_on(async {
            let units: &[&str] = &[unit];
            self.connection
                .call_method(
                    Some(Self::SYSTEMD_DESTINATION),
                    Self::MANAGER_PATH,
                    Some(Self::MANAGER_INTERFACE),
                    "DisableUnitFiles",
                    &(units, false),
                )
                .await?;
            Ok(())
        })
    }

    pub fn start(&self, unit: &str) -> Result<(), Error> {
        self.runtime.block_on(async {
            self.connection
                .call_method(
                    Some(Self::SYSTEMD_DESTINATION),
                    Self::MANAGER_PATH,
                    Some(Self::MANAGER_INTERFACE),
                    "StartUnit",
                    &(unit, "replace"),
                )
                .await?;
            Ok(())
        })
    }
}
