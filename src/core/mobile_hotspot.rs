use crate::{MobileHotspotCompatibilityOutcome, MobileHotspotCompatibilityRequest, OwnerIdentity};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Deserializer};
use std::cell::RefCell;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};
use tracing::{debug, info};
use windows::Networking::Connectivity::{ConnectionProfile, NetworkInformation};
use windows::Networking::NetworkOperators::{
    NetworkOperatorTetheringManager, NetworkOperatorTetheringOperationResult, TetheringCapability,
    TetheringOperationStatus, TetheringOperationalState,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

const SCRIPT: &str = include_str!("windows_mobile_hotspot.ps1");
const SNAPSHOT_FILE_NAME: &str = "mobile-hotspot-ics-backup.json";
const LEGACY_WINRT_SESSION_FILE_NAME: &str = "mobile-hotspot-winrt-session.json";
const ICS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const TETHERING_TIMEOUT: Duration = Duration::from_secs(30);
const ICS_WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const ICS_WORKER_ACTION_TIMEOUT: Duration = Duration::from_secs(45);
const ICS_WORKER_BOOTSTRAP: &str = r#"
$ErrorActionPreference = 'Stop'
$scriptText = [Console]::In.ReadLine() | ConvertFrom-Json
[Console]::Out.WriteLine('{"workerReady":true}')
while (($workerLine = [Console]::In.ReadLine()) -ne $null) {
  try {
    $workerCommand = $workerLine | ConvertFrom-Json
    $env:CLASH_VERGE_HOTSPOT_REQUEST = [string]$workerCommand.requestJson
    $env:CLASH_VERGE_HOTSPOT_BACKUP = [string]$workerCommand.backupPath
    $env:CLASH_VERGE_HOTSPOT_ACTION = [string]$workerCommand.action
    $workerOutput = @(& ([ScriptBlock]::Create($scriptText)))
    if ($workerOutput.Count -ne 1) {
      throw "ICS helper returned $($workerOutput.Count) output values."
    }
    [Console]::Out.WriteLine([string]$workerOutput[0])
  } catch {
    [Console]::Out.WriteLine((@{ workerError = $_.Exception.ToString() } | ConvertTo-Json -Compress))
  }
}
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IcsSnapshot {
    version: u8,
    mode: String,
    active: bool,
    previous_public: Option<String>,
    applied_private: String,
    abandoned: bool,
}

#[derive(Debug, Deserialize)]
struct IcsActionResponse {
    outcome: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    public: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    private: Vec<String>,
    #[serde(default)]
    transition: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringList {
    One(String),
    Many(Vec<String>),
}

fn deserialize_string_list<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<StringList>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(StringList::One(value)) => vec![value],
        Some(StringList::Many(values)) => values,
    })
}

struct WinRtWork {
    owner_root: PathBuf,
    request: MobileHotspotCompatibilityRequest,
    response: mpsc::SyncSender<Result<MobileHotspotCompatibilityOutcome>>,
}

#[derive(Default)]
struct WinRtWorkerState {
    enabled_manager: Option<CachedTetheringManager>,
}

struct CachedTetheringManager {
    profile_name: String,
    // Keep the profile alive with the manager. Both objects belong to the
    // persistent WinRT apartment and are discarded together when TUN or the
    // compatibility feature is disabled.
    _profile: ConnectionProfile,
    manager: NetworkOperatorTetheringManager,
}

impl WinRtWorkerState {
    fn enabled_manager(&mut self, profile_name: &str) -> Result<NetworkOperatorTetheringManager> {
        if let Some(cached) = self
            .enabled_manager
            .as_ref()
            .filter(|cached| cached.profile_name == profile_name)
        {
            return Ok(cached.manager.clone());
        }

        let profile = find_unique_profile(profile_name)?;
        let manager = create_manager(&profile, profile_name)?;
        self.enabled_manager = Some(CachedTetheringManager {
            profile_name: profile_name.to_owned(),
            _profile: profile,
            manager: manager.clone(),
        });
        info!(
            profile_name,
            "Cached the Mobile Hotspot profile and manager on the persistent WinRT worker"
        );
        Ok(manager)
    }

    fn clear_enabled_manager(&mut self) {
        if self.enabled_manager.take().is_some() {
            debug!("Discarded the cached Mobile Hotspot profile and manager");
        }
    }
}

