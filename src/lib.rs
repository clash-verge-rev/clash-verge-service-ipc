mod core;

pub use core::{IpcCommand, run_ipc_server, stop_ipc_server};

#[cfg(unix)]
pub static IPC_PATH: &str = "/tmp/clash-verge-service-ipc.sock";
#[cfg(windows)]
pub static IPC_PATH: &str = r"\\.\pipe\clash-verge-service-ipc";
