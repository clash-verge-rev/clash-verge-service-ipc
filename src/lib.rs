mod core;

#[cfg(feature = "client")]
mod client;

pub use core::{CoreConfig, IpcCommand, ClashConfig, WriterConfig};

#[cfg(feature = "standalone")]
pub use core::{run_ipc_server, stop_ipc_server};

#[cfg(feature = "client")]
pub use client::*;

#[cfg(unix)]
pub static IPC_PATH: &str = "/tmp/verge/clash-verge-service.sock";
#[cfg(windows)]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service";

pub static VERSION: &str = env!("CARGO_PKG_VERSION");
