#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    panic!("This program is not intended to run on this platform.");
}

mod shared;

use anyhow::Error;
use anyhow::{Context as _, bail};
use sha2::{Digest as _, Sha256};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use shared::run_command;
#[cfg(all(target_os = "macos", not(feature = "development-channel")))]
use shared::uninstall_old_service;
use shared::{enter_repair_gate, run_maintenance_if_requested};
use std::fs::{File, OpenOptions};
use std::io::Read as _;
#[cfg(unix)]
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn bundled_service_binary() -> Result<PathBuf, Error> {
    let source = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "clash-verge-service.exe"
    } else {
        "clash-verge-service"
    });
    let metadata = std::fs::symlink_metadata(&source)
        .with_context(|| format!("failed to inspect bundled service binary {source:?}"))?;
    if !metadata.file_type().is_file() {
        bail!("bundled service binary is not an ordinary file: {source:?}");
    }
    Ok(source)
}

fn sha256(path: &Path) -> Result<[u8; 32], Error> {
    let mut file = File::open(path).with_context(|| format!("failed to open {path:?}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {path:?}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn remove_ordinary_file_if_exists(path: &Path) -> Result<(), Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {path:?}"));
        }
    };
    if !metadata.file_type().is_file() {
        bail!("refusing to replace non-file service entry {path:?}");
    }
    std::fs::remove_file(path).with_context(|| format!("failed to remove {path:?}"))
}

fn stage_binary(source: &Path, target: &Path) -> Result<PathBuf, Error> {
    let parent = target.parent().context("protected target has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("failed to create protected directory {parent:?}"))?;
    let staged = target.with_extension(if cfg!(windows) {
        format!("exe.{}", clash_verge_service_ipc::CORE_STAGING_EXTENSION)
    } else {
        clash_verge_service_ipc::CORE_STAGING_EXTENSION.to_owned()
    });
    remove_ordinary_file_if_exists(&staged)?;

    let mut source_file = File::open(source).with_context(|| format!("failed to open candidate {source:?}"))?;
    let mut staged_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .with_context(|| format!("failed to create staged binary {staged:?}"))?;
    std::io::copy(&mut source_file, &mut staged_file)
        .with_context(|| format!("failed to stage binary at {staged:?}"))?;
    staged_file
        .sync_all()
        .with_context(|| format!("failed to sync staged binary {staged:?}"))?;
    drop(staged_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o550))
            .with_context(|| format!("failed to secure staged binary {staged:?}"))?;
    }

    if sha256(source)? != sha256(&staged)? {
        let _ = std::fs::remove_file(&staged);
        bail!("staged binary hash does not match its source: {source:?}");
    }
    Ok(staged)
}

/// Core binaries Clash Verge ships beside this installer.
///
/// Named rather than discovered by scanning: the application directory also holds the launcher and
/// the uninstaller, and neither may become something the service is willing to execute as root.
const BUNDLED_CORE_NAMES: [&str; 2] = ["verge-mihomo", "verge-mihomo-alpha"];

fn core_file_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

