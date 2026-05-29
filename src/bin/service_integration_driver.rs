#![cfg(feature = "client")]

use clash_verge_service_ipc::{
    ClashConfig, CoreConfig, IpcConfig, WriterConfig, get_status, set_config, start_clash,
    stop_clash, stop_ipc_server,
};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::time::sleep;

const IPC_READY_TIMEOUT: Duration = Duration::from_secs(20);
const IPC_PROBE_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: service-integration-driver <ping|start|stop>");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "ping" => wait_ipc_ready().await?,
        "start" => start_flow().await?,
        "stop" => stop_flow().await?,
        _ => {
            eprintln!("usage: service-integration-driver <ping|start|stop>");
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn start_flow() -> anyhow::Result<()> {
    wait_ipc_ready().await?;
    let config = ClashConfig {
        core_config: CoreConfig {
            core_path: mock_binary_path()?,
            ..Default::default()
        },
        log_config: WriterConfig::default(),
    };
    start_clash(&config).await?;
    Ok(())
}

async fn stop_flow() -> anyhow::Result<()> {
    let _ = stop_clash().await;
    let _ = stop_ipc_server().await;
    Ok(())
}

async fn wait_ipc_ready() -> anyhow::Result<()> {
    set_config(Some(IpcConfig {
        default_timeout: Duration::from_millis(250),
        max_retries: 1,
        retry_delay: Duration::from_millis(25),
    }))
    .await;

    let result: anyhow::Result<()> = async {
        let deadline = Instant::now() + IPC_READY_TIMEOUT;
        while Instant::now() < deadline {
            if get_status().await.is_ok() {
                return Ok(());
            }
            sleep(IPC_PROBE_INTERVAL).await;
        }
        anyhow::bail!("IPC server not reachable within {:?}", IPC_READY_TIMEOUT)
    }
    .await;

    set_config(None).await;
    result
}

fn mock_binary_path() -> anyhow::Result<String> {
    let current_exe = std::env::current_exe()?;
    let mut path = current_exe;
    path.pop();
    #[cfg(windows)]
    path.push("mock_binary.exe");
    #[cfg(not(windows))]
    path.push("mock_binary");
    if path.exists() {
        return Ok(path.to_string_lossy().to_string());
    }

    let status = Command::new("cargo")
        .args(["build", "--features", "test"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to build mock_binary");
    }
    if path.exists() {
        return Ok(path.to_string_lossy().to_string());
    }
    anyhow::bail!("mock_binary not found after build");
}
