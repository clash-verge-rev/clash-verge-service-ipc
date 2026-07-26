//! Making a runtime generation match a bundle without restarting the core that runs in it.
//!
//! A generation is normally written once, by `start_clash`, into a directory nothing else has
//! touched. Staging writes into a directory a core is *currently running in*, which changes what
//! is safe: files may be held open, and the core reads whatever it finds when it is told to
//! reload. Three rules follow, and the plan below exists to make them explicit rather than
//! implicit in the order of some I/O.
//!
//! **The service only deletes what it can prove it put there.** Every deletion candidate comes
//! from the previous manifest, never from listing the directory. The core creates files of its
//! own in the generation — `cache.db` at least — and a sweep driven by "whatever the bundle did
//! not declare" would delete a database the running core has open.
//!
//! **A remote provider's cache is stale exactly when its url changed.** The core will not
//! re-fetch a provider whose file already exists, so a cache left over from a different source
//! keeps being served until that provider's own interval elapses. Deleting it is therefore not
//! housekeeping: it has to happen before the core reloads, or the reload is silently wrong.
//!
//! **A copied asset is worth re-copying only when its source changed.** Comparing content would
//! mean hashing tens of megabytes of geo data on every profile switch, which is most of what
//! staging was supposed to save. The manifest records what each copy was made from instead.

use crate::core::assets::{
    application_bundle_root, invalid_asset, runtime_cleanup_retry_delay, validate_core_path,
    validate_destination, validate_source,
};
use crate::core::auth::{AuthenticatedOwner, ServiceError};
use crate::core::manager::CORE_MANAGER;
use crate::{RemoteProvider, RuntimeAsset, RuntimeBundle, StageRejection, StageRuntimeOutcome};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The name staging keeps its own bookkeeping under, inside the generation.
pub(crate) const MANIFEST_FILE_NAME: &str = ".runtime-manifest.json";

/// Identity of a copy's source at the moment the copy was made.
///
/// Length and modification time rather than a digest: the point of skipping an unchanged asset
/// is to not read it. A source rewritten in place, to the same length, with its modification
/// time restored would be missed; nothing in this system produces that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceIdentity {
    pub source: String,
    pub len: u64,
    pub mtime_ns: u128,
}

/// What the previous staging (or start) left in a generation.
///
/// Absent for a generation written by a service that predates staging. Absence is not an error:
/// it means nothing can be proven, so nothing is skipped, nothing is swept, and every declared
/// remote cache is discarded rather than trusted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeManifest {
    #[serde(default)]
    pub assets: BTreeMap<String, SourceIdentity>,
    #[serde(default)]
    pub remote_providers: BTreeMap<String, String>,
}

/// A copy staging intends to make, with the identity to record once it succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedCopy {
    pub source: String,
    pub destination: String,
    pub identity: SourceIdentity,
}

/// The full set of changes that turn a generation into the bundle, ordered by when they matter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StagePlan {
    /// Stale remote caches. Must be gone before the core reloads.
    pub required_deletes: Vec<String>,
    /// Assets whose source changed since the copy on disk was made.
    pub copies: Vec<PlannedCopy>,
    /// Assets already made from the source they still name. Recorded for logging only.
    pub skipped: Vec<String>,
    /// Paths a previous staging wrote that this bundle no longer declares. Pure housekeeping:
    /// failing to remove them changes nothing the core observes.
    pub hygiene_deletes: Vec<String>,
    /// What to persist once the copies and required deletes have gone through.
    pub manifest: RuntimeManifest,
}

/// Freshly stat'd metadata for one declared asset's source, gathered by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssetSource {
    pub asset: RuntimeAsset,
    pub len: u64,
    pub mtime_ns: u128,
}

