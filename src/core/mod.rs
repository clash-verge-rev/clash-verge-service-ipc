pub mod command;
mod state;

pub use command::IpcCommand;
use state::IpcState;

use kode_bridge::{IpcHttpServer, Result, Router, ipc_http_server::HttpResponse};

pub async fn run_ipc_server() -> Result<()> {
    cleanup_ipc_path()?;
    init_ipc_state().await?;
    IpcState::get_server().await.write().await.serve().await
}

pub async fn stop_ipc_server() -> Result<()> {
    IpcState::get_server().await.write().await.shutdown();
    cleanup_ipc_path()?;
    Ok(())
}

fn cleanup_ipc_path() -> Result<()> {
    #[cfg(unix)]
    {
        use crate::IPC_PATH;
        use std::{fs, path::Path};

        if Path::new(IPC_PATH).exists() {
            fs::remove_file(IPC_PATH)?;
        }
    }
    #[cfg(windows)]
    {
        // Named pipes on Windows are automatically cleaned up when the last handle is closed
        // No manual cleanup needed
    }
    Ok(())
}

pub async fn init_ipc_state() -> Result<()> {
    let server = create_ipc_server()?;
    let router = create_ipc_router()?;
    let server = server.router(router);
    IpcState::set_server(server).await;
    Ok(())
}

fn create_ipc_server() -> Result<IpcHttpServer> {
    use crate::IPC_PATH;
    IpcHttpServer::new(IPC_PATH)
}

fn create_ipc_router() -> Result<Router> {
    let router = Router::new().get(IpcCommand::Magic.as_ref(), |_| async move {
        Ok(HttpResponse::builder().text("Tunglies!").build())
    });
    Ok(router)
}
