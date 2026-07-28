use crate::core::auth::{AuthenticatedOwner, ServiceError};
use crate::core::paths::ensure_owner_state_directory;
use crate::{
    ClashConfig, CoreConfig, RuntimeBundle, ServiceErrorCode, WriterConfig, mihomo_ipc_path,
};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// The one directory an owner's core is ever started against.
///
/// A fixed name, not a per-start one: the core keeps its own state here and that state is the
/// point. `snapshot_stale_runtime_directories` already treats this name as service-owned, so an
/// installation still holding per-start directories retires them on its next start.
const RUNTIME_GENERATION_DIRECTORY_NAME: &str = "runtime";

#[cfg(windows)]
const WINDOWS_RUNTIME_RETRY_DELAYS: [Duration; 6] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// The owner's generation, refreshed and ready for a core to be started against it.
///
/// There is nothing here to undo. The generation is durable — one per owner, reused for the
/// life of the installation — so a start that fails leaves it exactly as this left it, which is
/// a configuration a core can be started against. It used to be a directory minted per start,
/// and then every way of failing had to remember to delete it before it leaked; the state
/// machine that tracked whether deleting was still safe went with it.
///
/// What survives is the sweep of the *old* per-start directories, which is how an installation
/// that predates this migrates: they are collected once the start commits, and removed then.
#[derive(Debug)]
pub(crate) struct PreparedRuntime {
    clash_config: ClashConfig,
    runtime: PathBuf,
    stale_runtime_paths: Vec<PathBuf>,
    plan: super::staging::StagePlan,
    yaml: String,
}

impl PreparedRuntime {
    pub(crate) fn clash_config(&self) -> &ClashConfig {
        &self.clash_config
    }

    /// Write the planned changes into the generation.
    ///
    /// Separate from planning them because the generation is now the *same* directory the
    /// outgoing core is running in, and this rewrites it. Planning happens before anything is
    /// stopped, so a bundle the service will not accept still costs no outage; writing happens
    /// after, so nothing is replaced under a core that still has it open. On Windows that is the
    /// difference between a restart and a failed one — a geo database the running core has
    /// memory-mapped cannot be replaced at all while it lives.
    pub(crate) async fn materialize(&self) -> Result<(), ServiceError> {
        materialize_plan(&self.runtime, &self.plan, &self.yaml).await
    }

