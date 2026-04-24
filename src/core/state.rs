use crate::core::structure::ServiceLifecycleState;
use kode_bridge::IpcHttpServer;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::{Mutex, oneshot};

pub(super) struct IpcState {
    server: Mutex<Option<IpcHttpServer>>,
    sender: Mutex<Option<oneshot::Sender<()>>>,
    done: Mutex<Option<oneshot::Receiver<()>>>,
}

impl IpcState {
    pub(super) fn global() -> &'static IpcState {
        static IPC_STATE: Lazy<IpcState> = Lazy::new(|| IpcState {
            server: Mutex::new(None),
            sender: Mutex::new(None),
            done: Mutex::new(None),
        });
        &IPC_STATE
    }

    pub(super) async fn set_server(&self, server: IpcHttpServer) {
        let mut guard = self.server.lock().await;
        *guard = Some(server);
    }

    pub(super) async fn take_server(&self) -> Option<IpcHttpServer> {
        self.server.lock().await.take()
    }

    pub(super) async fn shutdown_server(&self) {
        let mut guard = self.server.lock().await;
        if let Some(server) = guard.as_mut() {
            server.shutdown();
        }
        *guard = None;
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

pub fn set_service_lifecycle_state(state: ServiceLifecycleState) {
    service_lifecycle_state_cell().store(state as u8, Ordering::Relaxed);
}

pub fn service_lifecycle_state() -> ServiceLifecycleState {
    ServiceLifecycleState::from_u8(service_lifecycle_state_cell().load(Ordering::Relaxed))
}

fn service_lifecycle_state_cell() -> &'static AtomicU8 {
    static SERVICE_STATE: Lazy<AtomicU8> =
        Lazy::new(|| AtomicU8::new(ServiceLifecycleState::Starting as u8));
    &SERVICE_STATE
}