static WINRT_WORKER: OnceLock<Result<mpsc::Sender<WinRtWork>, String>> = OnceLock::new();

thread_local! {
    static ICS_POWERSHELL_WORKER: RefCell<Option<IcsPowerShell>> = const { RefCell::new(None) };
}

struct IcsPowerShell {
    child: Child,
    stdin: ChildStdin,
    responses: mpsc::Receiver<std::result::Result<String, String>>,
}

impl IcsPowerShell {
    fn start() -> Result<Self> {
        let powershell = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let mut child = Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Sta",
                "-Command",
                ICS_WORKER_BOOTSTRAP,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start the persistent Windows PowerShell ICS worker")?;
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open persistent ICS worker stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to open persistent ICS worker stdout")?;

        writeln!(stdin, "{}", serde_json::to_string(SCRIPT)?)?;
        stdin.flush()?;

        let (response_tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match stdout.read_line(&mut line) {
                    Ok(0) => {
                        let _ = response_tx.send(Err("persistent ICS worker closed its response stream".to_owned()));
                        break;
                    }
                    Ok(_) => {
                        if response_tx
                            .send(Ok(line.trim_end_matches(['\r', '\n']).to_owned()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ =
                            response_tx.send(Err(format!("failed to read persistent ICS worker response: {error}")));
                        break;
                    }
                }
            }
        });
        let ready = match responses.recv_timeout(ICS_WORKER_STARTUP_TIMEOUT) {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{error}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "persistent ICS worker did not report readiness within {:?}",
                    ICS_WORKER_STARTUP_TIMEOUT
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("persistent ICS worker startup reader stopped unexpectedly");
            }
        };
        let value: serde_json::Value =
            serde_json::from_str(&ready).context("persistent ICS worker returned an invalid startup response")?;
        if value.get("workerReady").and_then(serde_json::Value::as_bool) != Some(true) {
            bail!("persistent ICS worker did not report readiness: {ready}");
        }
        Ok(Self {
            child,
            stdin,
            responses,
        })
    }

    fn request(
        &mut self,
        snapshot_path: &Path,
        request: &MobileHotspotCompatibilityRequest,
        action: &str,
    ) -> Result<IcsActionResponse> {
        let request_json = serde_json::to_string(request)?;
        let command = serde_json::json!({
            "requestJson": request_json,
            "backupPath": snapshot_path,
            "action": action,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&command)?)
            .context("failed to send a command to the persistent ICS worker")?;
        self.stdin
            .flush()
            .context("failed to flush a command to the persistent ICS worker")?;
        let encoded = self.read_response_line(&format!("Windows ICS action {action:?}"))?;
        let value: serde_json::Value = serde_json::from_str(&encoded)
            .with_context(|| format!("Windows ICS action {action:?} returned invalid JSON"))?;
        if let Some(message) = value.get("workerError").and_then(serde_json::Value::as_str) {
            bail!("Windows ICS action {action:?} failed: {message}");
        }
        serde_json::from_value(value)
            .with_context(|| format!("Windows ICS action {action:?} returned an invalid response"))
    }

    fn read_response_line(&mut self, context: &str) -> Result<String> {
        match self.responses.recv_timeout(ICS_WORKER_ACTION_TIMEOUT) {
            Ok(Ok(line)) => Ok(line),
            Ok(Err(error)) => {
                let status = self.child.try_wait().ok().flatten();
                bail!("{error} while waiting for {context} (status={status:?})")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                bail!("{context} did not complete within {ICS_WORKER_ACTION_TIMEOUT:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = self.child.try_wait().ok().flatten();
                bail!("persistent ICS worker response reader stopped while waiting for {context} (status={status:?})")
            }
        }
    }
}

