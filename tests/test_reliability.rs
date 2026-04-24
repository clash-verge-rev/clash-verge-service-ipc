#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

mod common;

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result};
    #[cfg(unix)]
    use clash_verge_service_ipc::acquire_service_owner;
    use clash_verge_service_ipc::{
        ClashConfig, CoreConfig, CoreWatchdogTestConfig, ServiceLifecycleState, connect,
        persist_core_stopped, reconcile_service_startup, run_ipc_server,
        run_ipc_supervisor_until_shutdown, service_lifecycle_state, service_paths,
        service_status_snapshot, set_core_watchdog_config_for_tests, start_clash, stop_ipc_server,
    };
    use serial_test::serial;
    use std::path::PathBuf;
    use std::process::{Child, Command, ExitStatus};
    use std::time::{Duration, Instant};
    use tokio::sync::oneshot;
    use tokio::time::sleep;

    use crate::common;

    fn test_bin_path(name: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("target");
        path.push("debug");
        path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        path
    }

    fn ensure_test_bin(name: &str) -> Result<PathBuf> {
        let path = test_bin_path(name);
        if path.exists() {
            return Ok(path);
        }

        let status = Command::new("cargo")
            .args(["build", "--features", "standalone,test", "--bin", name])
            .status()
            .with_context(|| format!("failed to build {name}"))?;
        anyhow::ensure!(status.success(), "cargo build failed for {name}");
        anyhow::ensure!(path.exists(), "missing built binary {:?}", path);
        Ok(path)
    }

    #[cfg(unix)]
    async fn wait_until<F>(label: &str, timeout: Duration, mut condition: F) -> Result<()>
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for {label}");
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_until_async<F, Fut>(
        label: &str,
        timeout: Duration,
        mut condition: F,
    ) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if condition().await {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for {label}");
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_ipc_ready() -> Result<()> {
        wait_until_async("IPC readiness", Duration::from_secs(5), || async {
            connect().await.is_ok()
        })
        .await
    }

    async fn wait_for_ipc_running() -> Result<()> {
        wait_until_async("IPC running state", Duration::from_secs(5), || async {
            connect().await.is_ok() && service_lifecycle_state() == ServiceLifecycleState::Running
        })
        .await
    }

    async fn wait_for_child_exit(
        child: &mut Child,
        timeout: Duration,
    ) -> Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    fn kill_child(child: &mut Child) {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }

    #[tokio::test]
    #[serial]
    async fn ipc_supervisor_rebuilds_listener_after_listener_exit() -> Result<()> {
        common::init_tracing_for_tests();
        let _ = stop_ipc_server().await;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let supervisor = tokio::spawn(run_ipc_supervisor_until_shutdown(async {
            let _ = shutdown_rx.await;
        }));

        wait_for_ipc_running().await?;

        stop_ipc_server().await?;
        wait_for_ipc_running().await?;

        let _ = shutdown_tx.send(());
        supervisor.await??;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn healthy_owner_causes_second_instance_to_exit_without_takeover() -> Result<()> {
        common::init_tracing_for_tests();
        let _ = stop_ipc_server().await;

        let owner_guard = acquire_service_owner()
            .await?
            .context("expected current process to acquire owner lock")?;
        let server_handle = run_ipc_server().await?;
        wait_for_ipc_ready().await?;

        let owner_helper = ensure_test_bin("owner_lock_holder")?;
        let mut probe = Command::new(owner_helper).spawn()?;
        let status = wait_for_child_exit(&mut probe, Duration::from_secs(5))
            .await?
            .context("owner probe did not exit")?;

        assert_eq!(
            status.code(),
            Some(2),
            "second owner should exit when healthy IPC owner is reachable"
        );
        assert!(
            connect().await.is_ok(),
            "healthy owner IPC should remain up"
        );

        stop_ipc_server().await?;
        server_handle.await??;
        drop(owner_guard);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn stale_owner_without_ipc_is_cleaned_and_reacquired() -> Result<()> {
        common::init_tracing_for_tests();
        let _ = stop_ipc_server().await;

        let owner_helper = ensure_test_bin("owner_lock_holder")?;
        let mut stale_owner = Command::new(owner_helper).spawn()?;
        let stale_pid = stale_owner.id();
        let paths = service_paths();

        wait_until("stale owner pid file", Duration::from_secs(5), || {
            std::fs::read_to_string(paths.pid_file_path())
                .ok()
                .and_then(|content| content.trim().parse::<u32>().ok())
                == Some(stale_pid)
        })
        .await?;

        let owner_guard = acquire_service_owner()
            .await?
            .context("expected stale owner to be cleaned and lock reacquired")?;

        let exited = wait_for_child_exit(&mut stale_owner, Duration::from_secs(3)).await?;
        assert!(exited.is_some(), "stale owner process should be terminated");
        assert_eq!(
            std::fs::read_to_string(paths.pid_file_path())?
                .trim()
                .parse::<u32>()?,
            std::process::id()
        );

        drop(owner_guard);
        kill_child(&mut stale_owner);
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn startup_reconcile_kills_live_core_with_unusable_socket() -> Result<()> {
        common::init_tracing_for_tests();
        let _ = stop_ipc_server().await;

        let mock_binary = ensure_test_bin("mock_binary")?;
        let mut old_core = Command::new(mock_binary).spawn()?;
        let paths = service_paths();
        tokio::fs::create_dir_all(paths.runtime_dir()).await?;

        let core_socket = paths.runtime_dir().join("reconcile-core.sock");
        let record = serde_json::json!({
            "pid": old_core.id(),
            "ipc_path": core_socket.to_string_lossy(),
        });
        tokio::fs::write(paths.core_runtime_path(), serde_json::to_vec(&record)?).await?;

        reconcile_service_startup().await?;

        let exited = wait_for_child_exit(&mut old_core, Duration::from_secs(3)).await?;
        assert!(
            exited.is_some(),
            "reconcile should terminate old unsupervised core"
        );
        assert!(
            !paths.core_runtime_path().exists(),
            "reconcile should remove stale core runtime record"
        );

        kill_child(&mut old_core);
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn core_watchdog_stops_after_bounded_crash_loop() -> Result<()> {
        common::init_tracing_for_tests();
        let _ = stop_ipc_server().await;
        struct WatchdogConfigReset;
        impl Drop for WatchdogConfigReset {
            fn drop(&mut self) {
                set_core_watchdog_config_for_tests(None);
            }
        }
        let _reset = WatchdogConfigReset;

        set_core_watchdog_config_for_tests(Some(CoreWatchdogTestConfig {
            max_restarts: 2,
            restart_window: Duration::from_secs(10),
            max_backoff: Duration::ZERO,
        }));

        let crash_binary = ensure_test_bin("crash_binary")?;
        let server_handle = run_ipc_server().await?;
        wait_for_ipc_ready().await?;

        let baseline_restart_count = service_status_snapshot().await?.restart_count;
        let clash_config = ClashConfig {
            core_config: CoreConfig {
                core_path: crash_binary.to_string_lossy().to_string(),
                ..Default::default()
            },
            log_config: Default::default(),
        };
        let response = start_clash(&clash_config).await?;
        assert_eq!(response.code, 0);

        wait_until_async("bounded crash loop", Duration::from_secs(5), || async {
            service_status_snapshot()
                .await
                .map(|status| {
                    status.restart_count >= baseline_restart_count + 2 && status.core_pid.is_none()
                })
                .unwrap_or(false)
        })
        .await?;

        let status = service_status_snapshot().await?;
        assert!(
            status.last_core_exit_reason.is_some(),
            "status should retain the last core exit reason"
        );
        assert!(
            status.restart_count >= baseline_restart_count + 2,
            "watchdog should record bounded restart attempts"
        );
        assert!(
            status.core_pid.is_none(),
            "watchdog should stop supervising after crash loop limit"
        );

        persist_core_stopped().await?;
        stop_ipc_server().await?;
        server_handle.await??;
        Ok(())
    }
}