/// Decide what staging must do. Pure: every fact it needs has already been read.
pub(crate) fn plan_stage(
    previous: &RuntimeManifest,
    sources: &[AssetSource],
    remote: &[RemoteProvider],
) -> StagePlan {
    let mut plan = StagePlan::default();

    for source in sources {
        let identity = SourceIdentity {
            source: source.asset.source.clone(),
            len: source.len,
            mtime_ns: source.mtime_ns,
        };
        let destination = source.asset.destination.clone();
        if previous.assets.get(&destination) == Some(&identity) {
            plan.skipped.push(destination.clone());
        } else {
            plan.copies.push(PlannedCopy {
                source: source.asset.source.clone(),
                destination: destination.clone(),
                identity: identity.clone(),
            });
        }
        plan.manifest.assets.insert(destination, identity);
    }

    for provider in remote {
        // No record means no proof the cache came from this url, so it cannot be reused. That is
        // the same branch a changed url takes, and it is why an absent manifest is merely slow.
        if previous.remote_providers.get(&provider.destination) != Some(&provider.url) {
            plan.required_deletes.push(provider.destination.clone());
        }
        plan.manifest
            .remote_providers
            .insert(provider.destination.clone(), provider.url.clone());
    }

    for recorded in previous
        .assets
        .keys()
        .chain(previous.remote_providers.keys())
    {
        if !plan.manifest.assets.contains_key(recorded)
            && !plan.manifest.remote_providers.contains_key(recorded)
        {
            plan.hygiene_deletes.push(recorded.clone());
        }
    }
    plan.hygiene_deletes.dedup();

    plan
}

/// The remote providers a bundle declares, deduplicated by destination.
///
/// Two providers pointing the same `path` at different urls is a configuration the client should
/// not produce; if it does, the conflict resolves towards deleting the cache, since neither url
/// can be shown to have produced it.
pub(crate) fn declared_remote_providers(bundle: &RuntimeBundle) -> Vec<RemoteProvider> {
    let mut seen: BTreeMap<String, Option<String>> = BTreeMap::new();
    for provider in &bundle.remote_providers {
        seen.entry(provider.destination.clone())
            .and_modify(|url| {
                if url.as_deref() != Some(provider.url.as_str()) {
                    *url = None;
                }
            })
            .or_insert_with(|| Some(provider.url.clone()));
    }
    seen.into_iter()
        .map(|(destination, url)| RemoteProvider {
            destination,
            // A conflicting declaration is given a url no record can match, so the cache is
            // always discarded rather than silently attributed to one of the two sources.
            url: url.unwrap_or_else(|| "\u{0}conflicting-declaration".to_owned()),
        })
        .collect()
}

/// Make the generation the core is running in match `bundle`, or decline and change nothing.
///
/// The order below is the whole safety argument, so it is worth stating plainly. Stale caches go
/// first because the core must not be able to read one. Copies go next because adding or
/// replacing a declared file cannot mislead a core still running the previous configuration.
/// `config.yaml` is replaced last and atomically, so every way of failing before that point
/// leaves the configuration on disk agreeing with the core that is running — which is what makes
/// the caller's fallback to stop + start safe. Housekeeping happens after the commit and its
/// failures are logged, never returned: by then the generation already matches the bundle in
/// every way the core can observe.
pub(crate) async fn stage_runtime(
    owner: &AuthenticatedOwner,
    bundle: &RuntimeBundle,
) -> Result<StageRuntimeOutcome, ServiceError> {
    let Some(running) = CORE_MANAGER.lock().await.running_core_config().await else {
        return Ok(StageRuntimeOutcome::RestartRequired {
            reason: StageRejection::CoreNotRunning,
        });
    };

    let core_path = validate_core_path(owner, &bundle.core_path)?;
    if Path::new(&running.core_config.core_path) != core_path {
        return Ok(StageRuntimeOutcome::RestartRequired {
            reason: StageRejection::CorePathChanged,
        });
    }

    let generation = PathBuf::from(&running.core_config.config_dir);
    let app_bundle_root = application_bundle_root(&core_path);
    let mut sources = Vec::with_capacity(bundle.assets.len());
    for asset in &bundle.assets {
        let source = validate_source(owner, app_bundle_root.as_deref(), &asset.source)?;
        let destination = validate_destination(&asset.destination)?;
        let metadata = tokio::fs::metadata(&source).await.map_err(|error| {
            invalid_asset(format!(
                "failed to inspect runtime asset {source:?}: {error}"
            ))
        })?;
        sources.push(AssetSource {
            asset: RuntimeAsset {
                source: source.to_string_lossy().into_owned(),
                destination: destination.to_string_lossy().replace('\\', "/"),
            },
            len: metadata.len(),
            mtime_ns: modified_nanos(&metadata),
        });
    }

    let remote = declared_remote_providers(bundle);
    for provider in &remote {
        validate_destination(&provider.destination)?;
    }

    let plan = plan_stage(&read_manifest(&generation).await, &sources, &remote);

    for destination in &plan.required_deletes {
        if let Err(error) = remove_staged_file(&generation.join(destination)).await {
            return Ok(StageRuntimeOutcome::RestartRequired {
                reason: StageRejection::RuntimeUnwritable {
                    detail: format!("failed to discard the stale cache {destination}: {error}"),
                },
            });
        }
    }

    for copy in &plan.copies {
        if let Err(error) =
            copy_staged_file(&copy.source, &generation.join(&copy.destination)).await
        {
            return Ok(StageRuntimeOutcome::RestartRequired {
                reason: StageRejection::RuntimeUnwritable {
                    detail: format!(
                        "failed to refresh the runtime asset {}: {error}",
                        copy.destination
                    ),
                },
            });
        }
    }

    let config_path = PathBuf::from(&running.core_config.config_path);
    if let Err(error) =
        commit_staged_config(&generation, &config_path, &bundle.yaml, &plan.manifest).await
    {
        return Ok(StageRuntimeOutcome::RestartRequired {
            reason: StageRejection::RuntimeUnwritable {
                detail: format!("failed to commit the staged configuration: {error}"),
            },
        });
    }

    for destination in &plan.hygiene_deletes {
        if let Err(error) = remove_staged_file(&generation.join(destination)).await {
            tracing::warn!(
                destination = %destination,
                error = %error,
                "Left an undeclared file behind in the staged runtime generation"
            );
        }
    }

    tracing::info!(
        copied = plan.copies.len(),
        skipped = plan.skipped.len(),
        discarded = plan.required_deletes.len(),
        "Staged a runtime generation in place"
    );
    Ok(StageRuntimeOutcome::Staged {
        config_path: config_path.to_string_lossy().into_owned(),
    })
}

