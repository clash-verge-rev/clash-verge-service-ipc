use crate::core::auth::{AuthenticatedOwner, ServiceError};
use crate::core::paths::ensure_owner_state_directory;
use crate::{
    ClashConfig, CoreConfig, RuntimeBundle, ServiceErrorCode, WriterConfig, mihomo_ipc_path,
};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt as _;

static RUNTIME_GENERATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
const WINDOWS_RUNTIME_RETRY_DELAYS: [Duration; 6] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

#[derive(Debug)]
pub(crate) struct PreparedRuntime {
    clash_config: ClashConfig,
    runtime: PathBuf,
    stale_runtime_paths: Vec<PathBuf>,
    state: PreparedRuntimeState,
}

#[derive(Clone, Copy, Debug)]
enum PreparedRuntimeState {
    Unused,
    MayBeInUse,
    Finalized,
}

impl PreparedRuntime {
    pub(crate) fn clash_config(&self) -> &ClashConfig {
        &self.clash_config
    }

    pub(crate) fn mark_may_be_in_use(&mut self) {
        self.state = PreparedRuntimeState::MayBeInUse;
    }

    pub(crate) fn is_unused(&self) -> bool {
        matches!(self.state, PreparedRuntimeState::Unused)
    }

    pub(crate) async fn discard_after_core_stopped(mut self) -> Result<(), ServiceError> {
        self.state = PreparedRuntimeState::Unused;
        match remove_runtime_directory(&self.runtime, "failed to discard prepared runtime").await {
            Ok(()) => {
                self.state = PreparedRuntimeState::Finalized;
                Ok(())
            }
            Err(error) => Err(invalid_asset(format!(
                "failed to discard prepared runtime {:?}: {error}; state={}",
                self.runtime,
                inspect_path(&self.runtime).await
            ))),
        }
    }

    pub(crate) fn commit(mut self) {
        self.state = PreparedRuntimeState::Finalized;
        let stale_paths = std::mem::take(&mut self.stale_runtime_paths);
        if stale_paths.is_empty() {
            return;
        }
        let active_runtime = self.runtime.clone();
        tokio::spawn(async move {
            cleanup_stale_runtime_directories(stale_paths, active_runtime).await;
        });
    }
}

impl Drop for PreparedRuntime {
    fn drop(&mut self) {
        match self.state {
            PreparedRuntimeState::Finalized => return,
            PreparedRuntimeState::MayBeInUse => {
                tracing::warn!(
                    runtime = ?self.runtime,
                    "Leaving uncommitted runtime generation because a core may still be using it"
                );
                return;
            }
            PreparedRuntimeState::Unused => {}
        }
        match std::fs::remove_dir_all(&self.runtime) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    runtime = ?self.runtime,
                    error = %error,
                    "Failed to discard uncommitted runtime generation"
                );
            }
        }
    }
}

async fn inspect_path(path: &Path) -> String {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => "symlink".to_owned(),
        Ok(metadata) if metadata.is_dir() => "directory".to_owned(),
        Ok(metadata) if metadata.is_file() => "file".to_owned(),
        Ok(_) => "other".to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".to_owned(),
        Err(error) => format!("inaccessible: {error}"),
    }
}

async fn snapshot_stale_runtime_directories(
    owner_root: &Path,
    active_runtime: &Path,
) -> Vec<PathBuf> {
    let mut stale_paths = Vec::new();
    let mut entries = match tokio::fs::read_dir(owner_root).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                owner_root = ?owner_root,
                error = %error,
                "Failed to enumerate stale runtime directories"
            );
            return stale_paths;
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(
                    owner_root = ?owner_root,
                    error = %error,
                    "Failed while enumerating stale runtime directories"
                );
                break;
            }
        };
        let path = entry.path();
        if path == active_runtime {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_runtime_directory = name == "runtime"
            || name == "runtime.backup"
            || name.starts_with("runtime.generation-")
            || name.starts_with("runtime.staging-");
        if !is_runtime_directory {
            continue;
        }
        stale_paths.push(path);
    }
    stale_paths
}

async fn cleanup_stale_runtime_directories(stale_paths: Vec<PathBuf>, active_runtime: PathBuf) {
    for path in stale_paths {
        if let Err(error) =
            remove_runtime_directory(&path, "failed to remove stale runtime directory").await
        {
            let state = inspect_path(&path).await;
            tracing::warn!(
                path = ?path,
                state = %state,
                error = %error,
                active_runtime = ?active_runtime,
                "Failed to remove stale runtime directory after committing new generation"
            );
        }
    }
}