/// Publishes a core into the directory the service executes from, under the caller-chosen `name`.
///
/// Elevation alone does not make the bytes trustworthy: the caller must either have taken them
/// from a location only privileged accounts can write, or attest what they are. `expected_sha256`
/// carries that attestation — it is checked against both the source and the staged copy, so a
/// file swapped between the caller computing the digest and this copy landing is refused rather
/// than published. The published name is fixed by the caller rather than read off `source`, so a
/// link-shaped source resolved during validation cannot smuggle a different name into the
/// approved directory.
fn install_core(
    source: &Path,
    cores: &Path,
    name: &std::ffi::OsStr,
    expected_sha256: Option<&[u8; 32]>,
) -> Result<PathBuf, Error> {
    let metadata = std::fs::symlink_metadata(source).with_context(|| format!("failed to inspect core {source:?}"))?;
    if !metadata.file_type().is_file() {
        bail!("core candidate is not an ordinary file: {source:?}");
    }
    if Path::new(name).extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case(clash_verge_service_ipc::CORE_STAGING_EXTENSION)
            || extension.eq_ignore_ascii_case(clash_verge_service_ipc::CORE_DISPLACED_EXTENSION)
    }) {
        bail!("core name {name:?} collides with an installer bookkeeping suffix and could never be run");
    }
    let target = cores.join(name);

    let source_hash = sha256(source)?;
    if let Some(expected) = expected_sha256
        && &source_hash != expected
    {
        bail!(
            "core {source:?} does not match the attested sha256; refusing to publish it. \
             The file may have been replaced since the digest was computed."
        );
    }
    // An unchanged core needs no republish. This also keeps a routine service reinstall from
    // fighting a still-running core over its open file: same bytes, nothing to fight about.
    if let Ok(existing) = std::fs::symlink_metadata(&target)
        && existing.is_file()
        && sha256(&target)? == source_hash
    {
        println!("Core {} is already current", target.display());
        // Best-effort metadata repair: a copy staged earlier may carry a stale stamp (or, on
        // Unix, loosened permissions) while its bytes are fine. A running core holds its file
        // open on Windows, so a failure here only means the drift warning stays until the next
        // successful publish.
        if let Err(error) = propagate_source_modified_time(&target, &metadata) {
            eprintln!("Could not refresh the stamp on {}: {error:#}", target.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o550));
        }
        return Ok(target);
    }

    let staged = stage_binary(source, &target)?;
    if let Some(expected) = expected_sha256
        && &sha256(&staged)? != expected
    {
        let _ = std::fs::remove_file(&staged);
        bail!("staged copy of {source:?} stopped matching the attested sha256; it changed while being copied");
    }
    propagate_source_modified_time(&staged, &metadata)?;
    publish_staged_binary(&staged, &target)?;
    Ok(target)
}

/// Stamps the staged copy with its source's modified time.
///
/// The service reports an in-place core update the installer never saw by comparing the client
/// core's length and modified time against the approved copy's; the copy has to carry the
/// source's stamp for that comparison to mean anything.
fn propagate_source_modified_time(staged: &Path, source_metadata: &std::fs::Metadata) -> Result<(), Error> {
    let modified = source_metadata
        .modified()
        .with_context(|| format!("source of {staged:?} carries no modified time"))?;
    let file = OpenOptions::new()
        .write(true)
        .open(staged)
        .with_context(|| format!("failed to reopen staged binary {staged:?}"))?;
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .with_context(|| format!("failed to stamp staged binary {staged:?}"))
}

/// Removes bookkeeping leftovers — half-written staging copies and displaced live cores — from an
/// earlier interrupted or in-flight run.
///
/// The service refuses to run them regardless; sweeping keeps the directory describable as
/// "exactly what an administrator published". A displaced core whose process is still running
/// stays locked and simply survives until a later sweep.
fn sweep_core_bookkeeping_leftovers(cores: &Path) {
    let Ok(entries) = std::fs::read_dir(cores) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_leftover = path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case(clash_verge_service_ipc::CORE_STAGING_EXTENSION)
                || extension.eq_ignore_ascii_case(clash_verge_service_ipc::CORE_DISPLACED_EXTENSION)
        }) && entry.file_type().is_ok_and(|file_type| file_type.is_file());
        if is_leftover && let Err(error) = std::fs::remove_file(&path) {
            eprintln!("Could not remove leftover {}: {error}", path.display());
        }
    }
}

/// A core named on the command line, with the digest the caller vouches for.
struct CoreInstallRequest {
    source: PathBuf,
    sha256: Option<[u8; 32]>,
}

/// Reads the `--install-core <path>` arguments and each one's optional `--sha256 <hex>` attestation.
fn requested_core_installs() -> Result<Vec<CoreInstallRequest>, Error> {
    parse_core_install_arguments(std::env::args_os().skip(1))
}

fn parse_core_install_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Vec<CoreInstallRequest>, Error> {
    let mut requested: Vec<CoreInstallRequest> = Vec::new();
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        if argument == "--install-core" {
            let value = arguments.next().context("--install-core requires a path")?;
            requested.push(CoreInstallRequest {
                source: PathBuf::from(value),
                sha256: None,
            });
        } else if argument == "--sha256" {
            let value = arguments.next().context("--sha256 requires a hex digest")?;
            let value = value.to_str().context("--sha256 digest is not valid UTF-8")?;
            let request = requested
                .last_mut()
                .context("--sha256 must follow the --install-core path it attests")?;
            if request.sha256.is_some() {
                bail!("--sha256 was given twice for {:?}", request.source);
            }
            request.sha256 = Some(parse_sha256_hex(value)?);
        }
    }
    Ok(requested)
}

