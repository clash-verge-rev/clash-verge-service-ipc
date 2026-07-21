use crate::core::auth::{AuthenticatedOwner, ServiceError};
use crate::core::paths::ensure_owner_state_directory;
use crate::{
    ClashConfig, CoreConfig, RuntimeBundle, ServiceErrorCode, WriterConfig, mihomo_ipc_path,
};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt as _;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct PreparedRuntime {
    pub(crate) clash_config: ClashConfig,
}

pub(crate) struct StagedRuntime {
    clash_config: ClashConfig,
    staging: PathBuf,
    runtime: PathBuf,
    backup: PathBuf,
}

impl StagedRuntime {
    pub(crate) async fn activate(self) -> Result<PreparedRuntime, ServiceError> {
        let _ = tokio::fs::remove_dir_all(&self.backup).await;
        let had_runtime = tokio::fs::try_exists(&self.runtime).await.unwrap_or(false);
        if had_runtime {
            tokio::fs::rename(&self.runtime, &self.backup)
                .await
                .map_err(|error| {
                    invalid_asset(format!("failed to stage current runtime: {error}"))
                })?;
        }
        if let Err(error) = tokio::fs::rename(&self.staging, &self.runtime).await {
            if had_runtime {
                let _ = tokio::fs::rename(&self.backup, &self.runtime).await;
            }
            return Err(invalid_asset(format!(
                "failed to activate prepared runtime: {error}"
            )));
        }
        if had_runtime {
            let _ = tokio::fs::remove_dir_all(&self.backup).await;
        }
        Ok(PreparedRuntime {
            clash_config: self.clash_config.clone(),
        })
    }
}

impl Drop for StagedRuntime {
    fn drop(&mut self) {
        if self.staging.exists() {
            let _ = std::fs::remove_dir_all(&self.staging);
        }
    }
}

pub(crate) async fn stage_runtime(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
) -> Result<StagedRuntime, ServiceError> {
    let core_path = validate_core_path(owner, &bundle.core_path)?;
    let owner_paths = ensure_owner_state_directory(&owner.identity)
        .map_err(|error| invalid_asset(format!("failed to secure owner state root: {error:#}")))?;
    let owner_root = owner_paths.root();
    crate::core::maintenance::persist_owner_identity(&owner.identity, owner_root)
        .await
        .map_err(|error| invalid_asset(format!("failed to persist owner identity: {error:#}")))?;
    prepare_owner_ipc_directory(owner).await?;

    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let staging = owner_root.join(format!("runtime.staging-{}-{sequence}", std::process::id()));
    let runtime = owner_paths.runtime_dir();
    let backup = owner_root.join("runtime.backup");
    let _ = tokio::fs::remove_dir_all(&staging).await;

    if let Err(error) = materialize_staging(owner, bundle, &core_path, &staging).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error);
    }

    let logs = owner_paths.logs_dir();
    tokio::fs::create_dir_all(&logs)
        .await
        .map_err(|error| invalid_asset(format!("failed to create owner log directory: {error}")))?;
    set_private_directory_permissions(&logs).await?;
    let log_config = WriterConfig {
        directory: logs.to_string_lossy().into_owned(),
        ..Default::default()
    };

    Ok(StagedRuntime {
        clash_config: ClashConfig {
            core_config: CoreConfig {
                core_path: core_path.to_string_lossy().into_owned(),
                core_ipc_path: mihomo_ipc_path(&owner.identity),
                config_path: runtime.join("config.yaml").to_string_lossy().into_owned(),
                config_dir: runtime.to_string_lossy().into_owned(),
            },
            log_config,
        },
        staging,
        runtime,
        backup,
    })
}

#[cfg(all(test, unix))]
async fn prepare_runtime(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
) -> Result<PreparedRuntime, ServiceError> {
    stage_runtime(owner, bundle).await?.activate().await
}

async fn materialize_staging(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
    core_path: &Path,
    staging: &Path,
) -> Result<(), ServiceError> {
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(|error| invalid_asset(format!("failed to create runtime staging: {error}")))?;
    set_private_directory_permissions(staging).await?;

    let app_bundle_root = application_bundle_root(core_path);
    for asset in &bundle.assets {
        let source = validate_source(owner, app_bundle_root.as_deref(), &asset.source)?;
        let destination = validate_destination(&asset.destination)?;
        let target = staging.join(destination);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                invalid_asset(format!("failed to create runtime asset directory: {error}"))
            })?;
        }
        tokio::fs::copy(&source, &target).await.map_err(|error| {
            invalid_asset(format!("failed to copy runtime asset {source:?}: {error}"))
        })?;
    }

    let config_path = staging.join("config.yaml");
    let mut config = tokio::fs::File::create(&config_path)
        .await
        .map_err(|error| invalid_asset(format!("failed to create runtime config: {error}")))?;
    config
        .write_all(bundle.yaml.as_bytes())
        .await
        .map_err(|error| invalid_asset(format!("failed to write runtime config: {error}")))?;
    config
        .sync_all()
        .await
        .map_err(|error| invalid_asset(format!("failed to sync runtime config: {error}")))?;
    Ok(())
}