impl Drop for IcsPowerShell {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub async fn set_mobile_hotspot_compatibility(
    owner_identity: &OwnerIdentity,
    request: &MobileHotspotCompatibilityRequest,
) -> Result<MobileHotspotCompatibilityOutcome> {
    let started_at = Instant::now();
    let owner_paths = crate::core::paths::ensure_owner_state_directory(owner_identity)
        .context("failed to prepare private service state for Mobile Hotspot compatibility")?;
    let owner_root = owner_paths.root().to_owned();
    let request = request.clone();
    let enabled = request.enabled;
    let outcome = tokio::task::spawn_blocking(move || run_on_winrt_worker(owner_root, request))
        .await
        .context("Windows Mobile Hotspot worker terminated unexpectedly")??;

    match outcome {
        MobileHotspotCompatibilityOutcome::Applied | MobileHotspotCompatibilityOutcome::Restored => info!(
            enabled,
            ?outcome,
            elapsed_ms = started_at.elapsed().as_millis(),
            "Reconciled Windows Mobile Hotspot compatibility through WinRT and HNetCfg"
        ),
        MobileHotspotCompatibilityOutcome::Unchanged | MobileHotspotCompatibilityOutcome::WaitingForHotspot => {
            debug!(
                enabled,
                ?outcome,
                elapsed_ms = started_at.elapsed().as_millis(),
                "Observed stable Windows Mobile Hotspot compatibility state"
            )
        }
    }
    Ok(outcome)
}

fn run_on_winrt_worker(
    owner_root: PathBuf,
    request: MobileHotspotCompatibilityRequest,
) -> Result<MobileHotspotCompatibilityOutcome> {
    let sender = WINRT_WORKER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<WinRtWork>();
            std::thread::Builder::new()
                .name("mobile-hotspot-winrt".to_owned())
                .spawn(move || {
                    // windows-rs caches WinRT activation factories for the process. Keep one
                    // MTA alive for the lifetime of the service and execute every hotspot call
                    // on that same thread; repeatedly uninitializing temporary Tokio blocking
                    // threads leaves those cached factories pointing at a torn-down apartment.
                    let apartment = WinRtApartment::initialize()
                        .map_err(|error| format!("failed to initialize the Mobile Hotspot WinRT worker: {error:#}"));
                    let mut state = WinRtWorkerState::default();
                    while let Ok(work) = receiver.recv() {
                        let result = match &apartment {
                            Ok(_) => reconcile(&work.owner_root, &work.request, &mut state),
                            Err(message) => Err(anyhow::anyhow!(message.clone())),
                        };
                        let _ = work.response.send(result);
                    }
                })
                .map_err(|error| format!("failed to start the Mobile Hotspot WinRT worker: {error}"))?;
            Ok(sender)
        })
        .as_ref()
        .map_err(|message| anyhow::anyhow!(message.clone()))?;

    let (response, receiver) = mpsc::sync_channel(1);
    sender
        .send(WinRtWork {
            owner_root,
            request,
            response,
        })
        .context("Mobile Hotspot WinRT worker is unavailable")?;
    receiver
        .recv()
        .context("Mobile Hotspot WinRT worker stopped before returning a result")?
}

fn reconcile(
    owner_root: &Path,
    request: &MobileHotspotCompatibilityRequest,
    worker_state: &mut WinRtWorkerState,
) -> Result<MobileHotspotCompatibilityOutcome> {
    remove_legacy_winrt_session(owner_root)?;
    let snapshot_path = owner_root.join(SNAPSHOT_FILE_NAME);
    let snapshot = load_snapshot(&snapshot_path)?;

    if !request.enabled {
        worker_state.clear_enabled_manager();
        return restore_disabled(&snapshot_path, snapshot.as_ref(), request);
    }

    let manager = worker_state.enabled_manager(&request.tun_device_name)?;
    let state = match stable_state(&manager) {
        Ok(state) => state,
        Err(error) => {
            // A TUN recreation can invalidate its old WinRT objects. Do not
            // keep a failing manager; the next one-second observation rebuilds
            // the profile and manager on this same apartment.
            worker_state.clear_enabled_manager();
            return Err(error).context("cached Mobile Hotspot manager became unavailable");
        }
    };
    match state {
        TetheringOperationalState::On => run_reconcile_script(&snapshot_path, request),
        TetheringOperationalState::Off => {
            let Some(snapshot) = snapshot.as_ref().filter(|snapshot| snapshot.active) else {
                return if snapshot.as_ref().is_some_and(|snapshot| snapshot.abandoned) {
                    run_reconcile_script(&snapshot_path, request)
                } else {
                    let response = run_ics_action(&snapshot_path, request, "prewarm")?;
                    if response.outcome != "waiting_for_hotspot" {
                        bail!(
                            "Windows ICS prewarm returned an unexpected outcome {:?}",
                            response.outcome
                        );
                    }
                    Ok(MobileHotspotCompatibilityOutcome::WaitingForHotspot)
                };
            };

            // WinRT is authoritative for the hotspot lifecycle. The restoration
            // path separately waits until the exact saved HNetCfg PRIVATE GUID
            // disappears before it mutates ICS, so an additional fixed delay and
            // repeated state query do not add a useful safety condition.
            restore_after_user_stopped_hotspot(&snapshot_path, snapshot)
        }
        state => bail!("Windows returned an unknown Mobile Hotspot state ({})", state.0),
    }
}

