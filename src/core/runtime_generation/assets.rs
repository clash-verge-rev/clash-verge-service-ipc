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

/// Whether a Windows failure is one that another attempt could get past.
///
/// The first four are what a handle held on a file or directory produces while it is being removed.
/// The `MoveFileEx` codes were added when this started being used for *replacing* a file and not
/// only for removing a directory: a replacement whose target is in the delete-pending state a held
/// handle leaves behind fails with one of them rather than with a sharing violation.
///
/// `ERROR_USER_MAPPED_FILE` is the one a memory-mapped file gives, and retrying it will not help
/// while the mapping lives — it is here so the wait is bounded and the caller reaches its "give up
/// and restart the core" path rather than treating it as a hard failure.
///
/// Which of these actually occur cannot be established on a machine that cannot build for Windows;
/// the set errs towards retrying, and the cost of a wrong guess is a bounded delay before the
/// caller falls back.
#[cfg(windows)]
pub(super) fn runtime_cleanup_retry_delay(
    error: &std::io::Error,
    retry_index: usize,
) -> Option<Duration> {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DELETE_PENDING, ERROR_DIR_NOT_EMPTY, ERROR_SHARING_VIOLATION,
        ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
        ERROR_UNABLE_TO_REMOVE_REPLACED, ERROR_USER_MAPPED_FILE,
    };

    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_ACCESS_DENIED as i32
                || code == ERROR_SHARING_VIOLATION as i32
                || code == ERROR_DIR_NOT_EMPTY as i32
                || code == ERROR_DELETE_PENDING as i32
                || code == ERROR_UNABLE_TO_REMOVE_REPLACED as i32
                || code == ERROR_UNABLE_TO_MOVE_REPLACEMENT as i32
                || code == ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32
                || code == ERROR_USER_MAPPED_FILE as i32
    )
    .then(|| WINDOWS_RUNTIME_RETRY_DELAYS.get(retry_index).copied())
    .flatten()
}

#[cfg(not(windows))]
pub(super) fn runtime_cleanup_retry_delay(
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
    let mut manifest = super::staging::RuntimeManifest::default();
    let mut asset_destinations = std::collections::BTreeSet::new();
    for asset in &bundle.assets {
        let source = validate_source(owner, app_bundle_root.as_deref(), &asset.source)?;
        let key = destination_key(&validate_destination(&asset.destination)?)?;
        let target = runtime.join(&key);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                invalid_asset(format!("failed to create runtime asset directory: {error}"))
            })?;
        }
        let metadata = tokio::fs::metadata(&source).await.map_err(|error| {
            invalid_asset(format!(
                "failed to inspect runtime asset {source:?}: {error}"
            ))
        })?;
        tokio::fs::copy(&source, &target).await.map_err(|error| {
            invalid_asset(format!("failed to copy runtime asset {source:?}: {error}"))
        })?;
        if !asset_destinations.insert(key.clone()) {
            return Err(invalid_asset(format!(
                "runtime destination {key:?} is declared as a copied asset twice"
            )));
        }
        let identity = super::staging::SourceIdentity {
            source: source.to_string_lossy().into_owned(),
            len: metadata.len(),
            mtime_ns: super::staging::modified_nanos(&metadata),
        };
        // The identity was read before the copy, so a source rewritten in between would be recorded
        // as content that is not on disk — and then skipped by every staging that follows, because
        // the record matches the source it still names. Staging re-checks for exactly this; the
        // start path has to as well, now that it writes the record. Omitting the entry is enough
        // here: a destination the manifest does not mention is one nothing can be proven about, and
        // the next staging simply copies it again.
        if super::staging::source_identity_changed(&identity.source, &identity).await {
            tracing::warn!(
                destination = %key,
                "Runtime asset changed while being copied; not recording what it was copied from"
            );
        } else {
            manifest.assets.insert(key, identity);
        }
    }
    for provider in
        super::staging::declared_remote_providers(&bundle.remote_providers, &asset_destinations)?
    {
        manifest
            .remote_providers
            .insert(provider.destination, provider.url);
    }

    let config_path = runtime.join(RUNTIME_CONFIG_FILE_NAME);
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

    // Record what was just written, so the first staging into this generation can tell an
    // unchanged asset from a changed one and a reusable download cache from a stale one. Without
    // it that first staging re-copies every geo database and discards every cache the core has
    // fetched since start — correct, but paying the whole cost staging exists to avoid.
    //
    // A manifest that cannot be written is not a reason to refuse to start a core. Its absence is
    // already a defined state: the next staging proves nothing and does the slow thing.
    let manifest_path = runtime.join(super::staging::MANIFEST_FILE_NAME);
    match serde_json::to_vec(&manifest) {
        Ok(encoded) => {
            if let Err(error) = super::staging::write_atomically(&manifest_path, &encoded).await {
                tracing::warn!(
                    error = %error,
                    "Started without a runtime manifest; the first configuration change will re-copy every asset"
                );
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "Could not encode the initial runtime manifest")
        }
    }
    Ok(())
}

