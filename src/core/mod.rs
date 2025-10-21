pub mod command;
pub use command::IpcCommand;

pub mod structure;
pub use structure::{ClashConfig, CoreConfig, WriterConfig};

#[cfg(feature = "standalone")]
mod auth;
#[cfg(feature = "standalone")]
mod logger;
#[cfg(feature = "standalone")]
mod manager;
#[cfg(feature = "standalone")]
mod server;
#[cfg(feature = "standalone")]
mod state;

#[cfg(feature = "standalone")]
pub use server::{run_ipc_server, stop_ipc_server};