fn restore_disabled(
    snapshot_path: &Path,
    snapshot: Option<&IcsSnapshot>,
    request: &MobileHotspotCompatibilityRequest,
) -> Result<MobileHotspotCompatibilityOutcome> {
    let Some(snapshot) = snapshot else {
        return Ok(MobileHotspotCompatibilityOutcome::Unchanged);
    };
    validate_snapshot(snapshot)?;
    if snapshot.abandoned {
        remove_snapshot(snapshot_path)?;
        return Ok(MobileHotspotCompatibilityOutcome::Unchanged);
    }
    if !snapshot.active {
        remove_snapshot(snapshot_path)?;
        return Ok(MobileHotspotCompatibilityOutcome::Unchanged);
    }

    let state_profile =
        find_unique_profile(&request.tun_device_name).or_else(|_| find_previous_public_profile(snapshot))?;
    let manager = create_manager(&state_profile, "current Mobile Hotspot state")?;
    match stable_state(&manager)? {
        TetheringOperationalState::On => {
            let response = run_ics_action(snapshot_path, request, "restore_live")?;
            if response.outcome != "restored" {
                bail!("refused to overwrite an externally changed live ICS topology");
            }
            remove_snapshot(snapshot_path)?;
            Ok(MobileHotspotCompatibilityOutcome::Restored)
        }
        TetheringOperationalState::Off => restore_after_user_stopped_hotspot(snapshot_path, snapshot),
        state => bail!("Windows returned an unknown Mobile Hotspot state ({})", state.0),
    }
}

fn restore_after_user_stopped_hotspot(
    snapshot_path: &Path,
    snapshot: &IcsSnapshot,
) -> Result<MobileHotspotCompatibilityOutcome> {
    validate_snapshot(snapshot)?;
    if snapshot.abandoned {
        bail!("cannot restore an abandoned Mobile Hotspot compatibility snapshot");
    }
    let previous_profile = find_previous_public_profile(snapshot)?;
    let previous_name = previous_profile
        .ProfileName()
        .context("failed to read the previous Mobile Hotspot public profile name")?
        .to_string();
    let request = MobileHotspotCompatibilityRequest {
        enabled: false,
        tun_device_name: previous_name.clone(),
    };
    let wait_started = Instant::now();
    wait_for_private_adapter_shutdown(snapshot_path, &request, snapshot)?;
    info!(
        elapsed_ms = wait_started.elapsed().as_millis(),
        "Completed stopped-hotspot private-adapter shutdown wait"
    );

    let manager = create_manager(&previous_profile, &previous_name)?;
    if stable_state(&manager)? == TetheringOperationalState::Off {
        info!(
            previous_profile = %previous_name,
            "Temporarily restarting Mobile Hotspot through the saved public profile to recreate its private adapter"
        );
        let start_started = Instant::now();
        start_tethering(&manager, &previous_name)?;
        info!(
            elapsed_ms = start_started.elapsed().as_millis(),
            "Completed temporary WLAN-backed Mobile Hotspot startup"
        );
    }

    let restore_started = Instant::now();
    let restore_result = run_ics_action(snapshot_path, &request, "restore_live");
    info!(
        elapsed_ms = restore_started.elapsed().as_millis(),
        "Completed temporary-session HNetCfg restore attempt"
    );
    let stop_started = Instant::now();
    let stop_result = stop_tethering(&manager, &previous_name);
    info!(
        elapsed_ms = stop_started.elapsed().as_millis(),
        "Completed temporary WLAN-backed Mobile Hotspot stop attempt"
    );

    if let Err(error) = stop_result {
        return Err(error).context("failed to stop the temporary WLAN-backed Mobile Hotspot restoration session");
    }
    let response =
        restore_result.context("failed to restore the saved ICS pair inside the temporary hotspot session")?;
    if response.outcome != "restored" {
        bail!("refused to overwrite an externally changed ICS topology during stopped-hotspot restoration");
    }

    let final_state = stable_state(&manager)?;
    if final_state != TetheringOperationalState::Off {
        bail!(
            "temporary Mobile Hotspot restoration session did not stop (state={})",
            final_state.0
        );
    }

    let expected_public = snapshot
        .previous_public
        .as_deref()
        .context("the compatibility snapshot has no previous public adapter")?;
    let final_inspect_started = Instant::now();
    let final_action = format!("inspect_stopped:{}|{}", expected_public, snapshot.applied_private);
    let observed = run_ics_action(snapshot_path, &request, &final_action)?;
    info!(
        elapsed_ms = final_inspect_started.elapsed().as_millis(),
        "Completed final stopped-hotspot ICS verification"
    );
    if !contains_guid(&observed.public, expected_public) || !observed.private.is_empty() {
        bail!(
            "final ICS verification failed after stopping the temporary hotspot (public={:?}, private={:?})",
            observed.public,
            observed.private
        );
    }

    remove_snapshot(snapshot_path)?;
    info!(
        previous_profile = %previous_name,
        previous_public = %expected_public,
        "Restored the saved WLAN public interface through a temporary WinRT hotspot session"
    );
    Ok(MobileHotspotCompatibilityOutcome::Restored)
}

