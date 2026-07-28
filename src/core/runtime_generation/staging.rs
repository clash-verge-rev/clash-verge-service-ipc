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

use super::assets::{
    destination_key, invalid_asset, resolve_in_generation, runtime_cleanup_retry_delay,
    validate_core_path, validate_destination,
};
use crate::core::auth::{AuthenticatedOwner, ServiceError};
use crate::core::manager::CORE_MANAGER;
use crate::{RemoteProvider, RuntimeAsset, RuntimeBundle, StageRejection, StageRuntimeOutcome};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The name staging keeps its own bookkeeping under, inside the generation.
pub(super) const MANIFEST_FILE_NAME: &str = ".runtime-manifest.json";

/// Identity of a copy's source at the moment the copy was made.
///
/// Length and modification time rather than a digest: the point of skipping an unchanged asset
/// is to not read it. A source rewritten in place, to the same length, with its modification
/// time deliberately restored would be missed; nothing in this system produces that, and the
/// alternative is hashing tens of megabytes of geo data on every profile switch.
///
/// `mtime_ns` is optional because a filesystem that cannot report a modification time leaves
/// nothing to compare. Collapsing that case into a number would make every same-length change
/// look unchanged, silently and forever — so an unknown time simply never matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SourceIdentity {
    pub source: String,
    pub len: u64,
    #[serde(default)]
    pub mtime_ns: Option<u128>,
}

impl SourceIdentity {
    /// Whether a copy made under `self` can be trusted to still match a source seen as `current`.
    fn still_matches(&self, current: &Self) -> bool {
        self.mtime_ns.is_some() && self == current
    }
}

/// What the previous staging (or start) left in a generation.
///
/// Absent for a generation written by a service that predates staging. Absence is not an error:
/// it means nothing can be proven, so nothing is skipped, nothing is swept, and every declared
/// remote cache is discarded rather than trusted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RuntimeManifest {
    #[serde(default)]
    pub assets: BTreeMap<String, SourceIdentity>,
    #[serde(default)]
    pub remote_providers: BTreeMap<String, String>,
}

/// A copy staging intends to make, with the identity to record once it succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlannedCopy {
    pub source: String,
    pub destination: String,
    pub identity: SourceIdentity,
}

/// The full set of changes that turn a generation into the bundle, ordered by when they matter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct StagePlan {
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
pub(super) struct AssetSource {
    pub asset: RuntimeAsset,
    pub len: u64,
    pub mtime_ns: Option<u128>,
}

