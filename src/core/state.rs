use kode_bridge::IpcHttpServer;
use once_cell::sync::Lazy;
use std::sync::Arc;
use tokio::sync::{RwLock, oneshot};

pub(super) struct IpcState {
    pub(super) server: Arc<RwLock<Option<IpcHttpServer>>>,
    pub(super) sender: Arc<RwLock<Option<oneshot::Sender<()>>>>,
}

impl IpcState {
    pub(super) fn global() -> &'static Arc<RwLock<IpcState>> {
        static IPC_STATE: Lazy<Arc<RwLock<IpcState>>> = Lazy::new(|| {
            Arc::new(RwLock::new(IpcState {
                server: Arc::new(RwLock::new(None)),
                sender: Arc::new(RwLock::new(None)),
            }))
        });
        &IPC_STATE
    }

    pub(super) async fn set_server(&self, server: IpcHttpServer) {
        let mut guard = self.server.write().await;
        *guard = Some(server);
    }

    pub(super) fn get_server(&self) -> Arc<RwLock<Option<IpcHttpServer>>> {
        Arc::clone(&self.server)
    }

    pub(super) async fn set_sender(&self, sender: oneshot::Sender<()>) {
        let mut guard = self.sender.write().await;
        *guard = Some(sender);
    }

    pub(super) async fn take_sender(&self) -> Option<oneshot::Sender<()>> {
        let mut guard = self.sender.write().await;
        guard.take()
    }
}