async fn remove_runtime_directory(path: &Path, operation: &str) -> std::io::Result<()> {
    let mut retry_index = 0;
    loop {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                let Some(delay) = runtime_cleanup_retry_delay(&error, retry_index) else {
                    return Err(error);
                };
                retry_index += 1;
                tracing::warn!(
                    path = ?path,
                    retry = retry_index,
                    delay_ms = delay.as_millis(),
                    error = %error,
                    operation,
                    "Retrying transient Windows runtime directory cleanup failure"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(windows)]
pub(crate) fn runtime_cleanup_retry_delay(
    error: &std::io::Error,
    retry_index: usize,
) -> Option<Duration> {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DELETE_PENDING, ERROR_DIR_NOT_EMPTY, ERROR_SHARING_VIOLATION,
    };

    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_ACCESS_DENIED as i32
                || code == ERROR_SHARING_VIOLATION as i32
                || code == ERROR_DIR_NOT_EMPTY as i32
                || code == ERROR_DELETE_PENDING as i32
    )
    .then(|| WINDOWS_RUNTIME_RETRY_DELAYS.get(retry_index).copied())
    .flatten()
}

#[cfg(not(windows))]
pub(crate) fn runtime_cleanup_retry_delay(
    _error: &std::io::Error,
    _retry_index: usize,
) -> Option<Duration> {
    None
}

async fn create_runtime_generation(owner_root: &Path) -> Result<PathBuf, ServiceError> {
    const MAX_COLLISION_RETRIES: usize = 16;

    let mut last_collision = None;
    for _ in 0..MAX_COLLISION_RETRIES {
        let sequence = RUNTIME_GENERATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let suffix = format!("{}-{timestamp}-{sequence}", std::process::id());
        let runtime = owner_root.join(format!("runtime.generation-{suffix}"));
        match tokio::fs::create_dir(&runtime).await {
            Ok(()) => return Ok(runtime),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(runtime);
            }
            Err(error) => {
                return Err(invalid_asset(format!(
                    "failed to create runtime generation {runtime:?}: {error}; state={}",
                    inspect_path(&runtime).await
                )));
            }
        }
    }

    Err(invalid_asset(format!(
        "failed to allocate a unique runtime generation after {MAX_COLLISION_RETRIES} attempts; last_collision={last_collision:?}"
    )))
}

pub(crate) async fn prepare_runtime(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
) -> Result<PreparedRuntime, ServiceError> {
    let core_path = validate_core_path(owner, &bundle.core_path)?;
    let owner_paths = ensure_owner_state_directory(&owner.identity)
        .map_err(|error| invalid_asset(format!("failed to secure owner state root: {error:#}")))?;
    let owner_root = owner_paths.root();
    crate::core::maintenance::persist_owner_identity(&owner.identity, owner_root)
        .await
        .map_err(|error| invalid_asset(format!("failed to persist owner identity: {error:#}")))?;
    prepare_owner_ipc_directory(owner).await?;

    let logs = owner_paths.logs_dir();
    tokio::fs::create_dir_all(&logs)
        .await
        .map_err(|error| invalid_asset(format!("failed to create owner log directory: {error}")))?;
    set_private_directory_permissions(&logs).await?;
    let log_config = WriterConfig {
        directory: logs.to_string_lossy().into_owned(),
        ..Default::default()
    };

    let runtime = create_runtime_generation(owner_root).await?;
    let mut prepared = PreparedRuntime {
        clash_config: ClashConfig {
            core_config: CoreConfig {
                core_path: core_path.to_string_lossy().into_owned(),
                core_ipc_path: mihomo_ipc_path(&owner.identity),
                config_path: runtime.join("config.yaml").to_string_lossy().into_owned(),
                config_dir: runtime.to_string_lossy().into_owned(),
            },
            log_config,
        },
        runtime: runtime.clone(),
        stale_runtime_paths: Vec::new(),
        state: PreparedRuntimeState::Unused,
    };
    if let Err(error) = materialize_runtime(owner, bundle, &core_path, &runtime).await {
        if let Err(cleanup_error) = prepared.discard_after_core_stopped().await {
            tracing::warn!(
                runtime = ?runtime,
                error = %cleanup_error,
                "Failed to clean rejected runtime generation"
            );
        }
        return Err(error);
    }
    prepared.stale_runtime_paths = snapshot_stale_runtime_directories(owner_root, &runtime).await;
    Ok(prepared)
}

