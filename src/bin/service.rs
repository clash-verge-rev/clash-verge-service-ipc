use clash_verge_service_ipc::run_ipc_server;
use kode_bridge::KodeBridgeError;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
#[cfg(windows)]
use tokio::signal::windows;
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), KodeBridgeError> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_writer(std::io::stdout)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let pid = std::process::id();
    info!("Current process PID: {}", pid);

    let mut server = Some(tokio::spawn(async { run_ipc_server().await }));

    #[cfg(unix)]
    {
        info!(
            "IPC server started. Waiting for Ctrl+C (Command+C on macOS) or SIGTERM to shut down..."
        );
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;

        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT (Ctrl+C / Command+C). Shutting down IPC server...");
                std::process::exit(0);
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM. Shutting down IPC server...");
                std::process::exit(0);
            },
            res = &mut server.as_mut().unwrap() => {
                info!("IPC server task finished.");
                return res.map_err(|e| KodeBridgeError::from(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))?;
            }
        }
    }
    #[cfg(windows)]
    {
        info!("IPC server started. Waiting for Ctrl+C or Ctrl+Break to shut down...");
        let mut ctrl_c = windows::ctrl_c()?;
        let mut ctrl_break = windows::ctrl_break()?;

        tokio::select! {
            _ = ctrl_c.recv() => {
                info!("Received Ctrl+C. Shutting down IPC server...");
                return Ok(());
            },
            _ = ctrl_break.recv() => {
                info!("Received Ctrl+Break. Shutting down IPC server...");
                return Ok(());
            },
            res = &mut server.as_mut().unwrap() => {
                info!("IPC server task finished.");
                return res.map_err(|e| KodeBridgeError::from(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))?;
            }
        }
    }
}
