use clash_verge_service_ipc::{run_ipc_server, set_ipc_socket_permissions, IPC_PATH};
use kode_bridge::KodeBridgeError;
use tokio::signal::unix::{SignalKind, signal};
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

    let mut server = Some(tokio::spawn(async {
        run_ipc_server().await
    }));

    #[cfg(unix)]
    {
        // Set IPC socket permissions once after a short delay to ensure the socket is created.
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_micros(100)).await;
            if let Err(e) = set_ipc_socket_permissions(IPC_PATH) {
                tracing::error!("Failed to set IPC socket permissions: {}", e);
            } else {
                info!("IPC socket permissions set to 666");
            }
        });
    }

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
        info!("IPC server started. Waiting for Ctrl+C or SIGTERM to shut down...");
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(windows)]
        let sigterm =
            tokio::signal::windows::signal(tokio::signal::windows::SignalKind::terminate())?;

        tokio::select! {
            _ = ctrl_c => {
                info!("Received Ctrl+C. Shutting down IPC server...");
                std::process::exit(0);
            },
            #[cfg(windows)]
            _ = sigterm.recv() => {
                info!("Received SIGTERM. Shutting down IPC server...");
                std::process::exit(0);
            },
            res = server => {
                info!("IPC server task finished.");
                return res.map_err(|e| KodeBridgeError::from(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))?;
            }
        }
    }
}
