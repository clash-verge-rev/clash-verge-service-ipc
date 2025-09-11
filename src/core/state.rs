use kode_bridge::IpcHttpServer;
use once_cell::sync::Lazy;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Default)]
pub(super) struct IpcState {
    pub(super) server: Option<Arc<RwLock<IpcHttpServer>>>,
}

impl IpcState {
    pub(super) fn global() -> &'static Arc<RwLock<IpcState>> {
        static IPC_STATE: Lazy<Arc<RwLock<IpcState>>> =
            Lazy::new(|| Arc::new(RwLock::new(IpcState::default())));
        &IPC_STATE
    }

    pub(super) async fn set_server(server: IpcHttpServer) {
        let mut guard = IpcState::global().write().await;
        guard.server = Some(Arc::new(RwLock::new(server)));
    }

    pub(super) async fn get_server() -> Arc<RwLock<IpcHttpServer>> {
        let guard = IpcState::global().read().await;
        guard
            .server
            .as_ref()
            .expect("Server not initialized")
            .clone()
    }
}
