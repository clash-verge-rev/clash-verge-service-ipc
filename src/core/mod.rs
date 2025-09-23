pub mod command;
pub use command::IpcCommand;

#[cfg(feature = "standalone")]
mod ipc;
#[cfg(feature = "standalone")]
mod manager;
#[cfg(feature = "standalone")]
mod permission;
#[cfg(feature = "standalone")]
mod state;

#[cfg(feature = "standalone")]
pub use ipc::{run_ipc_server, stop_ipc_server};

#[cfg(feature = "standalone")]
pub use permission::set_ipc_socket_permissions;