fn modified_nanos(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos())
}

async fn read_manifest(generation: &Path) -> RuntimeManifest {
    let path = generation.join(MANIFEST_FILE_NAME);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            // A manifest that cannot be parsed is treated exactly like a missing one: nothing can
            // be proven, so nothing is skipped and no cache is trusted.
            tracing::warn!(path = ?path, error = %error, "Ignoring an unreadable runtime manifest");
            RuntimeManifest::default()
        }),
        Err(_) => RuntimeManifest::default(),
    }
}

/// Retry the failures a handle held by the running core produces on Windows.
///
/// Both removing and replacing need this, and for the same reason: a deletion the core's handles
/// leave pending makes the name unusable for a while, so the next operation on it fails with a
/// code that says "not yet" rather than "never". A handle that is never released stops being
/// transient, and the caller turns that into a restart instead of a half-corrected directory.
async fn while_the_core_lets_go<Operation, Attempt>(mut operation: Operation) -> std::io::Result<()>
where
    Operation: FnMut() -> Attempt,
    Attempt: std::future::Future<Output = std::io::Result<()>>,
{
    let mut retry_index = 0;
    loop {
        match operation().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let Some(delay) = runtime_cleanup_retry_delay(&error, retry_index) else {
                    return Err(error);
                };
                retry_index += 1;
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn remove_staged_file(path: &Path) -> std::io::Result<()> {
    while_the_core_lets_go(|| async {
        match tokio::fs::remove_file(path).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    })
    .await
}

/// Put `staged` in place of `destination`, cleaning up the temporary on any failure.
///
/// Replace rather than overwrite: a rename detaches the handles the running core holds on the
/// previous file instead of writing underneath them, so a core mid-read sees one file or the
/// other and never a half-written one.
async fn replace_staged_file(staged: &Path, destination: &Path) -> std::io::Result<()> {
    let result =
        while_the_core_lets_go(|| crate::core::atomic_file::replace(staged, destination)).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(staged).await;
    }
    result
}

async fn copy_staged_file(source: &str, destination: &Path) -> std::io::Result<()> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let staged = staging_temp_path(destination);
    if let Err(error) = tokio::fs::copy(source, &staged).await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(error);
    }
    replace_staged_file(&staged, destination).await
}

