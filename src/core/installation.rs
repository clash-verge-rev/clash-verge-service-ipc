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
    let requirements = requirements.to_vec();
    tokio::task::spawn_blocking(move || {
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

#[derive(PartialEq, Eq)]
struct DigestIdentity {
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
    #[cfg(windows)]
    inode: (u32, u32, u32, i64),
}

fn digest_identity(file: &std::fs::File) -> anyhow::Result<DigestIdentity> {
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "installation artifact is not a regular file");
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.dev(), metadata.ino(), metadata.ctime(), metadata.ctime_nsec())
    };
    #[cfg(windows)]
    let inode = {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx,
        };
        let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut basic = unsafe { std::mem::zeroed::<FILE_BASIC_INFO>() };
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        (
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
            basic.ChangeTime,
        )
    };
    Ok(DigestIdentity {
        length: metadata.len(),
        modified: metadata.modified()?,
        inode,
    })
}

// Only the helper and two approved core paths enter this bounded, single-flight cache.
// Installer source attestation deliberately continues to hash fresh bytes.
static DIGESTS: parking_lot::Mutex<Vec<(std::path::PathBuf, DigestIdentity, String)>> =
    parking_lot::Mutex::new(Vec::new());

fn installation_digest(path: &Path) -> anyhow::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut cache = DIGESTS.lock();
    let identity = digest_identity(&file)?;
    if let Some((_, _, hash)) = cache.iter().find(|(cached, key, _)| cached == path && *key == identity) {
        anyhow::ensure!(
            identity == digest_identity(&file)?,
            "installation artifact changed during inspection"
        );
        return Ok(hash.clone());
    }
    let hash = crate::management::sha256_reader(&file)?;
    anyhow::ensure!(
        identity == digest_identity(&file)?,
        "installation artifact changed while hashing"
    );
    cache.retain(|(cached, _, _)| cached != path);
    if cache.len() == 3 {
        cache.remove(0);
    }
    cache.push((path.to_path_buf(), identity, hash.clone()));
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_cache_observes_replacement_and_same_length_edits() -> anyhow::Result<()> {
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
