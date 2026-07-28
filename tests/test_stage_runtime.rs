#![cfg(all(feature = "standalone", feature = "client", feature = "test"))]

//! Staging a runtime into the generation a live core is running in.
//!
//! Everything here runs against a real service and a real child process, because the properties
//! that matter are about a directory somebody else is holding open: that the core keeps running
//! and keeps its session, that a file the service never wrote is never removed, and that a
//! replacement it cannot perform makes it decline instead of leaving the generation half-done.

mod common;

use anyhow::{Context as _, Result};
use clash_verge_service_ipc::{
    OwnerCredentials, OwnerSessionProof, RemoteProvider, RuntimeAsset, RuntimeBundle,
    ServiceErrorCode, StageRejection, StageRuntimeOutcome, StartClashRequest, get_status,
    run_ipc_server, service_paths, stage_runtime, start_clash, stop_clash, stop_ipc_server,
    test_owner_credentials,
};
use serial_test::serial;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::{Duration, Instant};

/// An owner whose application root is canonical, since that is what the service authenticates
/// against and what every declared asset source is then required to sit below.
fn owner_in(app_root: &Path) -> Result<(OwnerCredentials, PathBuf)> {
    std::fs::create_dir_all(app_root)?;
    let canonical = std::fs::canonicalize(app_root)?;
    let credentials = test_owner_credentials(&canonical)?;
    Ok((credentials, canonical))
}

/// The one generation directory the owner has. Staging is defined against it, and so is every
/// start: an owner has exactly one, kept for the life of the installation.
fn sole_generation(credentials: &OwnerCredentials) -> Result<PathBuf> {
    let generation = service_paths()
        .for_owner(&credentials.identity)
        .runtime_dir();
    anyhow::ensure!(
        generation.is_dir(),
        "the owner has no runtime generation at {generation:?}"
    );
    Ok(generation)
}

fn bundle(app_root: &Path, yaml: &str) -> RuntimeBundle {
    RuntimeBundle {
        yaml: yaml.to_owned(),
        assets: Vec::new(),
        remote_providers: Vec::new(),
        core_path: app_root
            .join(core_file_name())
            .to_string_lossy()
            .into_owned(),
    }
}

fn core_file_name() -> String {
    format!("mock_binary{}", std::env::consts::EXE_SUFFIX)
}

/// Put a copy of the mock core inside the owner's root, so `core_path` passes the same validation
/// a real client's would.
fn install_mock_core(app_root: &Path) -> Result<()> {
    std::fs::copy(
        common::test_bin_path("mock_binary"),
        app_root.join(core_file_name()),
    )
    .context("mock core must be built before these tests run")?;
    Ok(())
}

struct RunningCore {
    credentials: OwnerCredentials,
    app_root: PathBuf,
    session: OwnerSessionProof,
    generation: PathBuf,
    pid: Option<u32>,
}

async fn start_core(label: &str, initial: &str) -> Result<RunningCore> {
    let initial = initial.to_owned();
    start_core_with(label, move |app_root| Ok(bundle(app_root, &initial))).await
}