async fn commit_staged_config(
    generation: &Path,
    config_path: &Path,
    yaml: &str,
    manifest: &RuntimeManifest,
) -> std::io::Result<()> {
    // The manifest describes the copies and deletions that already happened, so it is written
    // first: a manifest ahead of the configuration would claim work that a later failure undid.
    let manifest_path = generation.join(MANIFEST_FILE_NAME);
    let encoded = serde_json::to_vec(manifest)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write_atomically(&manifest_path, &encoded).await?;
    write_atomically(config_path, yaml.as_bytes()).await
}

async fn write_atomically(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let staged = staging_temp_path(destination);
    let mut file = tokio::fs::File::create(&staged).await?;
    file.write_all(contents).await?;
    file.sync_all().await?;
    drop(file);
    replace_staged_file(&staged, destination).await
}

fn staging_temp_path(destination: &Path) -> PathBuf {
    let sequence = STAGING_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = destination.file_name().map_or_else(
        || "staged".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    destination.with_file_name(format!(".{name}.staging-{}-{sequence}", std::process::id()))
}

static STAGING_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(source: &str, len: u64, mtime_ns: u128) -> SourceIdentity {
        SourceIdentity {
            source: source.to_owned(),
            len,
            mtime_ns,
        }
    }

    fn asset_source(source: &str, destination: &str, len: u64, mtime_ns: u128) -> AssetSource {
        AssetSource {
            asset: RuntimeAsset {
                source: source.to_owned(),
                destination: destination.to_owned(),
            },
            len,
            mtime_ns,
        }
    }

    fn remote(destination: &str, url: &str) -> RemoteProvider {
        RemoteProvider {
            destination: destination.to_owned(),
            url: url.to_owned(),
        }
    }

    fn manifest(assets: &[(&str, SourceIdentity)], providers: &[(&str, &str)]) -> RuntimeManifest {
        RuntimeManifest {
            assets: assets
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            remote_providers: providers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn an_asset_made_from_the_same_source_is_not_copied_again() {
        let previous = manifest(
            &[(
                "geoip.metadb",
                identity("/app/geoip.metadb", 60_000_000, 42),
            )],
            &[],
        );
        let sources = [asset_source(
            "/app/geoip.metadb",
            "geoip.metadb",
            60_000_000,
            42,
        )];

        let plan = plan_stage(&previous, &sources, &[]);

        assert!(
            plan.copies.is_empty(),
            "unchanged geo data must not be re-copied"
        );
        assert_eq!(plan.skipped, ["geoip.metadb"]);
        assert!(plan.required_deletes.is_empty());
    }

    #[test]
    fn an_asset_whose_source_changed_is_copied() {
        let previous = manifest(
            &[(
                "geoip.metadb",
                identity("/app/geoip.metadb", 60_000_000, 42),
            )],
            &[],
        );
        let sources = [asset_source(
            "/app/geoip.metadb",
            "geoip.metadb",
            61_000_000,
            99,
        )];

        let plan = plan_stage(&previous, &sources, &[]);

        assert_eq!(plan.copies.len(), 1);
        assert_eq!(plan.copies[0].destination, "geoip.metadb");
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn an_asset_copied_from_a_different_source_path_is_copied_again() {
        // Same length and modification time, different origin: the file on disk is not a copy of
        // what the bundle now names, whatever its metadata says.
        let previous = manifest(
            &[("providers/p.yaml", identity("/app/one.yaml", 128, 7))],
            &[],
        );
        let sources = [asset_source("/app/two.yaml", "providers/p.yaml", 128, 7)];

        let plan = plan_stage(&previous, &sources, &[]);

        assert_eq!(plan.copies.len(), 1);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn a_remote_cache_from_the_same_url_is_kept() {
        let previous = manifest(&[], &[("rules/ads.yaml", "https://one.example/ads.yaml")]);

        let plan = plan_stage(
            &previous,
            &[],
            &[remote("rules/ads.yaml", "https://one.example/ads.yaml")],
        );

        assert!(
            plan.required_deletes.is_empty(),
            "an unchanged source must keep its download cache"
        );
        assert!(plan.copies.is_empty());
    }

    #[test]
    fn a_remote_cache_whose_url_changed_must_be_deleted_before_reload() {
        let previous = manifest(&[], &[("rules/ads.yaml", "https://one.example/ads.yaml")]);

        let plan = plan_stage(
            &previous,
            &[],
            &[remote("rules/ads.yaml", "https://two.example/ads.yaml")],
        );

        assert_eq!(plan.required_deletes, ["rules/ads.yaml"]);
    }

    #[test]
    fn without_a_manifest_nothing_is_skipped_and_every_cache_is_discarded() {
        let sources = [asset_source(
            "/app/geoip.metadb",
            "geoip.metadb",
            60_000_000,
            42,
        )];

        let plan = plan_stage(
            &RuntimeManifest::default(),
            &sources,
            &[remote("rules/ads.yaml", "https://one.example/ads.yaml")],
        );

        assert_eq!(plan.copies.len(), 1);
        assert_eq!(plan.required_deletes, ["rules/ads.yaml"]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn only_paths_a_previous_staging_recorded_are_swept() {
        let previous = manifest(
            &[("providers/gone.yaml", identity("/app/gone.yaml", 10, 1))],
            &[("rules/gone.yaml", "https://one.example/gone.yaml")],
        );

        let plan = plan_stage(&previous, &[], &[]);

        assert_eq!(
            plan.hygiene_deletes,
            ["providers/gone.yaml", "rules/gone.yaml"]
        );
        assert!(
            plan.required_deletes.is_empty() && plan.copies.is_empty(),
            "housekeeping alone must not make staging look like it has work to do"
        );
    }

    #[test]
    fn a_file_the_service_never_wrote_is_never_a_deletion_candidate() {
        // `cache.db` is created and held open by the running core. It appears in no manifest, so
        // no plan may name it — this is what keeps a sweep from deleting the core's database.
        let previous = manifest(
            &[("geoip.metadb", identity("/app/geoip.metadb", 1, 1))],
            &[],
        );

        let plan = plan_stage(&previous, &[], &[]);

        assert!(!plan.required_deletes.iter().any(|path| path == "cache.db"));
        assert!(!plan.hygiene_deletes.iter().any(|path| path == "cache.db"));
        assert_eq!(plan.hygiene_deletes, ["geoip.metadb"]);
    }

    #[test]
    fn a_destination_that_stays_declared_is_not_swept() {
        let previous = manifest(&[], &[("rules/ads.yaml", "https://one.example/ads.yaml")]);

        let plan = plan_stage(
            &previous,
            &[],
            &[remote("rules/ads.yaml", "https://two.example/ads.yaml")],
        );

        assert_eq!(plan.required_deletes, ["rules/ads.yaml"]);
        assert!(
            plan.hygiene_deletes.is_empty(),
            "a destination that is still declared is replaced, not swept"
        );
    }

    #[test]
    fn conflicting_declarations_for_one_destination_discard_the_cache() {
        let bundle = RuntimeBundle {
            yaml: String::new(),
            assets: Vec::new(),
            remote_providers: vec![
                remote("rules/ads.yaml", "https://one.example/ads.yaml"),
                remote("rules/ads.yaml", "https://two.example/ads.yaml"),
            ],
            core_path: String::new(),
        };
        let previous = manifest(&[], &[("rules/ads.yaml", "https://one.example/ads.yaml")]);

        let declared = declared_remote_providers(&bundle);
        let plan = plan_stage(&previous, &[], &declared);

        assert_eq!(declared.len(), 1, "one destination yields one decision");
        assert_eq!(plan.required_deletes, ["rules/ads.yaml"]);
    }

    #[test]
    fn repeating_one_provider_verbatim_still_keeps_its_cache() {
        let bundle = RuntimeBundle {
            yaml: String::new(),
            assets: Vec::new(),
            remote_providers: vec![
                remote("rules/ads.yaml", "https://one.example/ads.yaml"),
                remote("rules/ads.yaml", "https://one.example/ads.yaml"),
            ],
            core_path: String::new(),
        };
        let previous = manifest(&[], &[("rules/ads.yaml", "https://one.example/ads.yaml")]);

        let plan = plan_stage(&previous, &[], &declared_remote_providers(&bundle));

        assert!(plan.required_deletes.is_empty());
    }
}
