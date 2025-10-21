mod core;

#[cfg(feature = "client")]
mod client;

pub use core::{ClashConfig, CoreConfig, IpcCommand, WriterConfig};

#[cfg(feature = "standalone")]
pub use core::{run_ipc_server, stop_ipc_server};

#[cfg(feature = "client")]
pub use client::*;

#[cfg(unix)]
pub static IPC_PATH: &str = if cfg!(test) || cfg!(debug_assertions) {
    "/tmp/verge/clash-verge-service-dev.sock"
} else {
    "/tmp/verge/clash-verge-service.sock"
};
#[cfg(windows)]
pub static IPC_PATH: &str = if cfg!(test) || cfg!(debug_assertions) {
    r"\\.\pipe\clash-verge-service-dev"
} else {
    r"\\.\pipe\clash-verge-service"
};

#[cfg(any(feature = "standalone", feature = "client"))]
pub static IPC_AUTH_EXPECT: &str =
    r#"In me thou see'st the glowing of such fire, That on the ashes of his youth doth lie"#;

pub static VERSION: &str = env!("CARGO_PKG_VERSION");
