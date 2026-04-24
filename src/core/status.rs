use crate::core::desired::load_desired_state;
use crate::core::manager::CORE_MANAGER;
use crate::core::state::service_lifecycle_state;
use crate::core::structure::ServiceStatusSnapshot;
use anyhow::Result;

pub async fn service_status_snapshot() -> Result<ServiceStatusSnapshot> {
    let service_state = service_lifecycle_state();
    let core = CORE_MANAGER.lock().await.status().await;
    let desired = load_desired_state().await.unwrap_or_default();

    Ok(ServiceStatusSnapshot {
        service_state,
        core_pid: core.core_pid,
        core_started_at: core.core_started_at,
        last_core_exit_reason: core.last_core_exit_reason,
        restart_count: core.restart_count,
        last_recovery_at: core.last_recovery_at,
        desired_core_should_be_running: desired.core_should_be_running,
        desired_generation: desired.generation,
        desired_updated_at: desired.updated_at,
    })
}
