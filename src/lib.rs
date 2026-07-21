mod core;

#[cfg(feature = "client")]
mod client;

pub use core::{
    AuthenticatedRequest, ClashConfig, CoreConfig, IpcCommand, OWNER_TOKEN_FILE_NAME,
    OwnerCredentials, OwnerIdentity, RuntimeAsset, RuntimeBundle, ServiceErrorCode,
    ServiceLifecycleState, ServiceStatusSnapshot, WriterConfig, mihomo_ipc_path, owner_key,
};
pub use core::{OwnerPaths, ServicePaths, service_paths};

#[cfg(feature = "standalone")]
pub use core::{
    ActiveOwnerState, DesiredState, ServiceOwnerGuard, acquire_service_owner,
    cleanup_stale_owner_state, load_active_owner, load_owner_desired_state,
    reconcile_service_startup, restore_desired_state, run_ipc_server,
    run_ipc_supervisor_until_shutdown, service_lifecycle_state, set_service_lifecycle_state,
    stop_ipc_server,
};

#[cfg(feature = "test")]
pub use core::test_owner_credentials;
#[cfg(all(feature = "standalone", feature = "test"))]
pub use core::write_core_runtime_record_for_tests;
#[cfg(all(feature = "standalone", feature = "test"))]
pub use core::{CoreWatchdogTestConfig, set_core_watchdog_config_for_tests};

#[cfg(feature = "client")]
pub use client::*;

#[cfg(all(target_os = "macos", not(feature = "test")))]
pub static IPC_PATH: &str = "/var/run/clash-verge-service/service.sock";
#[cfg(all(unix, not(target_os = "macos"), not(feature = "test")))]
pub static IPC_PATH: &str = "/run/clash-verge-service/service.sock";
#[cfg(all(windows, not(feature = "test")))]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service";

#[cfg(all(feature = "test", unix))]
pub static IPC_PATH: &str = "/tmp/clash-verge-service-ipc-test/service.sock";
#[cfg(all(feature = "test", windows))]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service-test";

#[cfg(any(feature = "standalone", feature = "client"))]
pub static IPC_AUTH_EXPECT: &str = r#"A thing of beauty is a joy for ever. Its loveliness increases; it will never pass into nothingness."#;

pub static VERSION: &str = env!("CARGO_PKG_VERSION");
