use crate::core::runtime_generation::approved_core_copy;
use crate::core::trusted_core_location::require_trusted_core_location;
use crate::core::{
    desired::{load_active_owner, load_owner_desired_state},
    manager::core_status_snapshot,
    paths::service_paths,
};
use crate::{CoreAvailability, CoreInspection, CoreRequirement, InstallationStatus, ProtocolInfo};
use std::path::Path;

pub(super) async fn inspect(
    requirements: &[CoreRequirement],
    include_service_digest: bool,
) -> anyhow::Result<InstallationStatus> {
    anyhow::ensure!(requirements.len() <= 2, "at most two cores can be inspected");
    let recovering = match load_active_owner().await? {
        Some(owner) => load_owner_desired_state(&owner.owner_key).await?.core_should_be_running,
        None => false,
    };
    let core_busy =
        recovering || core_status_snapshot().core_pid.is_some() || crate::execution::CoreExecutionGuard::is_held()?;
    let hash_permit =
        hash_permit(include_service_digest || requirements.iter().any(|required| required.sha256.is_some()))?;
    let requirements = requirements.to_vec();
    tokio::task::spawn_blocking(move || {
        let _hash_permit = hash_permit;
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
        Ok(InstallationStatus {
            service_sha256: if include_service_digest {
                installation_digest(&std::env::current_exe()?)?
            } else {
                String::new()
            },
            protocol: ProtocolInfo::current(),
            cores,
            core_busy,
        })
    })
    .await?
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
            if installation_digest(&approved)? != expected.to_ascii_lowercase() {
                return Ok(CoreAvailability::DigestMismatch);
            }
        }
        Ok(CoreAvailability::Ready)
    })();
    result.unwrap_or_else(|error| CoreAvailability::Rejected {
        reason: error.to_string(),
    })
}

// A cancelled inspection must not release capacity while its blocking hash is
// still running. Keep the permit in the worker and reject duplicate work rather
// than queue unbounded hashes from raw IPC clients.
static HASH_INSPECTION: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

fn hash_permit(required: bool) -> anyhow::Result<Option<tokio::sync::SemaphorePermit<'static>>> {
    if required {
        Ok(Some(HASH_INSPECTION.try_acquire().map_err(|_| {
            anyhow::anyhow!("installation hash inspection is busy")
        })?))
    } else {
        Ok(None)
    }
}

fn installation_digest(path: &Path) -> anyhow::Result<String> {
    // Metadata does not reliably invalidate same-length, preserved-mtime edits
    // on every platform. Explicit installer verification always hashes bytes.
    crate::management::sha256_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_hashes_are_rejected_without_blocking_lightweight_inspection() -> anyhow::Result<()> {
        let permit = hash_permit(true)?;
        assert!(hash_permit(true).is_err());
        assert!(hash_permit(false)?.is_none());
        drop(permit);
        assert!(hash_permit(true)?.is_some());
        Ok(())
    }

    #[test]
    fn installation_hash_observes_replacement_and_same_length_edits() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("service-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("artifact");
        std::fs::write(&path, b"first")?;
        let first = installation_digest(&path)?;
        assert_eq!(first, installation_digest(&path)?);
        let modified = std::fs::metadata(&path)?.modified()?;
        std::fs::write(&path, b"other")?;
        std::fs::File::options()
            .write(true)
            .open(&path)?
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
        let second = installation_digest(&path)?;
        assert_ne!(first, second);
        let replacement = dir.join("replacement");
        std::fs::write(&replacement, b"third")?;
        std::fs::rename(&replacement, &path)?;
        assert_ne!(second, installation_digest(&path)?);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