fn validate_core_path(
    owner: &AuthenticatedOwner,
    core_path: &str,
) -> Result<PathBuf, ServiceError> {
    let requested = Path::new(core_path);
    let canonical = canonical_regular_file(requested, "core")?;

    #[cfg(target_os = "macos")]
    {
        let home_applications = owner.app_data_root.ancestors().find_map(|path| {
            path.file_name()
                .is_some_and(|name| name == "Library")
                .then(|| path.parent().map(|home| home.join("Applications")))
                .flatten()
        });
        let allowed = cfg!(feature = "test")
            || canonical.starts_with("/Applications")
            || home_applications
                .as_ref()
                .is_some_and(|root| canonical.starts_with(root));
        if !allowed {
            return Err(ServiceError::new(
                ServiceErrorCode::InvalidInstallLocation,
                "macOS core path is outside an allowed Applications directory",
            ));
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = owner;

    Ok(canonical)
}

fn validate_source(
    owner: &AuthenticatedOwner,
    app_bundle_root: Option<&Path>,
    source: &str,
) -> Result<PathBuf, ServiceError> {
    let requested = Path::new(source);
    let canonical = canonical_regular_file(requested, "runtime asset")?;
    if canonical != requested {
        return Err(invalid_asset(
            "runtime asset path contains a symlink or non-canonical component",
        ));
    }
    if !canonical.starts_with(&owner.app_data_root)
        && !app_bundle_root.is_some_and(|root| canonical.starts_with(root))
    {
        return Err(invalid_asset(
            "runtime asset is outside the authenticated application roots",
        ));
    }
    Ok(canonical)
}

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf, ServiceError> {
    if !path.is_absolute() {
        return Err(invalid_asset(format!("{label} path must be absolute")));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| invalid_asset(format!("{label} is unavailable: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_asset(format!("{label} must be an ordinary file")));
    }
    std::fs::canonicalize(path)
        .map_err(|error| invalid_asset(format!("failed to canonicalize {label}: {error}")))
}

fn validate_destination(destination: &str) -> Result<PathBuf, ServiceError> {
    let path = Path::new(destination);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_asset(
            "runtime asset destination must be a non-traversing relative path",
        ));
    }
    Ok(path.to_path_buf())
}

fn application_bundle_root(core_path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        core_path
            .ancestors()
            .find(|path| path.extension().is_some_and(|extension| extension == "app"))
            .map(Path::to_path_buf)
    }

    #[cfg(not(target_os = "macos"))]
    {
        core_path.parent().map(Path::to_path_buf)
    }
}

fn invalid_asset(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ServiceErrorCode::InvalidRuntimeAsset, message)
}

async fn set_private_directory_permissions(path: &Path) -> Result<(), ServiceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|error| {
                invalid_asset(format!(
                    "failed to secure owner directory {path:?}: {error}"
                ))
            })?;
    }

    #[cfg(windows)]
    crate::core::windows_security::secure_private_directory(path).map_err(|error| {
        invalid_asset(format!(
            "failed to secure owner directory {path:?}: {error:#}"
        ))
    })?;

    Ok(())
}

