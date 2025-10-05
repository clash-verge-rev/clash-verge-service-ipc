//! Clash Verge Service - Cross-platform IPC service daemon
//!
//! This service can run in two modes:
//! - As a system service (Windows Service / systemd / launchd)
//! - As a standalone process (for testing/debugging)
//!
//! Note: We do NOT use #![windows_subsystem = "windows"] to allow console output
//! in standalone mode. When running as a service, SCM will manage the process
//! without a visible console window.

use clash_verge_service_ipc::{run_ipc_server, stop_ipc_server};
use kode_bridge::KodeBridgeError;
#[cfg(windows)]
use tracing::{Level, error, info};
#[cfg(unix)]
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

// ============================================================================
// Platform-specific imports
// ============================================================================

#[cfg(windows)]
mod windows_service_impl {
    pub use std::ffi::OsString;
    pub use std::sync::{Arc, Mutex};
    pub use std::time::Duration;
    pub use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };
}

#[cfg(unix)]
mod unix_signal {
    pub use tokio::signal::unix::{SignalKind, signal};
}

// ============================================================================
// Main entry point
// ============================================================================

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows::main_entry()
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), KodeBridgeError> {
    unix::run_service().await
}

// ============================================================================
// Windows implementation
// ============================================================================

#[cfg(windows)]
mod windows {
    use super::*;
    use windows_service_impl::*;

    define_windows_service!(ffi_service_main, service_main);

    /// Parse command line arguments
    fn parse_args() -> bool {
        let args: Vec<String> = std::env::args().collect();
        let run_as_service = args.iter().any(|arg| arg == "--service" || arg == "-s");
        run_as_service
    }

    /// Check if we have a console attached (running in terminal)
    fn has_console() -> bool {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal()
    }

