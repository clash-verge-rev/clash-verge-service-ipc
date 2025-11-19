mod core;

#[cfg(feature = "standalone")]
pub mod cli;

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
pub static IPC_PATH: &str = "/tmp/verge/clash-verge-service-test.sock";
#[cfg(all(feature = "test", windows))]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service-test";

#[cfg(any(feature = "standalone", feature = "client"))]
pub static IPC_AUTH_EXPECT: &str =
    r#"In me thou see'st the glowing of such fire, That on the ashes of his youth doth lie"#;

pub static VERSION: &str = env!("CARGO_PKG_VERSION");
