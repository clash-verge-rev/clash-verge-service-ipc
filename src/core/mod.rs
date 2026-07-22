pub mod command;
pub use command::IpcCommand;

pub mod structure;
pub use structure::{
    AuthenticatedRequest, AuthenticatedSessionRequest, ClashConfig, CoreConfig, MacosProxyConfig,
    OWNER_TOKEN_FILE_NAME, OwnerCredentials, OwnerIdentity, OwnerSessionHandle, OwnerSessionProof,
    ProxyApplyOutcome, RuntimeAsset, RuntimeBundle, SERVICE_PROTOCOL_HEADER, SESSION_TOKEN_HEX_LEN,
    ServiceErrorCode, ServiceLifecycleState, ServiceStatusSnapshot, StartClashRequest,
    StartClashResult, WriterConfig, owner_key,
};

pub mod paths;
pub use paths::{OwnerPaths, ServicePaths, mihomo_ipc_path, service_paths};

#[cfg(feature = "standalone")]
mod assets;
#[cfg(feature = "standalone")]
mod atomic_file;
#[cfg(feature = "standalone")]
mod auth;
#[cfg(feature = "standalone")]
mod desired;
#[cfg(feature = "standalone")]
mod legacy_cleanup;
#[cfg(feature = "standalone")]
mod logger;
#[cfg(feature = "standalone")]
mod maintenance;
#[cfg(feature = "standalone")]
mod manager;
#[cfg(feature = "standalone")]
mod owner;
#[cfg(feature = "standalone")]
mod process;
#[cfg(feature = "standalone")]
mod proxy;
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
#[cfg(feature = "test")]
mod test_credentials;
#[cfg(all(feature = "standalone", unix))]
mod unix_security;
#[cfg(all(feature = "standalone", windows))]
mod windows_legacy_cleanup;
#[cfg(all(feature = "standalone", windows))]
mod windows_security;

#[cfg(feature = "standalone")]
pub use desired::{
    ActiveOwnerState, DesiredState, load_active_owner, load_owner_desired_state,
    restore_desired_state,
};
#[cfg(feature = "standalone")]
pub use maintenance::cleanup_stale_owner_state;
#[cfg(all(feature = "standalone", feature = "test"))]
pub use manager::{CoreWatchdogTestConfig, set_core_watchdog_config_for_tests};
#[cfg(feature = "standalone")]
pub use owner::{ServiceOwnerGuard, acquire_service_owner};
#[cfg(feature = "standalone")]
pub use proxy::{apply_proxy, apply_proxy_or_direct, clear_proxy, validate_proxy_config};
#[cfg(feature = "standalone")]
pub use reconcile::reconcile_service_startup;
#[cfg(all(feature = "standalone", feature = "test"))]
pub use runtime::write_core_runtime_record_for_tests;
#[cfg(feature = "standalone")]
pub use server::{run_ipc_server, run_ipc_supervisor_until_shutdown, stop_ipc_server};
#[cfg(feature = "standalone")]
pub use state::{service_lifecycle_state, set_service_lifecycle_state};
#[cfg(feature = "test")]
pub use test_credentials::test_owner_credentials;
#[cfg(all(feature = "test", unix))]
pub use test_credentials::test_owner_credentials_for_uid;