    /// Main entry point for Windows
    pub fn main_entry() -> Result<(), Box<dyn std::error::Error>> {
        let run_as_service = parse_args();

        if run_as_service {
            // Explicitly requested to run as service
            // This is called by SCM with --service flag
            service_dispatcher::start("clash_verge_service", ffi_service_main)?;
            Ok(())
        } else {
            // Run in standalone mode (debugging/testing)

            // Check if we're running in a terminal
            let in_terminal = has_console();

            eprintln!("╔════════════════════════════════════════════════════════════════╗");
            eprintln!("║  Clash Verge Service - Standalone Mode                        ║");
            eprintln!("╚════════════════════════════════════════════════════════════════╝");
            eprintln!();
            eprintln!("Running in standalone mode (not as a Windows service)");
            if in_terminal {
                eprintln!("Console: Attached (terminal session detected)");
            } else {
                eprintln!("Console: Window mode (launched directly)");
            }
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  - Install as service: clash-verge-service-install.exe");
            eprintln!("  - Run as service:     clash-verge-service.exe --service");
            eprintln!("  - Force console:      clash-verge-service.exe --console");
            eprintln!();

            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                run_standalone()
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
            })
        }
    }

    /// Windows service entry point (called by SCM)
    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_as_service() {
            error!("Service failed: {:?}", e);
        }
    }

    /// Run as a Windows service (with SCM integration)
    fn run_as_service() -> Result<(), Box<dyn std::error::Error>> {
        // Initialize file-based logging (services don't have console)
        let log_file = std::env::temp_dir().join("clash-verge-service.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)?;

        let subscriber = FmtSubscriber::builder()
            .with_max_level(Level::INFO)
            .with_writer(Mutex::new(file))
            .finish();

        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set default subscriber");

        info!("Service starting... Log file: {:?}", log_file);

        // Setup shutdown signal
        let shutdown_signal = Arc::new(Mutex::new(false));
        let shutdown_signal_clone = Arc::clone(&shutdown_signal);

        // Register service control handler
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop => {
                    info!("Received stop signal from SCM");
                    *shutdown_signal_clone.lock().unwrap() = true;
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle =
            service_control_handler::register("clash_verge_service", event_handler)?;

        // Notify SCM: Service is starting
        set_service_status(
            &status_handle,
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            0,
            Duration::from_secs(5),
        )?;

        // Create tokio runtime and start IPC server
        info!("Creating tokio runtime...");
        let rt = tokio::runtime::Runtime::new()?;

        info!("Starting IPC server...");
        let server_handle = rt.block_on(async {
            match run_ipc_server().await {
                Ok(handle) => {
                    info!("IPC server started successfully");
                    Some(handle)
                }
                Err(e) => {
                    error!("Failed to start IPC server: {:?}", e);
                    None
                }
            }
        });

        if server_handle.is_none() {
            error!("Failed to start IPC server, stopping service");
            set_service_status(
                &status_handle,
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                1,
                Duration::default(),
            )?;
            return Err("Failed to start IPC server".into());
        }

        // Notify SCM: Service is running
        info!("Service running, notifying SCM");
        set_service_status(
            &status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP,
            0,
            Duration::default(),
        )?;

        info!("Service is now running. Waiting for stop signal...");

        // Wait for shutdown signal
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if *shutdown_signal.lock().unwrap() {
                info!("Shutdown signal received, stopping service");
                break;
            }
        }

        // Notify SCM: Service is stopping
        info!("Setting service state to StopPending");
        set_service_status(
            &status_handle,
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
            0,
            Duration::from_secs(5),
        )?;

        // Stop IPC server
        info!("Stopping IPC server...");
        rt.block_on(async {
            let _ = stop_ipc_server().await;
        });

        // Notify SCM: Service stopped
        info!("Service stopped, notifying SCM");
        set_service_status(
            &status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            0,
            Duration::default(),
        )?;

        info!("Service shutdown complete");
        Ok(())
    }

    /// Helper to set service status
    fn set_service_status(
        status_handle: &windows_service::service_control_handler::ServiceStatusHandle,
        state: ServiceState,
        controls_accepted: ServiceControlAccept,
        exit_code: u32,
        wait_hint: Duration,
    ) -> windows_service::Result<()> {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint,
            process_id: None,
        })
    }

    /// Run in standalone mode (not as a service)
    async fn run_standalone() -> Result<(), KodeBridgeError> {
        use std::io::IsTerminal;
        use tokio::signal::windows;

        // Determine if we're running in a terminal or not
        let in_terminal = std::io::stdout().is_terminal();

        // Initialize logging with fallback to file if no terminal
        if in_terminal {
            // Console logging for terminal mode
            let subscriber = FmtSubscriber::builder()
                .with_max_level(Level::INFO)
                .with_writer(std::io::stdout)
                .with_ansi(true)
                .finish();

            tracing::subscriber::set_global_default(subscriber)
                .expect("Failed to set default subscriber");
        } else {
            // File logging for non-terminal mode (e.g., double-clicked from Explorer)
            let log_file = std::env::temp_dir().join("clash-verge-service-standalone.log");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_file)
                .expect("Failed to open log file");

            eprintln!("No terminal detected. Logging to: {:?}", log_file);
            eprintln!("The console window will remain open. Press Ctrl+C to exit.");
            eprintln!();

            let subscriber = FmtSubscriber::builder()
                .with_max_level(Level::INFO)
                .with_writer(Mutex::new(file))
                .finish();

            tracing::subscriber::set_global_default(subscriber)
                .expect("Failed to set default subscriber");
        }

        let pid = std::process::id();
        info!("Current process PID: {}", pid);
        info!("Starting Clash Verge Service IPC server...");

        // Start IPC server
        let server_handle = run_ipc_server().await?;

        info!("IPC server started successfully!");
        info!("Press Ctrl+C or Ctrl+Break to shut down...");

        // Spawn a task to monitor the server handle (but don't block on it)
        let monitor_handle = tokio::spawn(async move {
            match server_handle.await {
                Ok(Ok(())) => {
                    error!("IPC server task finished unexpectedly (no error)");
                }
                Ok(Err(e)) => {
                    error!("IPC server task finished with error: {:?}", e);
                }
                Err(e) => {
                    error!("IPC server task panicked: {:?}", e);
                }
            }
        });

        // Wait for shutdown signals only
        let mut ctrl_c = windows::ctrl_c()?;
        let mut ctrl_break = windows::ctrl_break()?;

        tokio::select! {
            _ = ctrl_c.recv() => {
                info!("Received Ctrl+C. Shutting down...");
            },
            _ = ctrl_break.recv() => {
                info!("Received Ctrl+Break. Shutting down...");
            },
        }

        // Graceful shutdown
        info!("Stopping IPC server...");
        let _ = stop_ipc_server().await;

        // Abort the monitor task since we're shutting down intentionally
        monitor_handle.abort();

        info!("Service shutdown complete.");
        Ok(())
    }
}

// ============================================================================
// Unix implementation (Linux & macOS)
// ============================================================================

#[cfg(unix)]
mod unix {
    use super::*;
    use unix_signal::*;

    /// Run as a Unix service (systemd/launchd)
    pub async fn run_service() -> Result<(), KodeBridgeError> {
        // Initialize console/syslog logging
        let subscriber = FmtSubscriber::builder()
            .with_max_level(Level::INFO)
            .with_writer(std::io::stdout)
            .finish();

        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set default subscriber");

        let pid = std::process::id();
        info!("Current process PID: {}", pid);

        // Start IPC server
        let mut server_handle = run_ipc_server().await?;

        info!("IPC server started. Waiting for signals (SIGINT/SIGTERM) to shut down...");

        // Setup signal handlers
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;

        // Wait for shutdown signals
        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT (Ctrl+C). Shutting down...");
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM. Shutting down...");
            },
            res = &mut server_handle => {
                info!("IPC server task finished.");
                let _ = stop_ipc_server().await;
                return res.map_err(|e| {
                    KodeBridgeError::from(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                })?;
            }
        }

        // Graceful shutdown
        let _ = stop_ipc_server().await;
        info!("Service shutdown complete");
        std::process::exit(0);
    }
}
