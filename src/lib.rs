mod core;

#[cfg(feature = "client")]
mod client;

pub use core::{ClashConfig, CoreConfig, IpcCommand, WriterConfig};

#[cfg(feature = "standalone")]
pub use core::{run_ipc_server, stop_ipc_server};

#[cfg(feature = "client")]
pub use client::*;

#[cfg(all(unix, not(feature = "test")))]
pub static IPC_PATH: &str = "/tmp/verge/clash-verge-service.sock";
#[cfg(all(windows, not(feature = "test")))]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service";

#[cfg(all(feature = "test", unix))]
pub static IPC_PATH: &str = "/tmp/verge/clash-verge-service.sock";
// pub static IPC_PATH: &str = "/tmp/verge/clash-verge-service-test.sock";
#[cfg(all(feature = "test", windows))]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service-test";

#[cfg(any(feature = "standalone", feature = "client"))]
pub static IPC_AUTH_EXPECT: &str =
    r#"Like as the waves make towards the pebbl'd shore, So do our minutes hasten to their end;"#;

pub static VERSION: &str = env!("CARGO_PKG_VERSION");