fn wait_for_private_adapter_shutdown(
    snapshot_path: &Path,
    request: &MobileHotspotCompatibilityRequest,
    snapshot: &IcsSnapshot,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let action = format!("inspect_private_role:{}", snapshot.applied_private);
        let observed = run_ics_action(snapshot_path, request, &action)?;
        if !contains_guid(&observed.private, &snapshot.applied_private) {
            info!(
                applied_private = %snapshot.applied_private,
                elapsed_ms = started.elapsed().as_millis(),
                "Confirmed that Windows finished removing the stopped Mobile Hotspot private adapter"
            );
            return Ok(());
        }
        if started.elapsed() >= ICS_SHUTDOWN_TIMEOUT {
            bail!(
                "the stopped Mobile Hotspot private adapter {:?} remained in HNetCfg for 30 seconds",
                snapshot.applied_private
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run_reconcile_script(
    snapshot_path: &Path,
    request: &MobileHotspotCompatibilityRequest,
) -> Result<MobileHotspotCompatibilityOutcome> {
    let response = run_ics_action(snapshot_path, request, "reconcile")?;
    match response.outcome.as_str() {
        "applied" => Ok(MobileHotspotCompatibilityOutcome::Applied),
        "restored" => Ok(MobileHotspotCompatibilityOutcome::Restored),
        "unchanged" => Ok(MobileHotspotCompatibilityOutcome::Unchanged),
        "waiting_for_hotspot" => Ok(MobileHotspotCompatibilityOutcome::WaitingForHotspot),
        other => bail!("Windows ICS helper returned an unknown outcome {other:?}"),
    }
}

fn run_ics_action(
    snapshot_path: &Path,
    request: &MobileHotspotCompatibilityRequest,
    action: &str,
) -> Result<IcsActionResponse> {
    let response = ICS_POWERSHELL_WORKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(IcsPowerShell::start()?);
        }
        let result = slot
            .as_mut()
            .context("persistent ICS worker was not initialized")?
            .request(snapshot_path, request, action);
        if result.is_err() {
            // Dropping the wedged process also terminates its STA runspaces. The
            // next monitor pass starts a clean worker instead of permanently
            // blocking every later reconciliation request.
            *slot = None;
        }
        result
    })?;
    info!(
        action,
        outcome = %response.outcome,
        public = ?response.public,
        private = ?response.private,
        transition = ?response.transition,
        "Completed Windows HNetCfg action"
    );
    Ok(response)
}