fn parse_sha256_hex(value: &str) -> Result<[u8; 32], Error> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        bail!("--sha256 expects 64 hex characters, got {value:?}");
    }
    let mut digest = [0u8; 32];
    for (index, chunk) in bytes.chunks_exact(2).enumerate() {
        digest[index] = u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?;
    }
    Ok(digest)
}

/// Handles a core-only update and reports whether it took over the run.
///
/// The application downloads a new core and asks this privileged binary to publish it, because the
/// destination is closed to unprivileged writers by design. A core already running is locked on
/// Windows, so the application has to stop it first; the publish failure says so rather than
/// leaving a half-written core behind.
fn run_core_install_if_requested() -> Result<bool, Error> {
    let requested = requested_core_installs()?;
    if requested.is_empty() {
        return Ok(false);
    }
    let _gate = enter_repair_gate()?;
    let cores = clash_verge_service_ipc::prepare_core_install_directory()?;
    sweep_core_bookkeeping_leftovers(&cores);
    for request in &requested {
        // An unattested request is honored only for a source no unprivileged account can write.
        // Anywhere else the file holds whatever the directory's writers last made it, and an
        // elevated copy must not turn that into something root executes. The digest closes that
        // channel: it travels on this process's own command line, which other local accounts
        // cannot alter, and describes the bytes the caller actually obtained. The copy must then
        // read the canonical path the verdict covered, not the caller's spelling of it, or a
        // retargetable link between the two would undo the check.
        let name = request
            .source
            .file_name()
            .context("--install-core path has no file name")?
            .to_owned();
        let source = if request.sha256.is_none() {
            clash_verge_service_ipc::require_trusted_core_source(&request.source).with_context(|| {
                format!(
                    "core source {:?} sits where unprivileged accounts can write; pass --sha256 <digest \
                     of the downloaded bytes> so the copy can be verified",
                    request.source
                )
            })?
        } else {
            request.source.clone()
        };
        let installed = install_core(&source, &cores, &name, request.sha256.as_ref())?;
        println!("Installed core {}", installed.display());
    }
    Ok(true)
}

/// Publishes the cores shipped beside the installer.
///
/// Runs as part of an ordinary install so a stock setup works without the application knowing that
/// any of this happens. A build that ships no core is not an error: the application can stage one
/// later with `--install-core`.
fn install_bundled_cores() -> Result<(), Error> {
    let installer = std::env::current_exe().context("failed to locate the running installer")?;
    let Some(directory) = installer.parent() else {
        return Ok(());
    };
    let mut roots = vec![directory.to_path_buf()];
    if let Some(parent) = directory.parent() {
        roots.push(parent.to_path_buf());
    }
    // The macOS bundle keeps this installer under Contents/Resources while the cores ship as
    // sidecars in Contents/MacOS, which neither of the directories above can see.
    #[cfg(target_os = "macos")]
    if let Some(bundle) = installer
        .ancestors()
        .find(|ancestor| ancestor.extension().is_some_and(|extension| extension == "app"))
    {
        roots.push(bundle.join("Contents").join("MacOS"));
    }

    let cores = clash_verge_service_ipc::prepare_core_install_directory()?;
    sweep_core_bookkeeping_leftovers(&cores);
    let mut seen: Vec<String> = Vec::new();
    for root in roots {
        for stem in BUNDLED_CORE_NAMES {
            let name = core_file_name(stem);
            let candidate = root.join(&name);
            if seen.contains(&name) || !candidate.is_file() {
                continue;
            }
            seen.push(name.clone());
            // Only a source that itself sits behind administrative write control may be staged
            // without an attestation. An application installed somewhere ordinary accounts can
            // write ships bytes any of them may have replaced, and this elevated pass must not
            // launder those into the directory root executes from; such installs stage their
            // cores explicitly through --install-core with a --sha256 digest. Copying then reads
            // the canonical path the verdict covered, so a link retargeted afterwards changes
            // nothing.
            let source = match clash_verge_service_ipc::require_trusted_core_source(&candidate) {
                Ok(canonical) => canonical,
                Err(reason) => {
                    eprintln!(
                        "Not auto-staging {}: {reason:#}. Stage it explicitly with --install-core <path> --sha256 <digest>.",
                        candidate.display()
                    );
                    continue;
                }
            };
            match install_core(&source, &cores, std::ffi::OsStr::new(&name), None) {
                Ok(installed) => println!("Installed core {}", installed.display()),
                // A core still running holds its own file open on Windows, and the copy already
                // staged is the one that keeps working; failing a routine reinstall over that
                // would break a healthy setup. With nothing staged there is nothing to fall back
                // to — reporting success would hand over a service whose every core start fails.
                Err(error) => {
                    // symlink_metadata: a link in the approved slot is not a fallback the runtime
                    // would accept, so it must not suppress the failure either.
                    let fallback_is_regular = std::fs::symlink_metadata(cores.join(&name))
                        .map(|metadata| metadata.is_file())
                        .unwrap_or(false);
                    if fallback_is_regular {
                        eprintln!("Kept the existing copy of {name}: {error:#}");
                    } else {
                        return Err(error.context(format!("failed to stage {name}, and no approved copy exists")));
                    }
                }
            }
        }
    }
    if seen.is_empty() {
        eprintln!(
            "No bundled core was found beside {}; stage one with --install-core before starting the core.",
            installer.display()
        );
    }
    Ok(())
}