    /// The core started against this generation: retire whatever the old layout left behind.
    pub(crate) fn commit(mut self) {
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

/// Make sure the owner's generation directory exists and is private to the service.
///
/// The owner has one generation and keeps it. It used to get a fresh
/// `runtime.generation-{pid}-{nanos}-{seq}` on every start, which is why a restart lost the
/// node the user had picked: the core keeps its own state in the directory it is started
/// against — `cache.db`, holding the `store-selected` choices and the fake-ip leases — and a
/// directory nothing had ever run in has none of it. Every `select` group then fell back to
/// the first entry in its `proxies:` list.
///
/// Reusing it is also what the service already did from the other direction: restoring an
/// owner's core from its desired state starts it against the recorded `config_dir`, which is
/// this same directory. Only an app-driven start minted a new one, so the two paths disagreed
/// about whether a generation outlives the core that ran in it.
async fn ensure_runtime_generation(owner_root: &Path) -> Result<PathBuf, ServiceError> {
    let runtime = owner_root.join(RUNTIME_GENERATION_DIRECTORY_NAME);
    tokio::fs::create_dir_all(&runtime).await.map_err(|error| {
        invalid_asset(format!(
            "failed to create runtime generation {runtime:?}: {error}"
        ))
    })?;
    set_private_directory_permissions(&runtime).await?;
    Ok(runtime)
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

    let runtime = ensure_runtime_generation(owner_root).await?;
    let mut prepared = PreparedRuntime {
        clash_config: ClashConfig {
            core_config: CoreConfig {
                core_path: core_path.to_string_lossy().into_owned(),
                core_ipc_path: mihomo_ipc_path(&owner.identity),
                config_path: runtime
                    .join(RUNTIME_CONFIG_FILE_NAME)
                    .to_string_lossy()
                    .into_owned(),
                config_dir: runtime.to_string_lossy().into_owned(),
            },
            log_config,
        },
        runtime: runtime.clone(),
        stale_runtime_paths: Vec::new(),
        plan: plan_runtime_refresh(owner, bundle, &core_path, &runtime).await?,
        yaml: bundle.yaml.clone(),
    };
    prepared.stale_runtime_paths = snapshot_stale_runtime_directories(owner_root, &runtime).await;
    Ok(prepared)
}

/// Work out what would make the owner's generation match `bundle`. Writes nothing.
///
/// The same plan `stage_runtime` uses, for the same reasons: an asset is re-copied only when its
/// source changed, a remote provider's download cache is discarded exactly when its url changed,
/// and nothing is deleted that the previous manifest did not record. That last rule is what
/// carries the core's own files — `cache.db` above all — across a restart, and it is the point of
/// running the plan here rather than copying blindly into an empty directory.
///
/// A first start finds no manifest, which the plan reads as "nothing can be proven": it copies
/// every asset and skips nothing, which is what building an empty generation always did.
async fn plan_runtime_refresh(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
    core_path: &Path,
    runtime: &Path,
) -> Result<super::staging::StagePlan, ServiceError> {
    let gathered = gather_bundle(owner, bundle, core_path).await?;
    // A manifest that cannot be read is not a reason to refuse to start a core. It is the same
    // state as no manifest at all — nothing can be proven — and the plan already answers that by
    // copying everything and sweeping nothing. Staging can afford to decline and let the caller
    // restart; a start has nowhere to fall back to, and the caller's fallback for a declined
    // staging *is* a start. Refusing here turns a corrupt bookkeeping file into no core at all,
    // with every retry hitting the same file.
    let previous = super::staging::read_manifest(runtime)
        .await
        .unwrap_or_else(|detail| {
            tracing::warn!(
                detail = %detail,
                "Rebuilding the runtime generation from nothing: its manifest could not be read"
            );
            super::staging::RuntimeManifest::default()
        });
    Ok(super::staging::plan_stage(
        &previous,
        &gathered.sources,
        &gathered.remote,
    ))
}

/// Apply a plan to the generation.
///
/// The order is the plan's own, and the same one staging applies: stale caches before the copies,
/// `config.yaml` committed last and atomically. Here it is not about what a live core might see —
/// the caller has already stopped it — but about what is on disk if this fails part way. A failure
/// before the commit leaves the generation still describing the configuration the previous core
/// ran, which is what the desired-state restore would start again.
async fn materialize_plan(
    runtime: &Path,
    plan: &super::staging::StagePlan,
    yaml: &str,
) -> Result<(), ServiceError> {
    // The plan is shared, so what gets recorded is corrected here: a copy whose source moved
    // under it loses its entry rather than the whole start losing.
    let mut manifest = plan.manifest.clone();
    for destination in &plan.required_deletes {
        let target = resolve_in_generation(runtime, destination)?;
        super::staging::remove_staged_file(&target)
            .await
            .map_err(|error| {
                invalid_asset(format!(
                    "failed to discard the stale cache {destination}: {error}"
                ))
            })?;
    }

    for copy in &plan.copies {
        let target = resolve_in_generation(runtime, &copy.destination)?;
        super::staging::copy_staged_file(&copy.source, &target)
            .await
            .map_err(|error| {
                invalid_asset(format!(
                    "failed to write the runtime asset {}: {error}",
                    copy.destination
                ))
            })?;
        // Re-stat for the reason staging does: an identity read before the copy would name
        // content that is no longer there, and every later staging would then skip it because
        // the record still matches the source. One stat against a warm file beats pinning the
        // wrong bytes for the life of the generation.
        //
        // Staging turns this into a restart; a start cannot, and does not need to. Dropping the
        // entry says the same thing the manifest says about any destination it omits — nothing is
        // proven about it — so the next staging copies it again. A geo database being rewritten
        // while the core starts must not be a start failure.
        if super::staging::source_identity_changed(&copy.source, &copy.identity).await {
            tracing::warn!(
                destination = %copy.destination,
                "Runtime asset changed while being copied; not recording what it was copied from"
            );
            manifest.assets.remove(&copy.destination);
        }
    }

    let config_path = runtime.join(RUNTIME_CONFIG_FILE_NAME);
    super::staging::commit_staged_config(runtime, &config_path, yaml, &manifest)
        .await
        .map_err(|error| {
            invalid_asset(format!(
                "failed to commit the runtime configuration: {error}"
            ))
        })?;

    // Housekeeping: files a previous bundle declared and this one does not. Failing to remove one
    // changes nothing the core observes, so it is logged rather than returned — the configuration
    // is already committed and the core is about to be started against it.
    for destination in &plan.hygiene_deletes {
        match resolve_in_generation(runtime, destination) {
            Ok(target) => {
                if let Err(error) = super::staging::remove_staged_file(&target).await {
                    tracing::warn!(
                        destination = %destination,
                        error = %error,
                        "Left an undeclared file behind in the runtime generation"
                    );
                }
            }
            Err(error) => tracing::warn!(
                destination = %destination,
                error = %error,
                "Refused to sweep a recorded destination that no longer validates"
            ),
        }
    }

    tracing::info!(
        copied = plan.copies.len(),
        skipped = plan.skipped.len(),
        discarded = plan.required_deletes.len(),
        "Prepared the runtime generation"
    );
    Ok(())
}

/// Everything a bundle declares, validated and stat'd, ready for the plan.
///
/// Both verbs need exactly this and used to compute it separately. The duplication was the kind
/// that drifts silently: only one of the two rejected a destination declared twice, and only one
/// of them re-checked a source that changed under the copy.
pub(super) struct GatheredBundle {
    pub sources: Vec<super::staging::AssetSource>,
    pub remote: Vec<crate::RemoteProvider>,
}

pub(super) async fn gather_bundle(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
    core_path: &Path,
) -> Result<GatheredBundle, ServiceError> {
    let app_bundle_root = application_bundle_root(core_path);
    let mut sources = Vec::with_capacity(bundle.assets.len());
    let mut asset_keys = std::collections::BTreeSet::new();
    for asset in &bundle.assets {
        let source = validate_source(owner, app_bundle_root.as_deref(), &asset.source)?;
        let destination = destination_key(&validate_destination(&asset.destination)?)?;
        let metadata = tokio::fs::metadata(&source).await.map_err(|error| {
            invalid_asset(format!(
                "failed to inspect runtime asset {source:?}: {error}"
            ))
        })?;
        if !asset_keys.insert(destination.clone()) {
            return Err(invalid_asset(format!(
                "runtime destination {destination:?} is declared as a copied asset twice"
            )));
        }
        sources.push(super::staging::AssetSource {
            asset: crate::RuntimeAsset {
                source: source.to_string_lossy().into_owned(),
                destination,
            },
            len: metadata.len(),
            mtime_ns: super::staging::modified_nanos(&metadata),
        });
    }
    let remote = super::staging::declared_remote_providers(&bundle.remote_providers, &asset_keys)?;
    Ok(GatheredBundle { sources, remote })
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
    use super::{PreparedRuntime, prepare_runtime};
    use crate::core::auth::{AuthenticatedOwner, ServiceError};
    use crate::{OwnerIdentity, RuntimeAsset, RuntimeBundle, ServiceErrorCode};
    use serial_test::serial;
    use std::path::PathBuf;

    /// Plan and then write, which is the pair the start path performs.
    ///
    /// They are separate in production because the write happens only once the previous core has
    /// been stopped; a test with no core running can do both at once.
    async fn prepare_and_materialize(
        owner: &AuthenticatedOwner,
        bundle: &RuntimeBundle,
    ) -> Result<PreparedRuntime, ServiceError> {
        let prepared = prepare_runtime(owner, bundle).await?;
        prepared.materialize().await?;
        Ok(prepared)
    }

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

        let prepared = prepare_and_materialize(&owner, &bundle).await?;

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

        let prepared = prepare_and_materialize(&owner, &bundle).await?;
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
        let prepared = prepare_and_materialize(&owner, &valid).await?;
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
        // The generation is the owner's one durable directory, so "did not replace" is a
        // statement about its contents rather than about how many directories exist: a rejected
        // bundle must leave the configuration a core could still be started against.
        assert_eq!(
            std::fs::read_to_string(&prepared.clash_config.core_config.config_path)?,
            "mode: rule\n"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    /// What the core writes for itself, which the service must never take away from it.
    fn core_owned_state(prepared: &PreparedRuntime) -> PathBuf {
        PathBuf::from(&prepared.clash_config.core_config.config_dir).join("cache.db")
    }

    #[tokio::test]
    #[serial]
    async fn a_restart_keeps_the_state_the_core_wrote_for_itself() -> anyhow::Result<()> {
        // The regression this pins: mihomo stores the node the user picked in `cache.db`, in the
        // directory it was started against. A start that minted a fresh directory handed the core
        // an empty one, `store-selected` restored nothing, and every `select` group fell back to
        // the first entry in its `proxies:` list.
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-cachedb-{}", std::process::id()));
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let bundle = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![],
            remote_providers: Vec::new(),
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };

        let first = prepare_and_materialize(&owner, &bundle).await?;
        std::fs::write(core_owned_state(&first), b"the node the user picked")?;

        let second = prepare_and_materialize(&owner, &bundle).await?;

        assert_eq!(
            second.clash_config.core_config.config_dir, first.clash_config.core_config.config_dir,
            "an owner has one generation, so a restart is started against the same directory"
        );
        assert_eq!(
            std::fs::read(core_owned_state(&second))?,
            b"the node the user picked",
            "a restart must not take the core's own state away from it"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn a_rejected_bundle_keeps_the_state_the_core_wrote_for_itself() -> anyhow::Result<()> {
        // Rejecting used to mean deleting the whole directory, which was safe only because the
        // directory was one this start had just created. Now it is the owner's, and deleting it
        // would throw away the running core's database to punish a bad bundle.
        let app_root = std::env::temp_dir().join(format!(
            "service-runtime-cachedb-reject-{}",
            std::process::id()
        ));
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
        let prepared = prepare_and_materialize(&owner, &valid).await?;
        std::fs::write(core_owned_state(&prepared), b"the node the user picked")?;

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
        prepare_runtime(&owner, &invalid)
            .await
            .expect_err("traversal must fail");

        assert_eq!(
            std::fs::read(core_owned_state(&prepared))?,
            b"the node the user picked"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn planning_a_start_writes_nothing_into_the_generation() -> anyhow::Result<()> {
        // The generation is the directory the *outgoing* core is still running in when a start is
        // planned, so planning may not touch it. Only `materialize`, which the caller runs once
        // that core has been stopped, is allowed to write. On Windows the difference is whether a
        // restart works at all: a geo database the running core has memory-mapped cannot be
        // replaced while it lives.
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-planonly-{}", std::process::id()));
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(app_root.join("asset"), b"new asset")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let core_path = app_root.join("mihomo").to_string_lossy().into_owned();
        let running = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![],
            remote_providers: Vec::new(),
            core_path: core_path.clone(),
        };
        let prepared = prepare_and_materialize(&owner, &running).await?;

        let candidate = RuntimeBundle {
            yaml: "mode: global\n".to_string(),
            assets: vec![RuntimeAsset {
                source: owner
                    .app_data_root
                    .join("asset")
                    .to_string_lossy()
                    .into_owned(),
                destination: "providers/new.yaml".to_string(),
            }],
            remote_providers: Vec::new(),
            core_path,
        };
        let planned = prepare_runtime(&owner, &candidate).await?;

        let generation = PathBuf::from(&prepared.clash_config.core_config.config_dir);
        assert_eq!(
            std::fs::read_to_string(&prepared.clash_config.core_config.config_path)?,
            "mode: rule\n",
            "planning must leave the running core's configuration alone"
        );
        assert!(
            !generation.join("providers/new.yaml").exists(),
            "planning must not copy assets into a directory a core is running in"
        );

        planned.materialize().await?;

        assert_eq!(
            std::fs::read_to_string(&prepared.clash_config.core_config.config_path)?,
            "mode: global\n"
        );
        assert!(generation.join("providers/new.yaml").exists());
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn planning_a_start_writes_nothing() -> anyhow::Result<()> {
        // The generation is the directory the outgoing core is still running in when a start is
        // being planned — `StartClash` prepares before the owner transition stops anything. So
        // planning has to be readable-only: a bundle is validated against a live core's directory
        // and only written into it once that core is gone.
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-planonly-{}", std::process::id()));
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(app_root.join("provider.yaml"), b"first\n")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let asset = owner
            .app_data_root
            .join("provider.yaml")
            .to_string_lossy()
            .into_owned();
        let running = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![RuntimeAsset {
                source: asset.clone(),
                destination: "providers/one.yaml".to_string(),
            }],
            remote_providers: Vec::new(),
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };
        let live = prepare_and_materialize(&owner, &running).await?;
        let generation = PathBuf::from(&live.clash_config.core_config.config_dir);

        std::fs::write(app_root.join("provider.yaml"), b"second\n")?;
        let next = RuntimeBundle {
            yaml: "mode: global\n".to_string(),
            ..running
        };
        let planned = prepare_runtime(&owner, &next).await?;

        assert_eq!(
            std::fs::read_to_string(&live.clash_config.core_config.config_path)?,
            "mode: rule\n",
            "planning must not replace the configuration the running core is using"
        );
        assert_eq!(
            std::fs::read(generation.join("providers/one.yaml"))?,
            b"first\n",
            "planning must not replace an asset the running core has open"
        );

        planned.materialize().await?;

        assert_eq!(
            std::fs::read_to_string(&live.clash_config.core_config.config_path)?,
            "mode: global\n"
        );
        assert_eq!(
            std::fs::read(generation.join("providers/one.yaml"))?,
            b"second\n"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn an_unreadable_manifest_rebuilds_rather_than_refusing_to_start() -> anyhow::Result<()> {
        // The manifest is bookkeeping the user never sees. Staging can decline a corrupt one and
        // let the caller restart — but the caller's fallback *is* a start, so if a start refused
        // too, a truncated JSON file would leave the machine with no core and every retry would
        // hit the same file.
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(app_root.join("provider.yaml"), b"proxies: []\n")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let bundle = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![RuntimeAsset {
                source: owner
                    .app_data_root
                    .join("provider.yaml")
                    .to_string_lossy()
                    .into_owned(),
                destination: "providers/one.yaml".to_string(),
            }],
            remote_providers: Vec::new(),
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };
        let first = prepare_and_materialize(&owner, &bundle).await?;
        let generation = PathBuf::from(&first.clash_config.core_config.config_dir);
        std::fs::write(
            generation.join(super::super::staging::MANIFEST_FILE_NAME),
            b"{ this is not json",
        )?;
        std::fs::write(core_owned_state(&first), b"the node the user picked")?;
        std::fs::remove_file(generation.join("providers/one.yaml"))?;

        let second = prepare_and_materialize(&owner, &bundle).await?;

        assert_eq!(
            std::fs::read(generation.join("providers/one.yaml"))?,
            b"proxies: []\n",
            "proving nothing means copying everything, not giving up"
        );
        assert_eq!(
            std::fs::read_to_string(&second.clash_config.core_config.config_path)?,
            "mode: rule\n"
        );
        assert_eq!(
            std::fs::read(core_owned_state(&second))?,
            b"the node the user picked",
            "rebuilding is not a reason to take the core's own state away"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn a_destination_the_manifest_omits_is_copied_again() -> anyhow::Result<()> {
        // An asset rewritten while it was being copied is dropped from the manifest rather than
        // failing the start — a geo database being updated must not leave the machine with no
        // core. This tests what that costs, which is the whole claim: a destination nothing is
        // proven about is copied again, so the omission is self-correcting.
        //
        // Driven by editing the manifest rather than by racing the copy, because the branch that
        // omits the entry only fires on a rewrite between two stats microseconds apart. The
        // omission is what it produces, and the omission is what has to be safe.
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-omitted-{}", std::process::id()));
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(app_root.join("geo.dat"), b"geo bytes")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let bundle = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![RuntimeAsset {
                source: owner
                    .app_data_root
                    .join("geo.dat")
                    .to_string_lossy()
                    .into_owned(),
                destination: "geo.dat".to_string(),
            }],
            remote_providers: Vec::new(),
            core_path: app_root.join("mihomo").to_string_lossy().into_owned(),
        };

        let first = prepare_and_materialize(&owner, &bundle).await?;
        let generation = PathBuf::from(&first.clash_config.core_config.config_dir);
        let manifest_path = generation.join(super::super::staging::MANIFEST_FILE_NAME);
        let recorded: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
        assert!(
            recorded["assets"].get("geo.dat").is_some(),
            "a copy that went through cleanly is recorded"
        );

        // What the tolerant branch leaves behind: the copy, without a record of what it was from.
        std::fs::write(&manifest_path, br#"{"assets":{},"remote_providers":{}}"#)?;
        std::fs::remove_file(generation.join("geo.dat"))?;

        prepare_and_materialize(&owner, &bundle).await?;

        assert_eq!(
            std::fs::read(generation.join("geo.dat"))?,
            b"geo bytes",
            "a destination the manifest omits must be copied again, not skipped"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn a_bundle_this_service_will_not_accept_changes_nothing() -> anyhow::Result<()> {
        // Everything is validated and stat'd before a byte moves, so a bundle that is rejected
        // half way through its asset list cannot leave the generation describing neither the old
        // configuration nor the new one.
        let app_root =
            std::env::temp_dir().join(format!("service-runtime-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&app_root)?;
        std::fs::write(app_root.join("first"), b"first asset")?;
        std::fs::write(app_root.join("mihomo"), b"mock core")?;
        let owner = test_owner(std::fs::canonicalize(&app_root)?);
        let core_path = app_root.join("mihomo").to_string_lossy().into_owned();
        let good = RuntimeBundle {
            yaml: "mode: rule\n".to_string(),
            assets: vec![],
            remote_providers: Vec::new(),
            core_path: core_path.clone(),
        };
        let prepared = prepare_and_materialize(&owner, &good).await?;

        let asset_source = owner.app_data_root.join("first");
        let half_bad = RuntimeBundle {
            yaml: "mode: global\n".to_string(),
            assets: vec![
                RuntimeAsset {
                    source: asset_source.to_string_lossy().into_owned(),
                    destination: "providers/first.yaml".to_string(),
                },
                RuntimeAsset {
                    source: asset_source.to_string_lossy().into_owned(),
                    destination: "../escape".to_string(),
                },
            ],
            remote_providers: Vec::new(),
            core_path,
        };
        prepare_runtime(&owner, &half_bad)
            .await
            .expect_err("the second asset must be rejected");

        let generation = PathBuf::from(&prepared.clash_config.core_config.config_dir);
        assert_eq!(
            std::fs::read_to_string(&prepared.clash_config.core_config.config_path)?,
            "mode: rule\n",
            "the configuration must still be the one a core could be started against"
        );
        assert!(
            !generation.join("providers/first.yaml").exists(),
            "the accepted asset must not have been written before the bundle was rejected"
        );
        std::fs::remove_dir_all(app_root)?;
        Ok(())
    }
}
