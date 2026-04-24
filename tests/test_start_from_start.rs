mod common;

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use clash_verge_service_ipc::{
        ClashConfig, CoreConfig, connect, load_desired_state, persist_core_stopped, run_ipc_server,
        service_status_snapshot, start_clash, stop_ipc_server,
    };
    use serial_test::serial;
    use std::sync::OnceLock;
    use std::{env, path::PathBuf, process::Command};
    use tokio::task::JoinHandle;
    use tokio::time::{Duration, sleep};
    use tracing::info;

    use crate::common;

    static BIN_PATH: OnceLock<PathBuf> = OnceLock::new();

    fn bin_path() -> &'static PathBuf {
        BIN_PATH.get_or_init(|| {
            let mut p = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
            p.push("target/debug");
            let exe = format!("mock_binary{}", std::env::consts::EXE_SUFFIX);
            p.push(exe);
            p
        })
    }

    fn is_mock_binary_running() -> bool {
        let exe = format!("mock_binary{}", std::env::consts::EXE_SUFFIX);

        #[cfg(unix)]
        {
            if let Ok(out) = Command::new("pgrep").arg("-f").arg(&exe).output()
                && out.status.success()
                && !out.stdout.is_empty()
            {
                return true;
            }
            if let Ok(out) = Command::new("ps").arg("aux").output()
                && out.status.success()
            {
                return String::from_utf8_lossy(&out.stdout).contains(&exe);
            }
            false
        }

        #[cfg(windows)]
        {
            if let Ok(out) = Command::new("tasklist").output()
                && out.status.success()
            {
                return String::from_utf8_lossy(&out.stdout)
                    .to_lowercase()
                    .contains(&exe.to_lowercase());
            }
            false
        }
    }

    async fn step_ensure_mock_binary_exists_or_build() -> Result<()> {
        let bin_path = bin_path();

        if bin_path.exists() {
            info!("✅ Found mock binary at {:?}", bin_path);
            return Ok(());
        }

        info!("🛠 mock binary not found, building...");
        let status = Command::new("cargo")
            .arg("build")
            .arg("--features")
            .arg("test")
            .status()?;

        assert!(status.success(), "cargo build failed");
        assert!(bin_path.exists(), "binary not found after build");

        info!("✅ Built mock binary at {:?}", bin_path);
        Ok(())
    }

    async fn step_connect_ipc_when_server_not_running() {
        let _ = stop_ipc_server().await;
        assert!(
            connect().await.is_err(),
            "Connecting when server not running should fail"
        );
        info!("✅ IPC connect failed as expected (server not running)");
    }

    async fn step_start_ipc_server() -> JoinHandle<kode_bridge::Result<()>> {
        let _ = stop_ipc_server().await;
        let handle = run_ipc_server().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        assert!(
            connect().await.is_ok(),
            "Should connect after server starts"
        );
        info!("✅ IPC server started and connectable");

        handle
    }

    async fn step_connect_ipc_after_starting_server() {
        assert!(
            connect().await.is_ok(),
            "Should connect to IPC after server start"
        );
        info!("✅ IPC connection works after server start");
    }

    async fn step_start_mock_binary() {
        let clash_config = ClashConfig {
            core_config: CoreConfig {
                core_path: bin_path().to_string_lossy().to_string(),
                ..Default::default()
            },
            log_config: Default::default(),
        };
        let start_result = start_clash(&clash_config).await;
        assert!(
            start_result.is_ok(),
            "Starting clash with mock binary should return Ok"
        );
        let desired_state = load_desired_state().await.unwrap();
        assert!(
            desired_state.core_should_be_running,
            "Desired state should persist running core intent"
        );
        assert!(
            desired_state.last_clash_config.is_some(),
            "Desired state should persist last ClashConfig"
        );

        let status = service_status_snapshot().await.unwrap();
        assert!(
            status.core_pid.is_some(),
            "Status should include the running core PID"
        );
        assert!(
            status.desired_core_should_be_running,
            "Status should include desired running state"
        );
        info!("✅ mock binary started successfully");
    }

    #[tokio::test]
    #[serial]
    async fn test_full_ipc_flow() -> Result<()> {
        // 在测试最开始初始化 tracing（只会初始化一次）
        common::init_tracing_for_tests();

        info!("==== Step 1: Ensure mock binary ====");
        step_ensure_mock_binary_exists_or_build().await?;

        info!("==== Step 2: Connect when server not running ====");
        step_connect_ipc_when_server_not_running().await;

        info!("==== Step 3: Start IPC server ====");
        let server_handle = step_start_ipc_server().await;

        info!("==== Step 4: Connect after server start ====");
        step_connect_ipc_after_starting_server().await;

        info!("==== Step 5: Start mock binary 30 times ====");
        for i in 1..=30 {
            info!("-- Iteration {}/30: starting mock binary --", i);
            step_start_mock_binary().await;
            assert!(
                is_mock_binary_running(),
                "Mock binary should be running (iteration {})",
                i
            );
            info!("✅ mock binary running (iteration {})", i);
        }

        info!("🎉 All IPC flow steps passed!");
        persist_core_stopped().await.unwrap();
        stop_ipc_server().await.unwrap();
        let res = server_handle.await.unwrap();
        assert!(res.is_ok(), "server should exit cleanly");
        Ok(())
    }
}