fn publish_staged_binary(staged: &Path, target: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!("refusing to replace non-file service entry {target:?}");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {target:?}"));
        }
    }

    #[cfg(unix)]
    {
        std::fs::rename(staged, target)
            .with_context(|| format!("failed to publish service binary {staged:?} at {target:?}"))
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

        let wide = |path: &Path| {
            let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
            value.push(0);
            value
        };
        let move_over = |from: &Path, to: &Path| -> std::io::Result<()> {
            let from = wide(from);
            let to = wide(to);
            if unsafe {
                MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        };

        let Err(direct_error) = move_over(staged, target) else {
            // A displaced sibling from an earlier fallback publish may still be lying around.
            let _ = std::fs::remove_file(target.with_extension(clash_verge_service_ipc::CORE_DISPLACED_EXTENSION));
            return Ok(());
        };

        // Windows will not overwrite a running executable, but it will rename one. Move the live
        // file aside and slot the replacement into its name; a core start after this picks up the
        // new bytes without the caller having to stop the old core first. The displaced file is
        // never runnable through the core resolution and is swept on the next install.
        let displaced = target.with_extension(clash_verge_service_ipc::CORE_DISPLACED_EXTENSION);
        if move_over(target, &displaced).is_err() {
            return Err(direct_error).with_context(|| format!("failed to publish {staged:?} at {target:?}"));
        }
        match move_over(staged, target) {
            Ok(()) => {
                // Still open while the displaced binary runs; the sweep on a later install gets it.
                let _ = std::fs::remove_file(&displaced);
                Ok(())
            }
            Err(publish_error) => {
                // Put the displaced file back so the name does not go empty; say so if even that failed.
                if move_over(&displaced, target).is_ok() {
                    Err(publish_error).with_context(|| format!("failed to publish {staged:?} at {target:?}"))
                } else {
                    Err(publish_error).with_context(|| {
                        format!("failed to publish {staged:?} at {target:?}, and to restore it from {displaced:?}")
                    })
                }
            }
        }
    }
}

fn wait_for_service_ready() -> Result<(), Error> {
    const READY_TIMEOUT: Duration = Duration::from_secs(20);
    const READY_INTERVAL: Duration = Duration::from_millis(250);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create service readiness runtime")?;
    runtime.block_on(async {
        clash_verge_service_ipc::set_config(Some(clash_verge_service_ipc::IpcConfig {
            default_timeout: Duration::from_millis(250),
            max_retries: 1,
            retry_delay: Duration::from_millis(25),
        }))
        .await;

        let deadline = Instant::now() + READY_TIMEOUT;
        let result = loop {
            if let Ok(response) = clash_verge_service_ipc::get_version().await
                && response.code == 0
                && response.data.is_some_and(|info| {
                    info.supports_client(
                        clash_verge_service_ipc::ProtocolVersion::current(),
                        clash_verge_service_ipc::MIN_REQUIRED_SERVICE_REVISION,
                    )
                })
            {
                break Ok(());
            }
            if Instant::now() >= deadline {
                break Err(anyhow::anyhow!(
                    "service IPC did not become protocol-ready within {READY_TIMEOUT:?}"
                ));
            }
            tokio::time::sleep(READY_INTERVAL).await;
        };

        clash_verge_service_ipc::set_config(None).await;
        result
    })
}

