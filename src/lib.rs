mod core;

#[cfg(feature = "client")]
mod client;

pub use core::{
    ClashConfig, CoreConfig, IpcCommand, ServiceLifecycleState, ServiceStatusSnapshot, WriterConfig,
};
pub use core::{ServicePaths, service_paths};

#[cfg(feature = "standalone")]
pub use core::{
    DesiredState, ServiceOwnerGuard, acquire_service_owner, load_desired_state,
    persist_core_started, persist_core_stopped, persist_writer_config, reconcile_service_startup,
    restore_desired_state, run_ipc_server, run_ipc_supervisor_until_shutdown,
    service_lifecycle_state, service_status_snapshot, set_service_lifecycle_state, stop_ipc_server,
};

#[cfg(all(feature = "standalone", feature = "test"))]
pub use core::{CoreWatchdogTestConfig, set_core_watchdog_config_for_tests};

#[cfg(feature = "client")]
pub use client::*;

#[cfg(all(unix, not(feature = "test")))]
pub static IPC_PATH: &str = "/tmp/verge/clash-verge-service.sock";
#[cfg(all(windows, not(feature = "test")))]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service";

#[cfg(all(feature = "test", unix))]
pub static IPC_PATH: &str = "/tmp/clash-verge-service-ipc-test/service.sock";
#[cfg(all(feature = "test", windows))]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service-test";

#[cfg(any(feature = "standalone", feature = "client"))]
pub static IPC_AUTH_EXPECT: &str = r#"A thing of beauty is a joy for ever. Its loveliness increases; it will never pass into nothingness."#;

pub static VERSION: &str = env!("CARGO_PKG_VERSION");
