use kode_bridge::IpcHttpServer;
use once_cell::sync::Lazy;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

pub(super) struct IpcState {
    pub(super) server: Arc<Mutex<Option<IpcHttpServer>>>,
    pub(super) sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    pub(super) done: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl IpcState {
    pub(super) fn global() -> &'static Arc<Mutex<IpcState>> {
        static IPC_STATE: Lazy<Arc<Mutex<IpcState>>> = Lazy::new(|| {
            Arc::new(Mutex::new(IpcState {
                server: Arc::new(Mutex::new(None)),
                sender: Arc::new(Mutex::new(None)),
                done: Arc::new(Mutex::new(None)),
            }))
        });
        &IPC_STATE
    }

    pub(super) async fn set_server(&self, server: IpcHttpServer) {
        let mut guard = self.server.lock().await;
        *guard = Some(server);
    }

    pub(super) fn get_server(&self) -> Arc<Mutex<Option<IpcHttpServer>>> {
        Arc::clone(&self.server)
    }

    pub(super) async fn set_sender(&self, sender: oneshot::Sender<()>) {
        let mut guard = self.sender.lock().await;
        *guard = Some(sender);
    }

    pub(super) async fn take_sender(&self) -> Option<oneshot::Sender<()>> {
        let mut guard = self.sender.lock().await;
        guard.take()
    }

    pub(super) async fn set_done(&self, done: oneshot::Receiver<()>) {
        let mut guard = self.done.lock().await;
        *guard = Some(done);
    }

    pub(super) async fn take_done(&self) -> Option<oneshot::Receiver<()>> {
        let mut guard = self.done.lock().await;
        guard.take()
    }
}
