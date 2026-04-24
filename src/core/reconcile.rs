use crate::core::process::{is_process_alive, terminate_process};
use crate::core::runtime::{
    cleanup_core_socket, is_core_socket_reachable, read_core_runtime_record,
    remove_core_runtime_record,
};
use anyhow::Result;
use tracing::{info, warn};

pub async fn reconcile_service_startup() -> Result<()> {
    info!("Running service startup reconciliation");

    let Some(record) = read_core_runtime_record().await? else {
        return Ok(());
    };

    let pid_alive = is_process_alive(record.pid);
    let socket_reachable = is_core_socket_reachable(&record.ipc_path).await;

    if pid_alive {
        warn!(
            "Found previous core process {} during startup; stopping it before supervision resumes",
            record.pid
        );
        terminate_process(record.pid).await;
        cleanup_core_socket(&record.ipc_path).await;
        remove_core_runtime_record().await;
        return Ok(());
    }

    if !socket_reachable {
        info!(
            "Cleaning stale core socket from dead process: {}",
            record.ipc_path
        );
        cleanup_core_socket(&record.ipc_path).await;
    } else {
        warn!(
            "Core runtime PID {} is dead but socket {} is reachable; leaving socket untouched",
            record.pid, record.ipc_path
        );
    }

    remove_core_runtime_record().await;
    Ok(())
}
