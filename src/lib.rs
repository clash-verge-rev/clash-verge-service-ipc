mod core;

pub use core::IpcCommand;

#[cfg(feature = "standalone")]
pub use core::{run_ipc_server, set_ipc_socket_permissions, stop_ipc_server};

#[cfg(unix)]
pub static IPC_PATH: &str = "/tmp/clash-verge-service-ipc.sock";
#[cfg(windows)]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service-ipc";
