//! Clash Verge Service - Cross-platform IPC service daemon
//!
//! This service can run as a standalone process or as a Windows service.
//! It listens for shutdown signals (Ctrl+C, SIGTERM, or service stop) to gracefully terminate.

use anyhow::Result;
use clash_verge_service_ipc::{
    acquire_service_owner, reconcile_service_startup, restore_desired_state,
    run_ipc_supervisor_until_shutdown,
};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[cfg(windows)]
use {
    platform_lib::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    },
    std::ffi::OsString,
    std::time::Duration,
};

// --- Main Entry Points ---

/// Main entry point for non-Windows platforms (Linux, macOS).
#[cfg(not(windows))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_logger();
    run_standalone().await
}

/// Main entry point for Windows.
/// Tries to run as a service, falls back to standalone mode if that fails.
#[cfg(windows)]
fn main() -> Result<()> {
    init_logger();
    if service_dispatcher::start("clash_verge_service", ffi_service_main).is_err() {
        info!("Not running as a service, starting in standalone mode.");
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_standalone())?;
    }
    Ok(())
}

// --- Windows Service Implementation ---

#[cfg(windows)]
define_windows_service!(ffi_service_main, my_service_main);

/// The entry point for the Windows service.
#[cfg(windows)]
fn my_service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        info!("Service failed to run: {}", e);
    }
}

/// Contains the core logic for running as a Windows service.
#[cfg(windows)]
fn run_service() -> platform_lib::Result<()> {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                let _ = shutdown_tx.blocking_send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register("clash_verge_service", event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fatal = rt.block_on(async {
        let owner_guard = match acquire_service_owner().await {
            Ok(Some(owner_guard)) => owner_guard,
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!("Failed to acquire service owner lock: {}", error);
                return true;
            }
        };

        if let Err(error) = reconcile_service_startup().await {
            tracing::warn!("Service startup reconciliation failed: {}", error);
            return true;
        }
        if let Err(error) = restore_desired_state().await {
            tracing::warn!("Desired state restoration failed: {}", error);
            return true;
        }

        let result = run_ipc_supervisor_until_shutdown(async {
            let _ = shutdown_rx.recv().await;
        })
        .await;
        if let Err(error) = result {
            tracing::warn!("IPC supervisor failed: {}", error);
            drop(owner_guard);
            return true;
        }

        drop(owner_guard);
        false
    });

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(if fatal { 1 } else { 0 }),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    if fatal {
        std::process::exit(1);
    }

    Ok(())
}

// --- Common Logic ---

/// Initializes the global logger.
fn init_logger() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

async fn run_standalone() -> Result<()> {
    let pid = std::process::id();
    info!("Clash Verge Service - Standalone Mode");
    info!("Current process PID: {}", pid);

    let Some(_owner_guard) = acquire_service_owner().await? else {
        return Ok(());
    };

    reconcile_service_startup().await?;
    restore_desired_state().await?;

    run_ipc_supervisor_until_shutdown(shutdown_signal()).await?;

    info!("Service shutdown complete.");
    Ok(())
}

/// Waits for a shutdown signal appropriate for the current platform.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");

        tokio::select! {
            _ = sigint.recv() => info!("Received SIGINT (Ctrl+C)"),
            _ = sigterm.recv() => info!("Received SIGTERM"),
        }
    }

    #[cfg(windows)]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        info!("Received Ctrl+C");
    }
}
