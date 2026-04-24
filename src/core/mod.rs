pub mod command;
pub use command::IpcCommand;

pub mod structure;
pub use structure::{
    ClashConfig, CoreConfig, ServiceLifecycleState, ServiceStatusSnapshot, WriterConfig,
};

pub mod paths;
pub use paths::{ServicePaths, service_paths};

#[cfg(feature = "standalone")]
mod auth;
#[cfg(feature = "standalone")]
mod desired;
#[cfg(feature = "standalone")]
mod logger;
#[cfg(feature = "standalone")]
mod manager;
#[cfg(feature = "standalone")]
mod owner;
#[cfg(feature = "standalone")]
mod process;
#[cfg(feature = "standalone")]
mod reconcile;
#[cfg(feature = "standalone")]
mod runtime;
#[cfg(feature = "standalone")]
mod server;
#[cfg(feature = "standalone")]
mod state;
#[cfg(feature = "standalone")]
mod status;

#[cfg(feature = "standalone")]
pub use desired::{
    DesiredState, load_desired_state, persist_core_started, persist_core_stopped,
    persist_writer_config, restore_desired_state,
};
#[cfg(all(feature = "standalone", feature = "test"))]
pub use manager::{CoreWatchdogTestConfig, set_core_watchdog_config_for_tests};
#[cfg(feature = "standalone")]
pub use owner::{ServiceOwnerGuard, acquire_service_owner};
#[cfg(feature = "standalone")]
pub use reconcile::reconcile_service_startup;
#[cfg(feature = "standalone")]
pub use server::{run_ipc_server, run_ipc_supervisor_until_shutdown, stop_ipc_server};
#[cfg(feature = "standalone")]
pub use state::{service_lifecycle_state, set_service_lifecycle_state};
#[cfg(feature = "standalone")]
pub use status::service_status_snapshot;