pub(super) fn validate_core_path(
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
            || is_permitted_macos_core_location(&canonical, home_applications.as_deref());
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

/// Where a macOS core binary is allowed to live.
///
/// This is the whole security value of the macOS arm of [`validate_core_path`]: a core outside an
/// Applications directory is one the owner could have written wherever it liked, without the
/// protections macOS gives an installed application.
///
/// It takes plain paths rather than an owner so the rule can be checked without a filesystem, a
/// running service, or a particular build. The `test` escape hatch stays at the call site — this
/// function always answers the production question.
#[cfg(target_os = "macos")]
fn is_permitted_macos_core_location(canonical: &Path, home_applications: Option<&Path>) -> bool {
    canonical.starts_with("/Applications")
        || home_applications.is_some_and(|root| canonical.starts_with(root))
}

pub(super) fn validate_source(
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

pub(super) fn validate_destination(destination: &str) -> Result<PathBuf, ServiceError> {
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

/// The file a runtime generation's configuration always lives in.
pub(super) const RUNTIME_CONFIG_FILE_NAME: &str = "config.yaml";
/// The infix every file staging writes as a temporary carries.
pub(super) const STAGING_TEMP_INFIX: &str = ".staging-";

/// Reduce a validated destination to the single string form used as its key everywhere.
///
/// Built by reassembling the validated components, never by rewriting separators in the client's
/// own string. That distinction is the whole point: `validate_destination` accepts a destination
/// because every component is `Normal`, and on Unix a backslash is an ordinary character *inside*
/// a component — so rewriting `\` to `/` afterwards turns a filename the validator accepted back
/// into a path, and `..\..\etc\thing` becomes a traversal the validator already approved.
///
/// Also refuses the names the generation directory owns. A bundle that could claim `config.yaml`
/// could have its configuration deleted by the next staging's housekeeping sweep; one that could
/// claim the manifest could rewrite the record staging trusts; one that could claim a staging
/// temporary could be clobbered mid-write.
pub(super) fn destination_key(destination: &Path) -> Result<String, ServiceError> {
    let mut parts = Vec::new();
    for component in destination.components() {
        let Component::Normal(part) = component else {
            return Err(invalid_asset(
                "runtime asset destination must be a non-traversing relative path",
            ));
        };
        let part = part.to_string_lossy();
        if is_staging_temporary(&part) {
            return Err(invalid_asset(format!(
                "runtime asset destination {part:?} is reserved for staging temporaries"
            )));
        }
        parts.push(part.into_owned());
    }
    match parts.as_slice() {
        [] => Err(invalid_asset("runtime asset destination is empty")),
        // Compared without regard to case, because most Windows filesystems are: `Config.yaml`
        // would be a different manifest key naming the same file, and the housekeeping sweep — which
        // runs after the configuration is committed — would then delete the live configuration.
        [only]
            if only.eq_ignore_ascii_case(RUNTIME_CONFIG_FILE_NAME)
                || only.eq_ignore_ascii_case(super::staging::MANIFEST_FILE_NAME) =>
        {
            Err(invalid_asset(format!(
                "runtime asset destination {only:?} is owned by the runtime generation"
            )))
        }
        _ => Ok(parts.join("/")),
    }
}

/// Whether a name is shaped like one of staging's own temporaries, `.{name}.staging-{pid}-{seq}`.
///
/// Matched on the shape rather than on the infix alone: a provider legitimately called
/// `company.staging-prod.yaml` is not a temporary, and refusing it would keep a configuration the
/// core is willing to load from ever being materialised.
fn is_staging_temporary(name: &str) -> bool {
    let Some(tail) = name
        .strip_prefix('.')
        .and_then(|rest| rest.rsplit_once(STAGING_TEMP_INFIX))
        .map(|(_, tail)| tail)
    else {
        return false;
    };
    matches!(tail.split_once('-'), Some((pid, sequence))
        if !pid.is_empty()
            && !sequence.is_empty()
            && pid.bytes().all(|byte| byte.is_ascii_digit())
            && sequence.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Resolve a recorded destination inside `generation`, refusing anything that would leave it.
///
/// Applied to keys read back from the manifest as well as to keys from the bundle: the manifest is
/// a file, and a file is a thing that can be wrong.
pub(super) fn resolve_in_generation(
    generation: &Path,
    destination: &str,
) -> Result<PathBuf, ServiceError> {
    let key = destination_key(&validate_destination(destination)?)?;
    Ok(generation.join(key))
}

pub(super) fn application_bundle_root(core_path: &Path) -> Option<PathBuf> {
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

pub(super) fn invalid_asset(message: impl Into<String>) -> ServiceError {
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
        if !owner_ipc_directory_is_usable(
            inspected,
            stat.st_mode,
            stat.st_uid,
            uid,
            test_process_owned,
        ) {
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

/// Whether an already-open directory is one the service may use as an owner's IPC directory.
///
/// Root owns it when the installer made it; the owner owns it after a handover. Anything else is
/// a directory somebody else can write to, and the core's socket must not be created inside one.
///
/// The facts arrive as plain values so the rule can be checked without an `fstat`, a real
/// directory, or root. `process_owned` is the caller's `test`-build concession — no test runs as
/// root — and this function stays honest about the rest regardless of build.
#[cfg(unix)]
fn owner_ipc_directory_is_usable(
    inspected: bool,
    mode: platform_lib::mode_t,
    directory_uid: u32,
    owner_uid: u32,
    process_owned: bool,
) -> bool {
    inspected
        && mode & platform_lib::S_IFMT == platform_lib::S_IFDIR
        && (directory_uid == 0 || directory_uid == owner_uid || process_owned)
}

#[cfg(all(test, target_os = "macos"))]
mod macos_core_location_tests {
    use super::is_permitted_macos_core_location;
    use std::path::Path;

    #[test]
    fn accepts_a_system_applications_core() {
        assert!(is_permitted_macos_core_location(
            Path::new("/Applications/Clash Verge.app/Contents/MacOS/verge-mihomo"),
            None,
        ));
    }

    #[test]
    fn accepts_a_core_under_the_owners_own_applications() {
        assert!(is_permitted_macos_core_location(
            Path::new("/Users/someone/Applications/Clash Verge.app/core"),
            Some(Path::new("/Users/someone/Applications")),
        ));
    }

    #[test]
    fn rejects_a_core_the_owner_could_have_dropped_anywhere() {
        assert!(!is_permitted_macos_core_location(
            Path::new("/tmp/verge-mihomo"),
            Some(Path::new("/Users/someone/Applications")),
        ));
        assert!(!is_permitted_macos_core_location(
            Path::new("/Users/someone/Downloads/verge-mihomo"),
            Some(Path::new("/Users/someone/Applications")),
        ));
    }

    #[test]
    fn rejects_another_users_applications_directory() {
        assert!(!is_permitted_macos_core_location(
            Path::new("/Users/someone-else/Applications/evil.app/core"),
            Some(Path::new("/Users/someone/Applications")),
        ));
    }

    #[test]
    fn does_not_accept_a_prefix_that_merely_looks_alike() {
        assert!(!is_permitted_macos_core_location(
            Path::new("/Applications-elsewhere/verge-mihomo"),
            None,
        ));
    }
}

#[cfg(all(test, unix))]
mod owner_ipc_directory_tests {
    use super::owner_ipc_directory_is_usable;

    const DIR: platform_lib::mode_t = platform_lib::S_IFDIR | 0o700;
    const FILE: platform_lib::mode_t = platform_lib::S_IFREG | 0o600;

    #[test]
    fn accepts_a_root_owned_directory() {
        assert!(owner_ipc_directory_is_usable(true, DIR, 0, 501, false));
    }

    #[test]
    fn accepts_a_directory_already_handed_to_the_owner() {
        assert!(owner_ipc_directory_is_usable(true, DIR, 501, 501, false));
    }

    #[test]
    fn rejects_a_directory_belonging_to_somebody_else() {
        assert!(!owner_ipc_directory_is_usable(true, DIR, 502, 501, false));
    }

    #[test]
    fn accepts_somebody_elses_directory_only_when_the_caller_allows_it() {
        assert!(owner_ipc_directory_is_usable(true, DIR, 502, 501, true));
    }

    #[test]
    fn rejects_anything_that_is_not_a_directory() {
        assert!(!owner_ipc_directory_is_usable(true, FILE, 0, 501, false));
        assert!(!owner_ipc_directory_is_usable(true, FILE, 501, 501, true));
    }

    #[test]
    fn rejects_a_directory_it_could_not_inspect() {
        assert!(!owner_ipc_directory_is_usable(false, DIR, 0, 501, false));
        assert!(!owner_ipc_directory_is_usable(false, DIR, 501, 501, true));
    }
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
        // `ensure_service_directory` creates one level and then hardens it, so it needs its parent
        // to exist. In production the installer provides that; the integration tests get it as a
        // side effect of starting the IPC server. These tests start nothing, so they provide it —
        // relaxing the hardening helper into `create_dir_all` instead would create the
        // intermediate directories of a privileged path unhardened.
        if let Some(root) = std::path::Path::new(crate::IPC_PATH).parent() {
            std::fs::create_dir_all(root).expect("test IPC root must be creatable");
        }

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
