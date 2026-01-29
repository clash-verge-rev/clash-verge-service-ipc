#![cfg(feature = "client")]

use clash_verge_service_ipc::{
    ClashConfig, CoreConfig, WriterConfig, connect, start_clash, stop_clash, stop_ipc_server,
};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: service-integration-driver <start|stop>");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "start" => start_flow().await?,
        "stop" => stop_flow().await?,
        _ => {
            eprintln!("usage: service-integration-driver <start|stop>");
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
    for _ in 0..20 {
        if connect().await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("IPC server not reachable");
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