/// Decide what staging must do. Pure: every fact it needs has already been read.
pub(super) fn plan_stage(
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
        if previous
            .assets
            .get(&destination)
            .is_some_and(|recorded| recorded.still_matches(&identity))
        {
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
    // A destination recorded as both an asset and a remote provider appears twice; the two chained
    // maps are each sorted but their concatenation is not, so `dedup` alone would not notice.
    // Sorting first is what makes it a deduplication rather than a no-op.
    plan.hygiene_deletes.sort();
    plan.hygiene_deletes.dedup();

    plan
}

/// The remote providers a bundle declares, keyed by destination.
///
/// A destination declared twice with different urls, or declared as both a copied asset and a
/// remote provider, is refused rather than reconciled. The two declarations disagree about who
/// owns the file, and staging has no way to settle it: it cannot know which url produced the cache
/// on disk, and copying an asset over what the core fetched would pin the wrong content — silently
/// and permanently, because the next staging would find both records matching and skip both.
pub(super) fn declared_remote_providers(
    declared: &[RemoteProvider],
    asset_destinations: &BTreeSet<String>,
) -> Result<Vec<RemoteProvider>, ServiceError> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for provider in declared {
        let destination = destination_key(&validate_destination(&provider.destination)?)?;
        if asset_destinations.contains(&destination) {
            return Err(invalid_asset(format!(
                "runtime destination {destination:?} is declared both as a copied asset and as a remote provider"
            )));
        }
        match seen.entry(destination) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(provider.url.clone());
            }
            std::collections::btree_map::Entry::Occupied(slot) if slot.get() == &provider.url => {}
            std::collections::btree_map::Entry::Occupied(slot) => {
                return Err(invalid_asset(format!(
                    "runtime destination {:?} is declared for two different provider sources",
                    slot.key()
                )));
            }
        }
    }
    Ok(seen
        .into_iter()
        .map(|(destination, url)| RemoteProvider { destination, url })
        .collect())
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
    let Some((core_pid, running)) = CORE_MANAGER.lock().await.running_core_config().await else {
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
    let super::assets::GatheredBundle { sources, remote } =
        super::assets::gather_bundle(owner, bundle, &core_path).await?;

    let previous = match read_manifest(&generation).await {
        Ok(previous) => previous,
        Err(detail) => {
            return Ok(StageRuntimeOutcome::RestartRequired {
                reason: StageRejection::RuntimeUnwritable { detail },
            });
        }
    };
    let plan = plan_stage(&previous, &sources, &remote);

    for destination in &plan.required_deletes {
        let target = resolve_in_generation(&generation, destination)?;
        if let Err(error) = remove_staged_file(&target).await {
            return Ok(StageRuntimeOutcome::RestartRequired {
                reason: StageRejection::RuntimeUnwritable {
                    detail: format!("failed to discard the stale cache {destination}: {error}"),
                },
            });
        }
    }

    for copy in &plan.copies {
        let target = resolve_in_generation(&generation, &copy.destination)?;
        if let Err(error) = copy_staged_file(&copy.source, &target).await {
            return Ok(StageRuntimeOutcome::RestartRequired {
                reason: StageRejection::RuntimeUnwritable {
                    detail: format!(
                        "failed to refresh the runtime asset {}: {error}",
                        copy.destination
                    ),
                },
            });
        }
        // The identity was read before the copy, so a source rewritten in between would be
        // recorded as the content that is no longer there — and then skipped forever, because the
        // record matches the source it now names. Re-reading is one stat against a file already in
        // cache; being wrong here pins the wrong bytes for the life of the generation.
        if source_identity_changed(&copy.source, &copy.identity).await {
            return Ok(StageRuntimeOutcome::RestartRequired {
                reason: StageRejection::RuntimeUnwritable {
                    detail: format!(
                        "runtime asset {} changed while it was being copied",
                        copy.destination
                    ),
                },
            });
        }
    }

    // The watchdog restarts a dead core without taking the lifecycle lock, and it restarts it in
    // this same generation from whatever `config.yaml` is on disk. Had that happened while the
    // assets above were being replaced, the core now running would not be the one this plan was
    // built against: it may have re-fetched a cache the plan just deleted, and committing would
    // record a provenance that never happened — which no later staging could ever detect, because
    // the record would look correct. Declining sends the caller down the path that rebuilds the
    // generation from nothing.
    if CORE_MANAGER
        .lock()
        .await
        .running_core_config()
        .await
        .map(|(pid, _)| pid)
        != Some(core_pid)
    {
        return Ok(StageRuntimeOutcome::RestartRequired {
            reason: StageRejection::CoreRestarted,
        });
    }

    let config_path = PathBuf::from(&running.core_config.config_path);
    if let Err(error) =
        commit_staged_config(&generation, &config_path, &bundle.yaml, &plan.manifest).await
    {
        // The manifest may already be in place while `config.yaml` is not, which would leave it
        // claiming a remote cache belongs to a url the running core is not using. Discarding it
        // costs the next staging its skips and its cache reuse; keeping it risks a cache that no
        // staging will ever discard again.
        if let Err(discard) = remove_staged_file(&generation.join(MANIFEST_FILE_NAME)).await {
            tracing::warn!(
                error = %discard,
                "Left a manifest behind that describes an uncommitted configuration"
            );
        }
        return Ok(StageRuntimeOutcome::RestartRequired {
            reason: StageRejection::RuntimeUnwritable {
                detail: format!("failed to commit the staged configuration: {error}"),
            },
        });
    }

    for destination in &plan.hygiene_deletes {
        match resolve_in_generation(&generation, destination) {
            Ok(target) => {
                if let Err(error) = remove_staged_file(&target).await {
                    tracing::warn!(
                        destination = %destination,
                        error = %error,
                        "Left an undeclared file behind in the staged runtime generation"
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
        "Staged a runtime generation in place"
    );
    Ok(StageRuntimeOutcome::Staged {
        config_path: config_path.to_string_lossy().into_owned(),
    })
}

pub(super) fn modified_nanos(metadata: &std::fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos())
}

/// Whether `source` still looks like what `recorded` says was copied from it.
///
/// Reads metadata only. An unreadable source counts as changed, since a copy that cannot be
/// re-checked is a copy whose record cannot be trusted.
pub(super) async fn source_identity_changed(source: &str, recorded: &SourceIdentity) -> bool {
    match tokio::fs::metadata(source).await {
        Ok(metadata) => {
            metadata.len() != recorded.len || modified_nanos(&metadata) != recorded.mtime_ns
        }
        Err(_) => true,
    }
}

/// Read the previous manifest, distinguishing "no record" from "a record that cannot be read".
///
/// Absence is ordinary and merely costs the skips. A manifest that is present but unparseable is
/// different: it means files were written here whose names can no longer be recovered, so they can
/// never be swept. Reporting it lets the caller rebuild the generation from nothing instead.
pub(super) async fn read_manifest(generation: &Path) -> Result<RuntimeManifest, String> {
    let path = generation.join(MANIFEST_FILE_NAME);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("runtime manifest {path:?} is unreadable: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(RuntimeManifest::default())
        }
        Err(error) => Err(format!("runtime manifest {path:?} cannot be read: {error}")),
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

pub(super) async fn remove_staged_file(path: &Path) -> std::io::Result<()> {
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

pub(super) async fn copy_staged_file(source: &str, destination: &Path) -> std::io::Result<()> {
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

pub(super) async fn commit_staged_config(
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

pub(super) async fn write_atomically(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
    async fn write_temp(staged: &Path, contents: &[u8]) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let mut file = tokio::fs::File::create(staged).await?;
        file.write_all(contents).await?;
        file.sync_all().await
    }

    let staged = staging_temp_path(destination);
    // A failure between creating the temporary and replacing with it would otherwise leave the
    // temporary behind forever: housekeeping only visits names a manifest recorded, and this name
    // is deliberately one no manifest can hold.
    if let Err(error) = write_temp(&staged, contents).await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(error);
    }
    replace_staged_file(&staged, destination).await
}

/// Where a replacement is written before it takes the destination's place.
///
/// The name carries [`STAGING_TEMP_INFIX`], which no declared destination is allowed to contain —
/// so a bundle cannot arrange for its own file to be the one a concurrent write is about to
/// clobber.
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
            mtime_ns: Some(mtime_ns),
        }
    }

    fn asset_source(source: &str, destination: &str, len: u64, mtime_ns: u128) -> AssetSource {
        AssetSource {
            asset: RuntimeAsset {
                source: source.to_owned(),
                destination: destination.to_owned(),
            },
            len,
            mtime_ns: Some(mtime_ns),
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
    fn one_destination_declared_for_two_sources_is_refused() {
        let declared = [
            remote("rules/ads.yaml", "https://one.example/ads.yaml"),
            remote("rules/ads.yaml", "https://two.example/ads.yaml"),
        ];

        let error = declared_remote_providers(&declared, &BTreeSet::new())
            .expect_err("a destination cannot be owned by two sources at once");

        assert!(
            error.message.contains("two different provider sources"),
            "{}",
            error.message
        );
    }

    #[test]
    fn repeating_one_provider_verbatim_still_keeps_its_cache() {
        let declared = [
            remote("rules/ads.yaml", "https://one.example/ads.yaml"),
            remote("rules/ads.yaml", "https://one.example/ads.yaml"),
        ];
        let previous = manifest(&[], &[("rules/ads.yaml", "https://one.example/ads.yaml")]);

        let resolved = declared_remote_providers(&declared, &BTreeSet::new())
            .expect("an identical repeat is not a conflict");
        let plan = plan_stage(&previous, &[], &resolved);

        assert_eq!(resolved.len(), 1, "one destination yields one decision");
        assert!(
            plan.required_deletes.is_empty(),
            "a cache attributable to the declared url must be reused"
        );
    }

    #[test]
    fn a_destination_claimed_by_both_an_asset_and_a_provider_is_refused() {
        // The two declarations disagree about who owns the file: staging would delete the cache and
        // then copy over the same name, pinning the app's content where the core expected its own —
        // and every later staging would find both records matching and skip both.
        let declared = [remote("providers/p.yaml", "https://one.example/p.yaml")];
        let assets = BTreeSet::from(["providers/p.yaml".to_owned()]);

        let error = declared_remote_providers(&declared, &assets)
            .expect_err("one destination cannot be both copied and downloaded");

        assert!(error.message.contains("copied asset"), "{}", error.message);
    }

    #[test]
    fn an_asset_whose_modification_time_is_unknown_is_never_skipped() {
        // Collapsing an unreadable timestamp into a number would make every same-length change
        // look unchanged, and the copy on disk would stay wrong for the life of the generation.
        let unknown = SourceIdentity {
            source: "/app/geo.dat".to_owned(),
            len: 10,
            mtime_ns: None,
        };
        let previous = manifest(&[("geo.dat", unknown.clone())], &[]);
        let sources = [AssetSource {
            asset: RuntimeAsset {
                source: "/app/geo.dat".to_owned(),
                destination: "geo.dat".to_owned(),
            },
            len: 10,
            mtime_ns: None,
        }];

        let plan = plan_stage(&previous, &sources, &[]);

        assert_eq!(plan.copies.len(), 1);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn a_name_shaped_like_a_staging_temporary_is_refused_but_a_lookalike_is_not() {
        use super::destination_key;
        use std::path::Path;

        for temporary in [".config.yaml.staging-123-4", ".geoip.metadb.staging-1-0"] {
            assert!(
                destination_key(Path::new(temporary)).is_err(),
                "{temporary} is one of staging's own temporaries"
            );
        }
        for legitimate in [
            "company.staging-prod.yaml",
            "providers/x.staging-.yaml",
            ".hidden.staging-notanumber",
        ] {
            assert!(
                destination_key(Path::new(legitimate)).is_ok(),
                "{legitimate} is an ordinary name the core would happily load"
            );
        }
    }

    #[test]
    fn the_names_the_generation_owns_are_refused_whatever_their_case() {
        use super::destination_key;
        use std::path::Path;

        // Most Windows filesystems are case-insensitive, so `Config.yaml` would be a second
        // manifest key for the one live configuration — and the sweep runs after the commit.
        for reserved in [
            "config.yaml",
            "Config.yaml",
            "CONFIG.YAML",
            ".runtime-manifest.json",
            ".Runtime-Manifest.JSON",
        ] {
            assert!(
                destination_key(Path::new(reserved)).is_err(),
                "{reserved} names a file the generation owns"
            );
        }
        // Only at the top level: nothing owns a file of that name inside a subdirectory.
        assert!(destination_key(Path::new("providers/config.yaml")).is_ok());
    }

    #[test]
    fn a_destination_recorded_as_both_kinds_is_swept_once() {
        // The two maps are each sorted but their concatenation is not, so a key present in both
        // survives a bare `dedup` whenever another key sorts between them.
        let previous = manifest(
            &[
                ("a.yaml", identity("/app/a.yaml", 1, 1)),
                ("z.yaml", identity("/app/z.yaml", 1, 1)),
            ],
            &[("a.yaml", "https://one.example/a.yaml")],
        );

        let plan = plan_stage(&previous, &[], &[]);

        assert_eq!(plan.hygiene_deletes, ["a.yaml", "z.yaml"]);
    }
}