// Only launchd code needs the concrete target; tests exercise the plan classifier instead.
#[cfg(target_os = "macos")]
fn launchd_service_target() -> String {
    format!("system/{}", clash_verge_service_ipc::MACOS_SERVICE_ID)
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum LaunchdInstallPlan {
    SkipBootout,
    Bootout,
}

#[cfg(any(target_os = "macos", test))]
fn classify_launchd_service_probe(exit_code: Option<i32>, diagnostic: &str) -> Result<LaunchdInstallPlan, Error> {
    match exit_code {
        Some(0) => Ok(LaunchdInstallPlan::Bootout),
        Some(113) if diagnostic.contains("Could not find service") => Ok(LaunchdInstallPlan::SkipBootout),
        _ => Err(anyhow::anyhow!(
            "Unexpected launchctl service probe result (exit code: {:?}): {}",
            exit_code,
            diagnostic
        )),
    }
}

#[cfg(target_os = "macos")]
fn probe_launchd_service(debug: bool) -> Result<LaunchdInstallPlan, Error> {
    if debug {
        println!("Executing: launchctl print {}", launchd_service_target());
    }

    let output = std::process::Command::new("launchctl")
        .args(["print", &launchd_service_target()])
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to probe launchd service: {}", e))?;
    let diagnostic = format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    classify_launchd_service_probe(output.status.code(), &diagnostic)
}

#[cfg(unix)]
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.parse().ok()
}

#[cfg(unix)]
fn resolve_service_group_name() -> Result<String, Error> {
    use nix::unistd::{Gid, Group, Uid, User};

    if let Some(gid) = env_u32("CLASH_VERGE_SERVICE_GID")
        && let Ok(Some(group)) = Group::from_gid(Gid::from_raw(gid))
    {
        return Ok(group.name);
    }

    if let Some(uid) = env_u32("SUDO_UID").or_else(|| env_u32("PKEXEC_UID"))
        && let Ok(Some(user)) = User::from_uid(Uid::from_raw(uid))
        && let Ok(Some(group)) = Group::from_gid(user.gid)
    {
        return Ok(group.name);
    }

    if let Some(gid) = env_u32("SUDO_GID")
        && let Ok(Some(group)) = Group::from_gid(Gid::from_raw(gid))
    {
        return Ok(group.name);
    }

    bail!("unable to resolve the invoking user's service group; use sudo or pkexec")
}

#[cfg(target_os = "macos")]
fn set_macos_owner(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::lchown;

    lchown(path, Some(0), Some(0)).with_context(|| format!("failed to set root:wheel owner on {path:?}"))
}

