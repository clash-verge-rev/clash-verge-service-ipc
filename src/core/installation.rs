use crate::core::runtime_generation::approved_core_copy;
use crate::core::trusted_core_location::require_trusted_core_location;
use crate::core::{desired::load_active_owner, manager::CORE_MANAGER, paths::service_paths};
use crate::{CoreAvailability, CoreInspection, CoreRequirement, InstallationStatus, ProtocolInfo};
use std::path::Path;

pub(super) async fn inspect(requirements: &[CoreRequirement]) -> anyhow::Result<InstallationStatus> {
    anyhow::ensure!(requirements.len() <= 2, "at most two cores can be inspected");
    let directory = service_paths()?.core_dir();
    let cores = requirements
        .iter()
        .map(|required| {
            let availability = inspect_core(&directory, required);
            CoreInspection {
                name: required.name.clone(),
                availability,
            }
        })
        .collect();
    let core_busy = load_active_owner().await?.is_some()
        || CORE_MANAGER.lock().await.status().await.core_pid.is_some()
        || crate::execution::CoreExecutionGuard::is_held()?;
    Ok(InstallationStatus {
        service_sha256: crate::management::sha256_file(&std::env::current_exe()?)?,
        protocol: ProtocolInfo::current(),
        cores,
        core_busy,
    })
}

fn inspect_core(directory: &Path, required: &CoreRequirement) -> CoreAvailability {
    let allowed = [
        format!("verge-mihomo{}", std::env::consts::EXE_SUFFIX),
        format!("verge-mihomo-alpha{}", std::env::consts::EXE_SUFFIX),
    ];
    if !allowed.contains(&required.name) {
        return CoreAvailability::Rejected {
            reason: "unsupported core name".into(),
        };
    }
    let requested = directory.join(&required.name);
    if matches!(std::fs::symlink_metadata(&requested), Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
        return CoreAvailability::Missing;
    }
    let result = (|| -> anyhow::Result<CoreAvailability> {
        let approved = approved_core_copy(directory, &requested)?;
        require_trusted_core_location(&approved)?;
        if let Some(expected) = &required.sha256 {
            anyhow::ensure!(
                expected.len() == 64 && expected.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid SHA-256"
            );
            if crate::management::sha256_file(&approved)? != expected.to_ascii_lowercase() {
                return Ok(CoreAvailability::DigestMismatch);
            }
        }
        Ok(CoreAvailability::Ready)
    })();
    result.unwrap_or_else(|error| CoreAvailability::Rejected {
        reason: error.to_string(),
    })
}