/// Start a core from a bundle the caller builds, for tests about what the *start* path recorded.
async fn start_core_with<Build>(label: &str, build: Build) -> Result<RunningCore>
where
    Build: FnOnce(&Path) -> Result<RuntimeBundle>,
{
    let app_root =
        std::env::temp_dir().join(format!("service-ipc-stage-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&app_root);
    let (credentials, app_root) = owner_in(&app_root)?;
    install_mock_core(&app_root)?;

    let token = "ab".repeat(32);
    let start = start_clash(
        &credentials,
        &StartClashRequest {
            runtime: build(&app_root)?,
            proposed_session_token: token.clone(),
            macos_proxy: None,
        },
    )
    .await?;
    anyhow::ensure!(start.code == 0, "{}", start.message);
    let generation = start
        .data
        .context("start omitted the session")?
        .session
        .generation;
    let status = get_status(&credentials).await?.data.context("no status")?;
    anyhow::ensure!(
        status.core_pid.is_some(),
        "the service must report a pid for the core it just started — without this, every later \
         pid comparison in these tests would pass by both sides being absent"
    );

    Ok(RunningCore {
        generation: sole_generation(&credentials)?,
        session: OwnerSessionProof { generation, token },
        pid: status.core_pid,
        credentials,
        app_root,
    })
}

impl RunningCore {
    async fn stage(&self, bundle: &RuntimeBundle) -> Result<StageRuntimeOutcome> {
        let response = stage_runtime(&self.credentials, &self.session, bundle).await?;
        anyhow::ensure!(response.code == 0, "{}", response.message);
        response.data.context("staging returned no outcome")
    }

    fn staged_config(&self) -> Result<String> {
        Ok(std::fs::read_to_string(
            self.generation.join("config.yaml"),
        )?)
    }

    /// Stopping the core does not remove its generation — only the next start sweeps stale ones —
    /// so a test that left it behind would be handing the next test a directory it did not expect.
    async fn shut_down(self) -> Result<()> {
        let _ = stop_clash(&self.credentials, &self.session).await;
        let _ = std::fs::remove_dir_all(&self.generation);
        let _ = std::fs::remove_dir_all(&self.app_root);
        Ok(())
    }
}

async fn with_service<F, Fut>(body: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    common::init_tracing_for_tests();
    let _ = stop_ipc_server().await;
    let server = run_ipc_server().await?;
    common::wait_for_ipc().await?;
    let result = body().await;
    stop_ipc_server().await?;
    server.await??;
    result
}

#[tokio::test]
#[serial]
async fn staging_replaces_the_configuration_and_leaves_the_core_running() -> Result<()> {
    with_service(|| async {
        let core = start_core("replace", "mode: rule\n").await?;

        let outcome = core
            .stage(&bundle(&core.app_root, "mode: global\n"))
            .await?;

        let StageRuntimeOutcome::Staged { config_path } = outcome else {
            anyhow::bail!("expected the runtime to be staged, got {outcome:?}");
        };
        assert_eq!(
            Path::new(&config_path),
            core.generation.join("config.yaml"),
            "staging must write into the generation the core is already running in"
        );
        assert_eq!(core.staged_config()?, "mode: global\n");

        let after = get_status(&core.credentials)
            .await?
            .data
            .context("no status")?;
        assert_eq!(
            after.core_pid, core.pid,
            "staging must not replace the process"
        );
        assert_eq!(
            after.active_generation,
            Some(core.session.generation),
            "staging must not disturb the owner session the app is holding"
        );
        assert_eq!(
            sole_generation(&core.credentials)?,
            core.generation,
            "staging must reuse the generation, not allocate another"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn staging_keeps_an_unchanged_cache_and_discards_one_whose_url_changed() -> Result<()> {
    with_service(|| async {
        let core = start_core("cache", "mode: rule\n").await?;
        let cache = core.generation.join("rules/ads.yaml");
        std::fs::create_dir_all(cache.parent().context("no parent")?)?;
        // Stand in for what the core downloads: staging never writes these, it only decides
        // whether the one already there can still be attributed to the declared source.
        std::fs::write(&cache, b"from source one\n")?;

        let mut declared = bundle(&core.app_root, "mode: rule\n");
        declared.remote_providers = vec![RemoteProvider {
            destination: "rules/ads.yaml".to_owned(),
            url: "https://one.example/ads.yaml".to_owned(),
        }];

        // First staging has no record to compare against, so it must discard rather than guess.
        core.stage(&declared).await?;
        assert!(
            !cache.exists(),
            "an unattributable cache must not be trusted"
        );

        std::fs::write(&cache, b"from source one\n")?;
        core.stage(&declared).await?;
        assert!(
            cache.exists(),
            "a cache the previous staging attributed to this url must be reused"
        );

        declared.remote_providers[0].url = "https://two.example/ads.yaml".to_owned();
        core.stage(&declared).await?;
        assert!(
            !cache.exists(),
            "a cache whose source changed must be gone before the core reloads"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn staging_never_removes_a_file_the_service_did_not_write() -> Result<()> {
    with_service(|| async {
        let core = start_core("foreign", "mode: rule\n").await?;
        // The running core creates and holds this. Nothing in a bundle ever declares it, so a
        // sweep driven by the declaration rather than by the manifest would delete it.
        let core_owned = core.generation.join("cache.db");
        std::fs::write(&core_owned, b"core state")?;

        let mut declared = bundle(&core.app_root, "mode: rule\n");
        std::fs::write(core.app_root.join("provider.yaml"), b"proxies: []\n")?;
        declared.assets = vec![RuntimeAsset {
            source: core
                .app_root
                .join("provider.yaml")
                .to_string_lossy()
                .into_owned(),
            destination: "providers/copied.yaml".to_owned(),
        }];
        core.stage(&declared).await?;
        assert!(core.generation.join("providers/copied.yaml").exists());

        // Now declare nothing at all: the copied asset is swept, the core's file is not.
        core.stage(&bundle(&core.app_root, "mode: direct\n"))
            .await?;

        assert!(
            core_owned.exists(),
            "a file the service never wrote must survive every sweep"
        );
        assert!(
            !core.generation.join("providers/copied.yaml").exists(),
            "an asset a previous staging wrote and this one dropped must be swept"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn an_unchanged_asset_is_not_copied_again() -> Result<()> {
    with_service(|| async {
        let core = start_core("skip", "mode: rule\n").await?;
        std::fs::write(core.app_root.join("geo.dat"), b"geo payload")?;
        let mut declared = bundle(&core.app_root, "mode: rule\n");
        declared.assets = vec![RuntimeAsset {
            source: core.app_root.join("geo.dat").to_string_lossy().into_owned(),
            destination: "geo.dat".to_owned(),
        }];

        core.stage(&declared).await?;
        let copied = core.generation.join("geo.dat");
        // A marker only a re-copy would destroy. Comparing modification times would be at the
        // mercy of filesystem timestamp granularity; replacing the file's contents is not.
        std::fs::write(&copied, b"untouched marker")?;

        core.stage(&declared).await?;

        assert_eq!(
            std::fs::read(&copied)?,
            b"untouched marker",
            "an asset whose source did not change must not be copied again"
        );

        std::fs::write(core.app_root.join("geo.dat"), b"a different geo payload")?;
        core.stage(&declared).await?;

        assert_eq!(
            std::fs::read(&copied)?,
            b"a different geo payload",
            "an asset whose source changed must be copied again"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn staging_declines_when_the_bundle_names_a_different_core() -> Result<()> {
    with_service(|| async {
        let core = start_core("core-path", "mode: rule\n").await?;
        let other = core.app_root.join("other_core");
        std::fs::copy(common::test_bin_path("mock_binary"), &other)?;

        let mut declared = bundle(&core.app_root, "mode: global\n");
        declared.core_path = other.to_string_lossy().into_owned();

        let outcome = core.stage(&declared).await?;

        assert_eq!(
            outcome,
            StageRuntimeOutcome::RestartRequired {
                reason: StageRejection::CorePathChanged
            },
            "a reload cannot swap the binary, so staging must not pretend it can"
        );
        assert_eq!(
            core.staged_config()?,
            "mode: rule\n",
            "declining must leave the configuration agreeing with the running core"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn staging_rejects_a_session_that_no_longer_owns_the_core() -> Result<()> {
    with_service(|| async {
        let core = start_core("session", "mode: rule\n").await?;
        let stale = OwnerSessionProof {
            generation: core.session.generation,
            token: "cd".repeat(32),
        };

        let response = stage_runtime(
            &core.credentials,
            &stale,
            &bundle(&core.app_root, "mode: global\n"),
        )
        .await?;

        assert_eq!(response.code, ServiceErrorCode::StaleOwnerSession as u16);
        assert_eq!(
            core.staged_config()?,
            "mode: rule\n",
            "a rejected request must not have written anything"
        );

        core.shut_down().await
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn a_destination_that_is_a_filename_on_this_platform_stays_a_filename() -> Result<()> {
    with_service(|| async {
        let core = start_core("traversal", "mode: rule\n").await?;
        std::fs::write(core.app_root.join("payload"), b"owned\n")?;
        let escape = core
            .generation
            .parent()
            .context("generation has no parent")?
            .join("escaped-by-staging");
        let _ = std::fs::remove_file(&escape);

        let mut declared = bundle(&core.app_root, "mode: global\n");
        // A backslash is an ordinary character in a Unix filename, so this is one component and the
        // destination validator accepts it. It must still be treated as the filename it is: the
        // moment anything rewrites the separator afterwards, an accepted destination becomes a
        // traversal, and the service writes client-controlled bytes wherever it points.
        declared.assets = vec![RuntimeAsset {
            source: core.app_root.join("payload").to_string_lossy().into_owned(),
            destination: r"..\escaped-by-staging".to_owned(),
        }];

        let outcome = core.stage(&declared).await;

        assert!(
            !escape.exists(),
            "staging wrote outside the generation it was given"
        );
        // Insisting on `Staged` is the point: swallowing an error here would make the test blind
        // to the opposite mistake, where the fix over-corrects and refuses an ordinary filename.
        let outcome = outcome?;
        let StageRuntimeOutcome::Staged { .. } = outcome else {
            anyhow::bail!("an ordinary Unix filename must still be accepted, got {outcome:?}");
        };
        assert!(
            core.generation.join(r"..\escaped-by-staging").exists(),
            "the destination should have become a file of that literal name"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn staging_refuses_to_let_a_bundle_claim_the_names_the_generation_owns() -> Result<()> {
    with_service(|| async {
        let core = start_core("reserved", "mode: rule\n").await?;
        std::fs::write(core.app_root.join("payload"), b"owned\n")?;

        for reserved in ["config.yaml", ".runtime-manifest.json"] {
            let mut declared = bundle(&core.app_root, "mode: global\n");
            declared.assets = vec![RuntimeAsset {
                source: core.app_root.join("payload").to_string_lossy().into_owned(),
                destination: reserved.to_owned(),
            }];

            let response = stage_runtime(&core.credentials, &core.session, &declared).await?;

            assert_eq!(
                response.code,
                ServiceErrorCode::InvalidRuntimeAsset as u16,
                "{reserved} must be refused as an invalid asset, not fail for some other reason"
            );
            assert!(
                response.message.contains("owned by the runtime generation"),
                "{reserved}: {}",
                response.message
            );
        }
        assert_eq!(
            core.staged_config()?,
            "mode: rule\n",
            "a refused bundle must not have written anything"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn staging_refuses_a_destination_claimed_twice() -> Result<()> {
    with_service(|| async {
        let core = start_core("conflict", "mode: rule\n").await?;
        std::fs::write(core.app_root.join("payload"), b"owned\n")?;

        let mut both_kinds = bundle(&core.app_root, "mode: global\n");
        both_kinds.assets = vec![RuntimeAsset {
            source: core.app_root.join("payload").to_string_lossy().into_owned(),
            destination: "providers/p.yaml".to_owned(),
        }];
        both_kinds.remote_providers = vec![RemoteProvider {
            destination: "providers/p.yaml".to_owned(),
            url: "https://one.example/p.yaml".to_owned(),
        }];
        let refused = stage_runtime(&core.credentials, &core.session, &both_kinds).await?;
        assert_eq!(refused.code, ServiceErrorCode::InvalidRuntimeAsset as u16);
        assert!(
            refused.message.contains("copied asset"),
            "one destination cannot be both copied by the service and downloaded by the core: {}",
            refused.message
        );

        let mut two_sources = bundle(&core.app_root, "mode: global\n");
        two_sources.remote_providers = vec![
            RemoteProvider {
                destination: "rules/ads.yaml".to_owned(),
                url: "https://one.example/ads.yaml".to_owned(),
            },
            RemoteProvider {
                destination: "rules/ads.yaml".to_owned(),
                url: "https://two.example/ads.yaml".to_owned(),
            },
        ];
        let refused = stage_runtime(&core.credentials, &core.session, &two_sources).await?;
        assert_eq!(refused.code, ServiceErrorCode::InvalidRuntimeAsset as u16);
        assert!(
            refused.message.contains("two different provider sources"),
            "staging cannot know which of two sources produced the cache on disk: {}",
            refused.message
        );

        assert_eq!(core.staged_config()?, "mode: rule\n");

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn the_first_staging_after_a_start_reuses_what_the_start_wrote() -> Result<()> {
    with_service(|| async {
        let core = start_core_with("firstskip", |app_root| {
            std::fs::write(app_root.join("geo.dat"), b"geo payload")?;
            let mut initial = bundle(app_root, "mode: rule\n");
            initial.assets = vec![RuntimeAsset {
                source: app_root.join("geo.dat").to_string_lossy().into_owned(),
                destination: "geo.dat".to_owned(),
            }];
            initial.remote_providers = vec![RemoteProvider {
                destination: "rules/ads.yaml".to_owned(),
                url: "https://one.example/ads.yaml".to_owned(),
            }];
            Ok(initial)
        })
        .await?;

        // Stand in for the download the core would have made under the configuration it started
        // with. Nothing about it changed, so the first configuration change must not throw it away.
        let cache = core.generation.join("rules/ads.yaml");
        std::fs::create_dir_all(cache.parent().context("no parent")?)?;
        std::fs::write(&cache, b"from source one\n")?;
        // A marker only a re-copy would destroy.
        std::fs::write(core.generation.join("geo.dat"), b"untouched marker")?;

        let mut declared = bundle(&core.app_root, "mode: global\n");
        declared.assets = vec![RuntimeAsset {
            source: core.app_root.join("geo.dat").to_string_lossy().into_owned(),
            destination: "geo.dat".to_owned(),
        }];
        declared.remote_providers = vec![RemoteProvider {
            destination: "rules/ads.yaml".to_owned(),
            url: "https://one.example/ads.yaml".to_owned(),
        }];
        core.stage(&declared).await?;

        assert_eq!(
            std::fs::read(core.generation.join("geo.dat"))?,
            b"untouched marker",
            "the start path must record what it copied, or the first change re-copies every asset"
        );
        assert!(
            cache.exists(),
            "the start path must record its providers, or the first change re-downloads every one"
        );

        core.shut_down().await
    })
    .await
}

#[tokio::test]
#[serial]
async fn staging_declines_when_it_cannot_write_into_the_generation() -> Result<()> {
    #[cfg(unix)]
    if unsafe { platform_lib::geteuid() } == 0 {
        // The Unix half of this test blocks staging by making a directory unwritable, and root
        // ignores that. Running anyway would assert the opposite of what the test means.
        eprintln!("skipping: cannot make a directory unwritable to root");
        return Ok(());
    }

    with_service(|| async {
        let core = start_core("blocked", "mode: rule\n").await?;
        std::fs::write(core.app_root.join("provider.yaml"), b"proxies: []\n")?;
        let mut declared = bundle(&core.app_root, "mode: global\n");
        declared.assets = vec![RuntimeAsset {
            source: core
                .app_root
                .join("provider.yaml")
                .to_string_lossy()
                .into_owned(),
            destination: "providers/copied.yaml".to_owned(),
        }];

        let blocked = core.generation.join("providers/copied.yaml");
        std::fs::create_dir_all(blocked.parent().context("no parent")?)?;
        std::fs::write(&blocked, b"previous\n")?;
        let guard = block_new_file(&blocked)?;

        let outcome = core.stage(&declared).await?;

        assert!(
            matches!(
                outcome,
                StageRuntimeOutcome::RestartRequired {
                    reason: StageRejection::RuntimeUnwritable { .. }
                }
            ),
            "a replacement that cannot be performed must decline, got {outcome:?}"
        );
        drop(guard);
        assert_eq!(
            core.staged_config()?,
            "mode: rule\n",
            "declining must leave the configuration agreeing with the running core"
        );

        core.shut_down().await
    })
    .await
}

/// Stop staging from being able to put a new file in place, by whatever means the platform offers.
///
/// The two platforms are blocked at different steps, and it is worth being precise about which.
/// On Unix nothing a process can do to a file stops another from replacing it, so the containing
/// directory is made unwritable and staging fails while *creating its temporary* — the rename is
/// never reached. On Windows a held handle blocks the rename itself, which is the step a real core
/// interferes with, since it keeps handles on every provider file it loaded. What both share, and
/// what the test asserts, is that staging declines instead of committing a directory it could not
/// finish correcting.
fn block_new_file(path: &Path) -> Result<ReplacementBlock> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = path
            .parent()
            .context("blocked path has no parent")?
            .to_path_buf();
        let original = std::fs::metadata(&directory)?.permissions();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500))?;
        Ok(ReplacementBlock {
            directory,
            original,
        })
    }

    #[cfg(windows)]
    {
        let sentinel = path.with_extension("held");
        let _ = std::fs::remove_file(&sentinel);
        let holder = std::process::Command::new(common::test_bin_path("asset_lock_holder"))
            .arg(path)
            .arg(&sentinel)
            .spawn()
            .context("asset_lock_holder must be built before these tests run")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !sentinel.exists() {
            anyhow::ensure!(
                Instant::now() < deadline,
                "asset_lock_holder never took the handle"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(ReplacementBlock { holder, sentinel })
    }
}

#[cfg(unix)]
struct ReplacementBlock {
    directory: PathBuf,
    original: std::fs::Permissions,
}

#[cfg(unix)]
impl Drop for ReplacementBlock {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.directory, self.original.clone());
    }
}

#[cfg(windows)]
struct ReplacementBlock {
    holder: std::process::Child,
    sentinel: PathBuf,
}

#[cfg(windows)]
impl Drop for ReplacementBlock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sentinel);
        let _ = self.holder.wait();
    }
}