fn load_snapshot(path: &Path) -> Result<Option<IcsSnapshot>> {
    match std::fs::read(path) {
        Ok(encoded) => {
            // Windows PowerShell 5.1 writes `-Encoding UTF8` with a UTF-8 BOM.
            // serde_json expects the JSON value itself at byte zero.
            let encoded = encoded.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&encoded);
            let snapshot: IcsSnapshot = serde_json::from_slice(encoded)
                .with_context(|| format!("failed to read Mobile Hotspot snapshot at {path:?}"))?;
            validate_snapshot(&snapshot)?;
            Ok(Some(snapshot))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to open Mobile Hotspot snapshot at {path:?}")),
    }
}

fn validate_snapshot(snapshot: &IcsSnapshot) -> Result<()> {
    if snapshot.version != 1 || snapshot.mode != "paired" {
        bail!(
            "unsupported Mobile Hotspot compatibility snapshot version={} mode={:?}",
            snapshot.version,
            snapshot.mode
        );
    }
    Ok(())
}

fn remove_snapshot(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove Mobile Hotspot snapshot at {path:?}")),
    }
}

fn remove_legacy_winrt_session(owner_root: &Path) -> Result<()> {
    let path = owner_root.join(LEGACY_WINRT_SESSION_FILE_NAME);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            info!(path = ?path, "Removed the obsolete WinRT-only Mobile Hotspot session file");
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove obsolete Mobile Hotspot session file at {path:?}"))
        }
    }
}

fn find_previous_public_profile(snapshot: &IcsSnapshot) -> Result<ConnectionProfile> {
    let previous_guid = snapshot
        .previous_public
        .as_deref()
        .context("the compatibility snapshot has no previous public adapter")?;
    find_active_profile_by_adapter_guid(previous_guid)
}