async fn prepare_owner_ipc_directory(owner: &AuthenticatedOwner) -> Result<(), ServiceError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let ipc_path = PathBuf::from(mihomo_ipc_path(&owner.identity));
        let directory = ipc_path
            .parent()
            .ok_or_else(|| invalid_asset("owner IPC path has no parent directory"))?;
        let users_directory = directory
            .parent()
            .ok_or_else(|| invalid_asset("owner IPC directory has no users root"))?;
        crate::core::unix_security::ensure_service_directory(users_directory, 0o755).map_err(
            |error| invalid_asset(format!("failed to secure IPC users directory: {error:#}")),
        )?;
        match std::fs::create_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(invalid_asset(format!(
                    "failed to create owner IPC directory: {error}"
                )));
            }
        }
        let directory = std::ffi::CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| invalid_asset("owner IPC directory contains NUL"))?;
        let fd = unsafe {
            platform_lib::open(
                directory.as_ptr(),
                platform_lib::O_DIRECTORY | platform_lib::O_NOFOLLOW | platform_lib::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(invalid_asset(format!(
                "failed to open owner IPC directory: {}",
                std::io::Error::last_os_error()
            )));
        }
        let crate::OwnerIdentity::Unix { uid, .. } = owner.identity else {
            unsafe { platform_lib::close(fd) };
            return Err(invalid_asset("Unix IPC directory requires a Unix owner"));
        };
        let mut stat = unsafe { std::mem::zeroed::<platform_lib::stat>() };
        if unsafe { platform_lib::fstat(fd, &mut stat) } != 0
            || stat.st_mode & platform_lib::S_IFMT != platform_lib::S_IFDIR
            || (stat.st_uid != 0 && stat.st_uid != uid)
        {
            unsafe { platform_lib::close(fd) };
            return Err(invalid_asset(
                "owner IPC directory has an unexpected owner or file type",
            ));
        }
        let chown_ok = unsafe { platform_lib::geteuid() } != 0
            || unsafe { platform_lib::fchown(fd, 0, 0) } == 0;
        let chmod_ok = unsafe { platform_lib::fchmod(fd, 0o700 as platform_lib::mode_t) } == 0;
        let os_error = (!chown_ok || !chmod_ok).then(std::io::Error::last_os_error);
        unsafe { platform_lib::close(fd) };
        if let Some(error) = os_error {
            return Err(invalid_asset(format!(
                "failed to secure owner IPC directory: {error}"
            )));
        }
    }

    #[cfg(windows)]
    let _ = owner;

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::{prepare_runtime, stage_runtime};
    use crate::core::auth::AuthenticatedOwner;
    use crate::{OwnerIdentity, RuntimeAsset, RuntimeBundle, ServiceErrorCode};
    use serial_test::serial;

    fn test_owner(app_data_root: std::path::PathBuf) -> AuthenticatedOwner {
        let uid = unsafe { platform_lib::geteuid() };
        let gid = unsafe { platform_lib::getegid() };
        AuthenticatedOwner {
            key: uid.to_string(),
            identity: OwnerIdentity::Unix { uid, gid },
            app_data_root,
        }
    }

    #[tokio::test]
    #[serial]
    async fn materializes_yaml_and_assets_below_owner_runtime() -> anyhow::Result<()> {
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-assets-{}", std::process::id()));
        std::fs::create_dir_all(app_root.join("providers"))?;
        std::fs::write(app_root.join("providers/source.yaml"), b"proxies: []\n")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let bundle = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![RuntimeAsset {
                source: owner
                    .app_data_root
                    .join("providers/source.yaml")
                    .to_string_lossy()
                    .into_owned(),
                destination: "providers/copied.yaml".to_string(),
            }],
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };

        let prepared = prepare_runtime(&owner, &bundle).await?;

        assert_eq!(
            std::fs::read_to_string(&prepared.clash_config.core_config.config_path)?,
            "mode: rule\n"
        );
        assert_eq!(
            std::fs::read(
                std::path::Path::new(&prepared.clash_config.core_config.config_dir)
                    .join("providers/copied.yaml")
            )?,
            b"proxies: []\n"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn staged_assets_survive_legacy_source_cleanup() -> anyhow::Result<()> {
        let app_root = std::env::temp_dir().join(format!(
            "service-runtime-cleanup-order-{}",
            std::process::id()
        ));
        let source = app_root.join("legacy-provider.yaml");
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(&source, b"proxies: []\n")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let canonical_source = owner.app_data_root.join("legacy-provider.yaml");
        let bundle = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![RuntimeAsset {
                source: canonical_source.to_string_lossy().into_owned(),
                destination: "providers/copied.yaml".to_string(),
            }],
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };

        let staged = stage_runtime(&owner, &bundle).await?;
        std::fs::remove_file(source)?;
        let prepared = staged.activate().await?;

        assert_eq!(
            std::fs::read(
                std::path::Path::new(&prepared.clash_config.core_config.config_dir)
                    .join("providers/copied.yaml")
            )?,
            b"proxies: []\n"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn rejects_traversal_without_replacing_existing_runtime() -> anyhow::Result<()> {
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-traversal-{}", std::process::id()));
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(app_root.join("asset"), b"safe")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let valid = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![],
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };
        let prepared = prepare_runtime(&owner, &valid).await?;
        let invalid = RuntimeBundle {
            yaml: "mode: global\n".to_string(),
            assets: vec![RuntimeAsset {
                source: owner
                    .app_data_root
                    .join("asset")
                    .to_string_lossy()
                    .into_owned(),
                destination: "../escape".to_string(),
            }],
            core_path: valid.core_path,
        };

        let error = prepare_runtime(&owner, &invalid)
            .await
            .expect_err("traversal must fail");

        assert_eq!(error.code, ServiceErrorCode::InvalidRuntimeAsset);
        assert_eq!(
            std::fs::read_to_string(prepared.clash_config.core_config.config_path)?,
            "mode: rule\n"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }
}
