#![cfg(feature = "standalone")]
#[cfg(test)]
mod tests {
    use anyhow::Result;
    use std::sync::OnceLock;
    use std::{env, path::PathBuf, process::Command};

    static BIN_PATH: OnceLock<PathBuf> = OnceLock::new();

    fn bin_path() -> &'static PathBuf {
        BIN_PATH.get_or_init(|| {
            let mut p = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
            p.push("target/debug");
            let exe = format!("clash-verge-service{}", std::env::consts::EXE_SUFFIX);
            p.push(exe);
            p
        })
    }

    fn step_ensure_service_exists_or_build() -> Result<()> {
        let path = bin_path();

        if path.exists() {
            tracing::info!("service binary present at {}", path.display());
            return Ok(());
        }

        tracing::info!("service binary not found, invoking cargo build...");
        let status = Command::new("cargo")
            .arg("build")
            .arg("--bin")
            .arg("clash-verge-service")
            .status()?;

        assert!(
            status.success(),
            "cargo build failed for clash-verge-service"
        );
        assert!(path.exists(), "binary still missing after build");

        tracing::info!("built service binary at {}", path.display());
        Ok(())
    }

    #[test]
    fn test_ensure_service_binary() -> Result<()> {
        step_ensure_service_exists_or_build()?;
        Ok(())
    }

    #[test]
    fn test_run_command() -> Result<()> {
        step_ensure_service_exists_or_build()?;

        let path = bin_path();

        let mut child = Command::new(path)
            .arg("run")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        std::thread::sleep(std::time::Duration::from_millis(300));

        assert!(child.try_wait()?.is_none(), "service exited prematurely");

        child.kill()?;
        let output = child.wait_with_output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        tracing::info!("service run output:\n{}", stdout);

        assert!(
            !output.status.success(),
            "process was expected to be killed and not exit successfully"
        );

        Ok(())
    }

    #[test]
    fn test_install_command() -> Result<()> {
        step_ensure_service_exists_or_build()?;

        let path = bin_path();

        let output = Command::new(path)
            .arg("install")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        tracing::info!("install command output:\n{}", stdout);

        assert!(
            output.status.success(),
            "expected `install` to exit successfully"
        );

        Ok(())
    }

    #[test]
    fn test_uninstall_command() -> Result<()> {
        step_ensure_service_exists_or_build()?;
        let path = bin_path();

        let install_out = Command::new(path)
            .arg("install")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;
        let install_stdout = String::from_utf8_lossy(&install_out.stdout);
        tracing::info!("install (pre-uninstall) output:\n{}", install_stdout);
        assert!(
            install_out.status.success(),
            "expected `install` to exit successfully before uninstall"
        );

        let output = Command::new(path)
            .arg("uninstall")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        tracing::info!("uninstall command output:\n{}", stdout);

        assert!(
            output.status.success(),
            "expected `uninstall` to exit successfully"
        );

        Ok(())
    }
}