fn find_active_profile_by_adapter_guid(adapter_guid: &str) -> Result<ConnectionProfile> {
    let profiles =
        NetworkInformation::GetConnectionProfiles().context("failed to enumerate Windows connection profiles")?;
    let expected = normalize_guid(adapter_guid);
    let mut matches = Vec::new();
    for index in 0..profiles.Size().context("failed to count Windows connection profiles")? {
        let profile = profiles
            .GetAt(index)
            .with_context(|| format!("failed to read Windows connection profile {index}"))?;
        if profile
            .GetNetworkConnectivityLevel()
            .context("failed to read Windows connection profile connectivity")?
            .0
            == 0
        {
            continue;
        }
        let adapter = profile
            .NetworkAdapter()
            .context("failed to read a Windows connection profile adapter")?;
        let actual = normalize_guid(&format!("{:?}", adapter.NetworkAdapterId()?));
        if actual == expected {
            matches.push(profile);
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        count => {
            bail!("expected exactly one active Windows connection profile for adapter {adapter_guid:?}, found {count}")
        }
    }
}

fn find_unique_profile(profile_name: &str) -> Result<ConnectionProfile> {
    let profiles =
        NetworkInformation::GetConnectionProfiles().context("failed to enumerate Windows connection profiles")?;
    let mut matches = Vec::new();
    for index in 0..profiles.Size().context("failed to count Windows connection profiles")? {
        let profile = profiles
            .GetAt(index)
            .with_context(|| format!("failed to read Windows connection profile {index}"))?;
        if profile
            .ProfileName()
            .context("failed to read a Windows connection profile name")?
            .to_string()
            .eq_ignore_ascii_case(profile_name)
        {
            matches.push(profile);
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        count => bail!("expected exactly one Windows connection profile named {profile_name:?}, found {count}"),
    }
}

fn create_manager(profile: &ConnectionProfile, description: &str) -> Result<NetworkOperatorTetheringManager> {
    let capability = NetworkOperatorTetheringManager::GetTetheringCapabilityFromConnectionProfile(profile)
        .with_context(|| format!("failed to query Mobile Hotspot capability for {description:?}"))?;
    if capability != TetheringCapability::Enabled {
        bail!(
            "Windows Mobile Hotspot is unavailable for {description:?} (capability={})",
            capability.0
        );
    }
    NetworkOperatorTetheringManager::CreateFromConnectionProfile(profile)
        .with_context(|| format!("failed to create a Mobile Hotspot manager for {description:?}"))
}

fn stable_state(manager: &NetworkOperatorTetheringManager) -> Result<TetheringOperationalState> {
    let started = Instant::now();
    loop {
        let state = manager
            .TetheringOperationalState()
            .context("failed to query Windows Mobile Hotspot state")?;
        if state != TetheringOperationalState::InTransition {
            return Ok(state);
        }
        if started.elapsed() >= TETHERING_TIMEOUT {
            bail!("Windows Mobile Hotspot remained in transition for 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn start_tethering(manager: &NetworkOperatorTetheringManager, description: &str) -> Result<()> {
    let operation = manager
        .StartTetheringAsync()
        .with_context(|| format!("failed to request Mobile Hotspot startup through {description:?}"))?;
    let started = Instant::now();
    while operation
        .Status()
        .context("failed to query Mobile Hotspot start progress")?
        .0
        == 0
    {
        if started.elapsed() >= TETHERING_TIMEOUT {
            bail!("Windows Mobile Hotspot start timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let result = operation
        .GetResults()
        .context("Windows Mobile Hotspot start did not complete")?;
    match result.Status().context("failed to read Mobile Hotspot start status")? {
        TetheringOperationStatus::Success | TetheringOperationStatus::AlreadyOn => Ok(()),
        status => operation_failed("start", description, status, &result),
    }
}

fn stop_tethering(manager: &NetworkOperatorTetheringManager, description: &str) -> Result<()> {
    if stable_state(manager)? == TetheringOperationalState::Off {
        return Ok(());
    }
    let operation = manager
        .StopTetheringAsync()
        .with_context(|| format!("failed to request Mobile Hotspot shutdown through {description:?}"))?;
    let started = Instant::now();
    while operation
        .Status()
        .context("failed to query Mobile Hotspot stop progress")?
        .0
        == 0
    {
        if started.elapsed() >= TETHERING_TIMEOUT {
            bail!("Windows Mobile Hotspot stop timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let result = operation
        .GetResults()
        .context("Windows Mobile Hotspot stop did not complete")?;
    match result.Status().context("failed to read Mobile Hotspot stop status")? {
        TetheringOperationStatus::Success => Ok(()),
        status => operation_failed("stop", description, status, &result),
    }
}

fn operation_failed(
    operation: &str,
    description: &str,
    status: TetheringOperationStatus,
    result: &NetworkOperatorTetheringOperationResult,
) -> Result<()> {
    let detail = result
        .AdditionalErrorMessage()
        .map(|message| message.to_string())
        .unwrap_or_default();
    if detail.is_empty() {
        bail!(
            "Windows failed to {operation} Mobile Hotspot through {description:?} (status={})",
            status.0
        );
    }
    bail!(
        "Windows failed to {operation} Mobile Hotspot through {description:?} (status={}): {detail}",
        status.0
    )
}

fn contains_guid(guids: &[String], expected: &str) -> bool {
    let expected = normalize_guid(expected);
    guids.iter().any(|guid| normalize_guid(guid) == expected)
}

fn normalize_guid(guid: &str) -> String {
    guid.chars()
        .filter(|character| character.is_ascii_hexdigit())
        .flat_map(char::to_lowercase)
        .collect()
}

struct WinRtApartment;

impl WinRtApartment {
    fn initialize() -> Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .context("failed to initialize Windows Runtime for Mobile Hotspot")?;
        Ok(Self)
    }
}

impl Drop for WinRtApartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::IcsActionResponse;

    #[test]
    fn ics_response_accepts_missing_and_null_lists() {
        let missing: IcsActionResponse = serde_json::from_str(r#"{"outcome":"unchanged"}"#).unwrap();
        assert!(missing.public.is_empty());
        assert!(missing.private.is_empty());

        let null: IcsActionResponse =
            serde_json::from_str(r#"{"outcome":"unchanged","public":null,"private":null}"#).unwrap();
        assert!(null.public.is_empty());
        assert!(null.private.is_empty());
    }

    #[test]
    fn ics_response_accepts_scalar_and_array_lists() {
        let response: IcsActionResponse =
            serde_json::from_str(r#"{"outcome":"unchanged","public":"public-guid","private":["private-guid"]}"#)
                .unwrap();
        assert_eq!(response.public, ["public-guid"]);
        assert_eq!(response.private, ["private-guid"]);
    }
}