async fn materialize_runtime(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
    core_path: &Path,
    runtime: &Path,
) -> Result<(), ServiceError> {
    set_private_directory_permissions(runtime).await?;

    let app_bundle_root = application_bundle_root(core_path);
    for asset in &bundle.assets {
        let source = validate_source(owner, app_bundle_root.as_deref(), &asset.source)?;
        let destination = validate_destination(&asset.destination)?;
        let target = runtime.join(destination);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                invalid_asset(format!("failed to create runtime asset directory: {error}"))
            })?;
        }
        tokio::fs::copy(&source, &target).await.map_err(|error| {
            invalid_asset(format!("failed to copy runtime asset {source:?}: {error}"))
        })?;
    }

    let config_path = runtime.join("config.yaml");
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

pub(crate) fn validate_core_path(
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

pub(crate) fn validate_source(
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

pub(crate) fn validate_destination(destination: &str) -> Result<PathBuf, ServiceError> {
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

pub(crate) fn application_bundle_root(core_path: &Path) -> Option<PathBuf> {
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

pub(crate) fn invalid_asset(message: impl Into<String>) -> ServiceError {
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
        let inspected = unsafe { platform_lib::fstat(fd, &mut stat) } == 0;
        let effective_uid = unsafe { platform_lib::geteuid() };
        let test_process_owned = cfg!(feature = "test") && stat.st_uid == effective_uid;
        if !inspected
            || stat.st_mode & platform_lib::S_IFMT != platform_lib::S_IFDIR
            || (stat.st_uid != 0 && stat.st_uid != uid && !test_process_owned)
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

#[cfg(test)]
mod runtime_gc_tests {
    use super::{cleanup_stale_runtime_directories, snapshot_stale_runtime_directories};

    #[tokio::test]
    async fn stale_snapshot_never_collects_a_later_generation() -> anyhow::Result<()> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let owner_root = std::env::temp_dir().join(format!(
            "service-runtime-gc-snapshot-{}-{timestamp}",
            std::process::id()
        ));
        let active = owner_root.join("runtime.generation-active");
        let stale = owner_root.join("runtime.generation-stale");
        tokio::fs::create_dir_all(&active).await?;
        tokio::fs::create_dir(&stale).await?;

        let stale_paths = snapshot_stale_runtime_directories(&owner_root, &active).await;
        let later = owner_root.join("runtime.generation-later");
        tokio::fs::create_dir(&later).await?;
        cleanup_stale_runtime_directories(stale_paths, active.clone()).await;

        tokio::fs::symlink_metadata(&active).await?;
        tokio::fs::symlink_metadata(&later).await?;
        let stale_error = tokio::fs::symlink_metadata(&stale)
            .await
            .expect_err("snapshotted stale generation must be removed");
        assert_eq!(stale_error.kind(), std::io::ErrorKind::NotFound);

        let next_stale_paths = snapshot_stale_runtime_directories(&owner_root, &later).await;
        cleanup_stale_runtime_directories(next_stale_paths, later.clone()).await;
        tokio::fs::symlink_metadata(&later).await?;
        let previous_active_error = tokio::fs::symlink_metadata(&active)
            .await
            .expect_err("the next snapshot must collect the previous active generation");
        assert_eq!(previous_active_error.kind(), std::io::ErrorKind::NotFound);
        tokio::fs::remove_dir_all(owner_root).await?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::prepare_runtime;
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
            remote_providers: Vec::new(),
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
    async fn prepared_assets_survive_legacy_source_cleanup() -> anyhow::Result<()> {
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
            remote_providers: Vec::new(),
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };

        let prepared = prepare_runtime(&owner, &bundle).await?;
        std::fs::remove_file(source)?;

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
            remote_providers: Vec::new(),
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
            remote_providers: Vec::new(),
            core_path: valid.core_path,
        };

        let error = prepare_runtime(&owner, &invalid)
            .await
            .expect_err("traversal must fail");

        assert_eq!(error.code, ServiceErrorCode::InvalidRuntimeAsset);
        assert_eq!(
            std::fs::read_to_string(&prepared.clash_config.core_config.config_path)?,
            "mode: rule\n"
        );
        let runtime_root = prepared
            .runtime
            .parent()
            .expect("runtime generation must have an owner root");
        let generation_count = std::fs::read_dir(runtime_root)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("runtime.generation-")
            })
            .count();
        assert_eq!(
            generation_count, 1,
            "rejected runtime left a partial generation behind"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }
}