#[cfg(target_os = "macos")]
fn set_macos_owner_recursive(path: &Path) -> Result<(), Error> {
    set_macos_owner(path)?;
    if !std::fs::symlink_metadata(path)?.file_type().is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        set_macos_owner_recursive(&entry?.path())?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_macos_permissions(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set mode {mode:o} on {path:?}"))
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Error> {
    if run_maintenance_if_requested()? {
        return Ok(());
    }
    if run_core_install_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let debug = std::env::args().any(|arg| arg == "--debug");
    let launchd_install_plan = probe_launchd_service(debug)?;
    let service_binary_path = bundled_service_binary()?;

    let bundle_path = PathBuf::from("/Library/PrivilegedHelperTools")
        .join(format!("{}.bundle", clash_verge_service_ipc::MACOS_SERVICE_ID));
    let contents_path = bundle_path.join("Contents");
    let macos_path = contents_path.join("MacOS");

    std::fs::create_dir_all(&macos_path).map_err(|e| anyhow::anyhow!("Failed to create bundle directories: {}", e))?;

    let target_binary_path = macos_path.join("clash-verge-service");
    let staged = stage_binary(&service_binary_path, &target_binary_path)?;

    let info_plist_path = contents_path.join("Info.plist");

    let plist_dir = PathBuf::from("/Library/LaunchDaemons");
    if !plist_dir.exists() {
        std::fs::create_dir(&plist_dir).map_err(|e| anyhow::anyhow!("Failed to create plist directory: {}", e))?;
    }

    let plist_file = plist_dir.join(format!("{}.plist", clash_verge_service_ipc::MACOS_SERVICE_ID));

    let launchd_plist_content = format!(
        include_str!("../../resources/launchd.plist.tmpl"),
        group_name = resolve_service_group_name()?,
        service_id = clash_verge_service_ipc::MACOS_SERVICE_ID,
        app_bundle_id = clash_verge_service_ipc::MACOS_APP_BUNDLE_ID,
        service_binary = target_binary_path.to_string_lossy(),
    );
    let info_plist_content = format!(
        include_str!("../../resources/info.plist.tmpl"),
        display_name = clash_verge_service_ipc::SERVICE_DISPLAY_NAME,
        service_id = clash_verge_service_ipc::MACOS_SERVICE_ID,
    );
    let plist_path = plist_file.to_string_lossy().into_owned();

    if launchd_install_plan == LaunchdInstallPlan::Bootout {
        run_command("launchctl", &["bootout", "system", &plist_path], debug)?;
    }
    // Staged where the service is already down, so a core it was running no longer holds its file.
    install_bundled_cores()?;
    publish_staged_binary(&staged, &target_binary_path)?;
    std::fs::write(&info_plist_path, info_plist_content)
        .with_context(|| format!("failed to write Info.plist {info_plist_path:?}"))?;
    File::create(&plist_file)
        .and_then(|mut file| file.write_all(launchd_plist_content.as_bytes()))
        .map_err(|e| anyhow::anyhow!("Failed to write plist file: {}", e))?;

    set_macos_permissions(&plist_file, 0o644)?;
    set_macos_owner(&plist_file)?;
    set_macos_permissions(&target_binary_path, 0o544)?;
    set_macos_owner(&target_binary_path)?;
    set_macos_permissions(&bundle_path, 0o755)?;
    set_macos_owner_recursive(&bundle_path)?;

    let launchd_target = launchd_service_target();
    run_command("launchctl", &["enable", &launchd_target], debug)?;
    run_command("launchctl", &["bootstrap", "system", &plist_path], debug)?;
    run_command(
        "launchctl",
        &["start", clash_verge_service_ipc::MACOS_SERVICE_ID],
        debug,
    )?;
    wait_for_service_ready()?;
    #[cfg(not(feature = "development-channel"))]
    let _ = uninstall_old_service();

    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Error> {
    if run_maintenance_if_requested()? {
        return Ok(());
    }
    if run_core_install_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let debug = std::env::args().any(|arg| arg == "--debug");
    let source = bundled_service_binary()?;
    let install_dir = clash_verge_service_ipc::prepare_service_install_directory()?;
    let target = install_dir.join("clash-verge-service");
    let staged = stage_binary(&source, &target)?;
    let unit_name = format!("{}.service", clash_verge_service_ipc::SERVICE_SLUG);
    let unit_path = PathBuf::from("/etc/systemd/system").join(&unit_name);

    let _ = run_command("systemctl", &["stop", &unit_name], debug);
    // Staged where the service is already down, so a core it was running no longer holds its file.
    install_bundled_cores()?;
    publish_staged_binary(&staged, &target)?;

    let unit_file_content = format!(
        include_str!("../../resources/systemd_service_unit.tmpl"),
        exec_start = target.to_string_lossy(),
        group = resolve_service_group_name()?,
        runtime_directory = clash_verge_service_ipc::SERVICE_SLUG,
    );

    let mut unit_file =
        File::create(&unit_path).with_context(|| format!("failed to create systemd unit {unit_path:?}"))?;
    unit_file
        .write_all(unit_file_content.as_bytes())
        .with_context(|| format!("failed to write systemd unit {unit_path:?}"))?;
    unit_file
        .sync_all()
        .with_context(|| format!("failed to sync systemd unit {unit_path:?}"))?;

    run_command("systemctl", &["daemon-reload"], debug)?;
    run_command("systemctl", &["enable", &unit_name], debug)?;
    run_command("systemctl", &["start", &unit_name], debug)?;
    wait_for_service_ready()?;

    Ok(())
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use platform_lib::{
        Error as WindowsServiceError,
        service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use std::ffi::{OsStr, OsString};
    use std::{thread, time::Duration};

    if run_maintenance_if_requested()? {
        return Ok(());
    }
    if run_core_install_if_requested()? {
        return Ok(());
    }
    let _gate = enter_repair_gate()?;
    let source = bundled_service_binary()?;
    let install_dir = clash_verge_service_ipc::prepare_service_install_directory()?;
    let target = install_dir.join("clash-verge-service.exe");
    let staged = stage_binary(&source, &target)?;

    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;
    let start_type = if cfg!(feature = "development-channel") {
        ServiceStartType::OnDemand
    } else {
        ServiceStartType::AutoStart
    };
    let service_info = ServiceInfo {
        name: OsString::from(clash_verge_service_ipc::WINDOWS_SERVICE_NAME),
        display_name: OsString::from(clash_verge_service_ipc::SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type,
        error_control: ServiceErrorControl::Normal,
        executable_path: target.clone(),
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    let service_access =
        ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::CHANGE_CONFIG;
    match service_manager.open_service(clash_verge_service_ipc::WINDOWS_SERVICE_NAME, service_access) {
        Ok(service) => {
            const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
            let status = service.query_status()?;
            if status.current_state != ServiceState::Stopped {
                if let Err(error) = service.stop()
                    && !matches!(
                        &error,
                        WindowsServiceError::Winapi(error)
                            if error.raw_os_error() == Some(ERROR_SERVICE_NOT_ACTIVE)
                    )
                {
                    return Err(error.into());
                }
                for _ in 0..200 {
                    if service.query_status()?.current_state == ServiceState::Stopped {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if service.query_status()?.current_state != ServiceState::Stopped {
                    bail!("timed out waiting for service to stop before replacement");
                }
            }

            // Staged where the service is already down, so a core it was running no longer holds
            // its file open.
            install_bundled_cores()?;
            publish_staged_binary(&staged, &target)?;
            service.change_config(&service_info)?;
            configure_windows_service_recovery(&service)?;
            service.start(&Vec::<&OsStr>::new())?;
            wait_for_service_ready()?;
            return Ok(());
        }
        Err(WindowsServiceError::Winapi(error)) if error.raw_os_error() == Some(1060) => {}
        Err(error) => return Err(error.into()),
    }

    install_bundled_cores()?;
    publish_staged_binary(&staged, &target)?;
    let start_access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
    let service = service_manager.create_service(&service_info, start_access)?;

    service.set_description("Clash Verge Service helps to launch Clash Core")?;
    configure_windows_service_recovery(&service)?;
    service.start(&Vec::<&OsStr>::new())?;
    wait_for_service_ready()?;

    Ok(())
}

#[cfg(windows)]
fn configure_windows_service_recovery(service: &platform_lib::service::Service) -> platform_lib::Result<()> {
    use platform_lib::service::{ServiceAction, ServiceActionType, ServiceFailureActions, ServiceFailureResetPeriod};
    use std::time::Duration;

    let actions = [5, 10, 30]
        .into_iter()
        .map(|delay_secs| ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(delay_secs),
        })
        .collect();

    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(actions),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;

    Ok(())
}

#[cfg(test)]
mod install_core_tests {
    use super::install_core;
    use std::path::PathBuf;

    fn scratch(label: &str) -> anyhow::Result<PathBuf> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!("install-core-{label}-{}-{timestamp}", std::process::id()));
        std::fs::create_dir_all(root.join("cores"))?;
        Ok(root)
    }

    #[test]
    fn publishes_the_bytes_and_stamps_the_source_modified_time() -> anyhow::Result<()> {
        let root = scratch("stamp")?;
        let source = root.join("verge-mihomo");
        std::fs::write(&source, b"core bytes")?;

        let target = install_core(&source, &root.join("cores"), source.file_name().unwrap(), None)?;

        assert_eq!(std::fs::read(&target)?, b"core bytes");
        assert_eq!(
            std::fs::metadata(&source)?.modified()?,
            std::fs::metadata(&target)?.modified()?,
            "the copy must carry the source's modified time for drift detection"
        );
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_wrong_attestation_publishes_nothing() -> anyhow::Result<()> {
        let root = scratch("attestation")?;
        let source = root.join("verge-mihomo");
        std::fs::write(&source, b"core bytes")?;

        let error = install_core(
            &source,
            &root.join("cores"),
            source.file_name().unwrap(),
            Some(&[0u8; 32]),
        )
        .expect_err("must refuse");

        assert!(error.to_string().contains("attested sha256"), "got {error:#}");
        assert!(
            std::fs::read_dir(root.join("cores"))?.next().is_none(),
            "nothing may land in the core directory on a failed attestation"
        );
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn an_unchanged_core_is_recognized_without_republishing() -> anyhow::Result<()> {
        let root = scratch("idempotent")?;
        let source = root.join("verge-mihomo");
        std::fs::write(&source, b"core bytes")?;
        let cores = root.join("cores");

        let first = install_core(&source, &cores, source.file_name().unwrap(), None)?;
        let second = install_core(&source, &cores, source.file_name().unwrap(), None)?;

        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second)?, b"core bytes");
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_core_named_like_a_bookkeeping_leftover_is_refused() -> anyhow::Result<()> {
        let root = scratch("bookkeeping-name")?;
        for name in ["verge-mihomo.next", "verge-mihomo.old"] {
            let source = root.join(name);
            std::fs::write(&source, b"core bytes")?;

            let error =
                install_core(&source, &root.join("cores"), source.file_name().unwrap(), None).expect_err("must refuse");

            assert!(error.to_string().contains("bookkeeping suffix"), "got {error:#}");
        }
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }
}

#[cfg(test)]
mod core_install_argument_tests {
    use super::{parse_core_install_arguments, parse_sha256_hex};
    use std::ffi::OsString;

    fn parse(arguments: &[&str]) -> anyhow::Result<Vec<super::CoreInstallRequest>> {
        parse_core_install_arguments(arguments.iter().map(OsString::from))
    }

    #[test]
    fn each_digest_attests_the_core_it_follows() -> anyhow::Result<()> {
        let digest = "ab".repeat(32);
        let requests = parse(&[
            "--install-core",
            "/dl/verge-mihomo",
            "--sha256",
            &digest,
            "--install-core",
            "/dl/other",
        ])?;

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].source, std::path::Path::new("/dl/verge-mihomo"));
        assert_eq!(requests[0].sha256, Some([0xab; 32]));
        assert_eq!(requests[1].sha256, None);
        Ok(())
    }

    #[test]
    fn a_digest_with_no_core_to_attest_is_refused() {
        assert!(parse(&["--sha256", &"ab".repeat(32)]).is_err());
    }

    #[test]
    fn a_second_digest_for_the_same_core_is_refused() {
        let digest = "ab".repeat(32);
        assert!(parse(&["--install-core", "/dl/core", "--sha256", &digest, "--sha256", &digest]).is_err());
    }

    #[test]
    fn a_missing_path_or_digest_value_is_refused() {
        assert!(parse(&["--install-core"]).is_err());
        assert!(parse(&["--install-core", "/dl/core", "--sha256"]).is_err());
    }

    #[test]
    fn digests_must_be_exactly_64_hex_characters() {
        assert!(parse_sha256_hex(&"ab".repeat(32)).is_ok());
        assert!(parse_sha256_hex(&"AB".repeat(32)).is_ok());
        assert!(parse_sha256_hex("ab").is_err());
        assert!(parse_sha256_hex(&"zz".repeat(32)).is_err());
        assert!(parse_sha256_hex(&format!("{}g", "ab".repeat(31))).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_launchd_service_skips_bootout() {
        let plan = classify_launchd_service_probe(
            Some(113),
            "Could not find service \"io.github.clash-verge-rev.clash-verge-rev.service\" in domain for system",
        )
        .unwrap();

        assert_eq!(plan, LaunchdInstallPlan::SkipBootout);
    }

    #[test]
    fn loaded_launchd_service_runs_bootout() {
        let plan = classify_launchd_service_probe(Some(0), "").unwrap();

        assert_eq!(plan, LaunchdInstallPlan::Bootout);
    }

    #[test]
    fn unexpected_launchd_exit_is_an_error() {
        let result = classify_launchd_service_probe(Some(5), "Could not find service");

        assert!(result.is_err());
    }

    #[test]
    fn unexpected_launchd_diagnostic_is_an_error() {
        let result = classify_launchd_service_probe(Some(113), "Operation not permitted");

        assert!(result.is_err());
    }
}
