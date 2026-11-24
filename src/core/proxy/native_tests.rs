//! This ignored regression runs only as root on a disposable macOS CI runner.
use crate::{
    MacosProxyConfig, OwnerCredentials, OwnerIdentity, OwnerSessionProof, ProxyApplyOutcome, RuntimeBundle,
    StartClashRequest, get_status, set_system_proxy, start_clash, stop_clash,
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use system_configuration::{
    core_foundation::{
        base::{CFRelease, CFType, TCFType},
        boolean::CFBoolean,
        data::CFData,
        dictionary::{CFDictionary, CFDictionaryRef},
        number::CFNumber,
        propertylist::{create_data, create_with_data, kCFPropertyListBinaryFormat_v1_0},
        string::CFString,
    },
    preferences::SCPreferences,
    sys::{
        network_configuration::{
            SCNetworkProtocolGetConfiguration, SCNetworkProtocolRef, SCNetworkProtocolSetConfiguration,
            SCNetworkServiceCopy, SCNetworkServiceCopyProtocol,
        },
        preferences::{SCPreferencesApplyChanges, SCPreferencesCommitChanges, SCPreferencesLock, SCPreferencesUnlock},
    },
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static BACKUPS: Mutex<Option<NativeBackup>> = Mutex::new(None);

#[derive(Clone, Default, Serialize, Deserialize)]
struct NativeBackup {
    services: Vec<Backup>,
    effective: Option<Vec<u8>>,
}

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCDynamicStoreCopyProxies(store: *const std::ffi::c_void) -> CFDictionaryRef;
}

fn effective_dictionary() -> Result<CFDictionary<CFString, CFType>> {
    let value = unsafe { SCDynamicStoreCopyProxies(std::ptr::null()) };
    ensure!(!value.is_null(), "cannot read effective native proxy settings");
    Ok(unsafe { CFDictionary::wrap_under_create_rule(value) })
}

fn encode_dictionary(dict: &impl TCFType) -> Result<Vec<u8>> {
    create_data(dict.as_CFTypeRef(), kCFPropertyListBinaryFormat_v1_0)
        .map(|data| data.bytes().to_vec())
        .map_err(|error| anyhow::anyhow!("cannot capture native proxy dictionary: {error}"))
}

fn decode_dictionary(bytes: &[u8]) -> Result<CFDictionary> {
    let (value, _) = create_with_data(CFData::from_buffer(bytes), 0)
        .map_err(|error| anyhow::anyhow!("invalid native backup: {error}"))?;
    unsafe { CFType::wrap_under_create_rule(value) }
        .downcast_into::<CFDictionary>()
        .context("backup is not a dictionary")
}

fn effective_matches(port: Option<u16>) -> Result<bool> {
    let dict = effective_dictionary()?;
    let flag = |key: &'static str| -> Result<bool> {
        let Some(value) = dict.find(CFString::from_static_string(key)) else {
            return Ok(false);
        };
        if let Some(number) = value.downcast::<CFNumber>() {
            return Ok(number.to_i64().context("invalid native enable flag")? != 0);
        }
        if let Some(boolean) = value.downcast::<CFBoolean>() {
            return Ok(bool::from(boolean));
        }
        anyhow::bail!("unexpected native enable flag type");
    };
    for (enabled, host, number) in [
        ("HTTPEnable", "HTTPProxy", "HTTPPort"),
        ("HTTPSEnable", "HTTPSProxy", "HTTPSPort"),
        ("SOCKSEnable", "SOCKSProxy", "SOCKSPort"),
    ] {
        if flag(enabled)? != port.is_some() {
            return Ok(false);
        }
        if let Some(port) = port {
            let actual_host = dict
                .find(CFString::from_static_string(host))
                .and_then(|value| value.downcast::<CFString>());
            let actual_port = dict
                .find(CFString::from_static_string(number))
                .and_then(|value| value.downcast::<CFNumber>())
                .and_then(|value| value.to_i64());
            if actual_host.map(|host| host.to_string()).as_deref() != Some("127.0.0.1")
                || actual_port != Some(i64::from(port))
            {
                return Ok(false);
            }
        }
    }
    Ok(!flag("ProxyAutoConfigEnable")?)
}

async fn wait_effective(port: Option<u16>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !effective_matches(port)? {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
struct Backup {
    service: String,
    dictionary: Option<Vec<u8>>,
}

struct NativePreferences {
    preferences: SCPreferences,
    protocol: SCNetworkProtocolRef,
    locked: bool,
}
impl NativePreferences {
    fn open(service: &str, lock: bool) -> Result<Self> {
        let preferences = SCPreferences::default(&CFString::new("Service native proxy regression"));
        let mut native = Self {
            preferences,
            protocol: std::ptr::null(),
            locked: false,
        };
        if lock {
            ensure!(
                unsafe { SCPreferencesLock(native.preferences.as_concrete_TypeRef(), 0) } != 0,
                "cannot lock native preferences for restoration"
            );
            native.locked = true;
        }
        let service = unsafe {
            SCNetworkServiceCopy(
                native.preferences.as_concrete_TypeRef(),
                CFString::new(service).as_concrete_TypeRef(),
            )
        };
        ensure!(!service.is_null(), "recorded network service disappeared");
        native.protocol = unsafe {
            SCNetworkServiceCopyProtocol(service, CFString::from_static_string("Proxies").as_concrete_TypeRef())
        };
        unsafe { CFRelease(service.cast()) };
        ensure!(!native.protocol.is_null(), "recorded service has no proxy protocol");
        Ok(native)
    }
    fn dictionary(&self) -> Option<CFDictionary> {
        let current = unsafe { SCNetworkProtocolGetConfiguration(self.protocol) };
        (!current.is_null()).then(|| unsafe { CFDictionary::wrap_under_get_rule(current) })
    }
    fn restore(&self, data: Option<&[u8]>) -> Result<()> {
        let dictionary = data.map(decode_dictionary).transpose()?;
        ensure!(
            unsafe {
                SCNetworkProtocolSetConfiguration(
                    self.protocol,
                    dictionary
                        .as_ref()
                        .map_or(std::ptr::null(), TCFType::as_concrete_TypeRef),
                )
            } != 0,
            "cannot stage original proxy dictionary"
        );
        ensure!(
            unsafe { SCPreferencesCommitChanges(self.preferences.as_concrete_TypeRef()) } != 0,
            "cannot commit original proxy dictionary"
        );
        ensure!(
            unsafe { SCPreferencesApplyChanges(self.preferences.as_concrete_TypeRef()) } != 0,
            "cannot apply original proxy dictionary"
        );
        ensure!(
            self.dictionary().map(|value| value.as_CFType()) == dictionary.map(|value| value.as_CFType()),
            "original dictionary was not restored exactly"
        );
        Ok(())
    }
}
impl Drop for NativePreferences {
    fn drop(&mut self) {
        if !self.protocol.is_null() {
            unsafe { CFRelease(self.protocol.cast()) };
        }
        if self.locked {
            unsafe { SCPreferencesUnlock(self.preferences.as_concrete_TypeRef()) };
        }
    }
}

fn fixture_root() -> Result<PathBuf> {
    ensure!(
        std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") && unsafe { platform_lib::geteuid() } == 0,
        "native proxy regression requires root on a disposable GitHub runner"
    );
    Ok(PathBuf::from(std::env::var("RUNNER_TEMP")?).join("service-native-proxy-regression"))
}

pub(in crate::core) fn before_proxy_write(receipt: &sysproxy::PrivilegedProxyReceipt) -> Result<()> {
    if !ENABLED.load(Ordering::SeqCst) {
        return Ok(());
    }
    let value = serde_json::to_value(receipt)?;
    let service = value["service_id"]
        .as_str()
        .context("native receipt omitted service identity")?;
    let mut stored = BACKUPS.lock().unwrap();
    let mut backups = stored.clone().unwrap_or_default();
    if backups.services.iter().any(|backup| backup.service == service) {
        return Ok(());
    }
    if backups.effective.is_none() {
        backups.effective = Some(encode_dictionary(&effective_dictionary()?)?);
    }
    // The production writer already holds the preferences lock while invoking this callback.
    let native = NativePreferences::open(service, false)?;
    let dictionary = native.dictionary().as_ref().map(encode_dictionary).transpose()?;
    backups.services.push(Backup {
        service: service.into(),
        dictionary,
    });
    let root = fixture_root()?;
    std::fs::create_dir_all(&root)?;
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    let file = root.join("original-proxy.json");
    let temporary = root.join("original-proxy.json.tmp");
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    output.write_all(&serde_json::to_vec(&backups)?)?;
    output.sync_all()?;
    std::fs::rename(temporary, file)?;
    std::fs::File::open(&root)?.sync_all()?;
    *stored = Some(backups);
    Ok(())
}

fn restore_original_proxy() -> Result<()> {
    let path = fixture_root()?.join("original-proxy.json");
    if !path.exists() {
        return Ok(());
    }
    let backups: NativeBackup = serde_json::from_slice(&std::fs::read(path)?)?;
    for backup in backups.services {
        NativePreferences::open(&backup.service, true)?.restore(backup.dictionary.as_deref())?;
    }
    if let Some(bytes) = backups.effective {
        let expected = decode_dictionary(&bytes)?.as_CFType();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while effective_dictionary()?.as_CFType() != expected {
            ensure!(
                std::time::Instant::now() < deadline,
                "effective native proxy settings were not restored"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    eprintln!("native proxy: original fixed-service dictionaries and effective settings restored");
    Ok(())
}
struct RestoreOriginal;
impl Drop for RestoreOriginal {
    fn drop(&mut self) {
        if let Err(error) = restore_original_proxy() {
            eprintln!("native proxy restoration failed: {error:#}");
        }
    }
}

async fn status(owner: &OwnerCredentials) -> Result<crate::ServiceStatusSnapshot> {
    let response = get_status(owner).await?;
    ensure!(response.code == 0, "Status failed: {}", response.message);
    response.data.context("Status omitted payload")
}
async fn start(owner: &OwnerCredentials, sequence: u8) -> Result<OwnerSessionProof> {
    let token = format!("{sequence:02x}").repeat(32);
    let response = start_clash(
        owner,
        &StartClashRequest {
            proposed_session_token: token.clone(),
            macos_proxy: None,
            runtime: RuntimeBundle {
                yaml: "mode: rule\n".into(),
                assets: Vec::new(),
                remote_providers: Vec::new(),
                core_path: crate::service_paths()?
                    .core_dir()
                    .join("verge-mihomo")
                    .to_string_lossy()
                    .into_owned(),
            },
        },
    )
    .await?;
    ensure!(response.code == 0, "Start failed: {}", response.message);
    let generation = response.data.context("Start omitted result")?.session.generation;
    eprintln!(
        "native Start: generation={generation}, pid={:?}",
        status(owner).await?.core_pid
    );
    Ok(OwnerSessionProof { generation, token })
}
async fn apply(owner: &OwnerCredentials, session: &OwnerSessionProof) -> Result<()> {
    let response = set_system_proxy(
        owner,
        session,
        &MacosProxyConfig::Global {
            host: "127.0.0.1".into(),
            port: 45671,
            bypass: "localhost".into(),
        },
    )
    .await?;
    ensure!(
        response.code == 0 && response.data == Some(ProxyApplyOutcome::Applied),
        "native apply did not succeed: {}",
        response.message
    );
    wait_effective(Some(45671)).await?;
    ensure!(
        sysproxy::Sysproxy::snapshot()?.matches_global(&target(45671)),
        "native preferences do not match applied proxy"
    );
    Ok(())
}
fn target(port: u16) -> sysproxy::Sysproxy {
    sysproxy::Sysproxy {
        host: "127.0.0.1".into(),
        port,
        bypass: "localhost".into(),
        enable: true,
    }
}
async fn stop(owner: &OwnerCredentials, session: &OwnerSessionProof) -> Result<()> {
    let record = crate::core::runtime::read_core_runtime_record().await?;
    let response = stop_clash(owner, session).await?;
    ensure!(response.code == 0, "Stop failed: {}", response.message);
    ensure!(
        !std::path::Path::new(&crate::mihomo_ipc_path(&owner.identity)).exists(),
        "core socket survived Stop"
    );
    assert_core_reaped(record.as_ref()).await?;
    Ok(())
}
async fn assert_core_reaped(record: Option<&crate::core::runtime::CoreRuntimeRecord>) -> Result<()> {
    if let Some(record) = record {
        ensure!(
            crate::core::process::process_identity(record.pid)?.as_ref() != Some(&record.identity),
            "the recorded fixture core survived cleanup"
        );
        eprintln!("native cleanup: verified process {} exited", record.pid);
    }
    ensure!(
        crate::core::runtime::read_core_runtime_record().await?.is_none(),
        "runtime record survived cleanup"
    );
    Ok(())
}

async fn terminate_owned_core(owner: &OwnerCredentials) -> Result<u32> {
    let pid = status(owner).await?.core_pid.context("fixture core is not running")?;
    let record = crate::core::runtime::read_core_runtime_record()
        .await?
        .context("fixture runtime identity missing")?;
    ensure!(record.pid == pid, "fixture PID and runtime record differ");
    crate::core::process::terminate_process_if_identity(pid, &record.identity, Duration::from_secs(1)).await?;
    Ok(pid)
}
fn publish_fixture(name: &str) -> Result<()> {
    let source = std::env::current_dir()?.join("target/native-fixture/debug").join(name);
    let core = crate::service_paths()?.core_dir().join("verge-mihomo");
    let staging = core.with_extension("staging");
    // macOS copyfile can preserve the runner's ownership even under sudo.
    // Publish fresh bytes with explicit ownership on the actual staging fd.
    use std::os::{fd::AsRawFd as _, unix::fs::MetadataExt as _};
    let mut staged = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    std::io::copy(&mut std::fs::File::open(&source)?, &mut staged)?;
    ensure!(
        unsafe { platform_lib::fchown(staged.as_raw_fd(), 0, 0) } == 0,
        "fixture fchown failed: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        unsafe { platform_lib::fchmod(staged.as_raw_fd(), 0o755) } == 0,
        "fixture fchmod failed: {}",
        std::io::Error::last_os_error()
    );
    staged.sync_all()?;
    let metadata = staged.metadata()?;
    ensure!(
        metadata.uid() == 0 && metadata.mode() & 0o7777 == 0o755,
        "fixture staging ownership or mode is invalid"
    );
    std::fs::rename(staging, &core)?;
    eprintln!(
        "native approved core: path={}, sha256={}",
        core.display(),
        crate::management::sha256_file(&core)?
    );
    Ok(())
}

async fn native_scene(owner: &OwnerCredentials) -> Result<()> {
    let session = start(owner, 0x81).await?;
    apply(owner, &session).await?;
    stop(owner, &session).await?;
    wait_effective(None).await?;
    ensure!(
        sysproxy::Sysproxy::snapshot()?.is_all_disabled(),
        "independent proxy survived Stop"
    );

    let session = start(owner, 0x82).await?;
    apply(owner, &session).await?;
    let old_pid = terminate_owned_core(owner).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let current = status(owner).await?;
            if current.core_pid.is_some_and(|pid| pid != old_pid) && current.restart_count > 0 {
                ensure!(
                    current.active_generation == Some(session.generation),
                    "recovery changed owner generation"
                );
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    ensure!(
        sysproxy::Sysproxy::snapshot()?.matches_global(&target(45671)),
        "successful recovery lost its proxy"
    );
    wait_effective(Some(45671)).await?;
    publish_fixture("crash_binary")?;
    terminate_owned_core(owner).await?;
    tokio::time::timeout(Duration::from_secs(240), async {
        loop {
            let current = status(owner).await?;
            if current.service_state == crate::ServiceLifecycleState::Fatal
                && current.core_pid.is_none()
                && sysproxy::Sysproxy::snapshot()?.is_all_disabled()
                && effective_matches(None)?
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await??;
    ensure!(
        sysproxy::Sysproxy::snapshot()?.is_all_disabled(),
        "exhausted recovery left its proxy enabled"
    );
    stop(owner, &session).await?;

    publish_fixture("mock_binary")?;
    let session = start(owner, 0x83).await?;
    apply(owner, &session).await?;
    let foreign =
        sysproxy::apply_privileged_native_owned(&target(45672), &sysproxy::Autoproxy::default(), |receipt| {
            before_proxy_write(receipt).map_err(|error| sysproxy::Error::Io(std::io::Error::other(error.to_string())))
        })?;
    foreign.result?;
    wait_effective(Some(45672)).await?;
    stop(owner, &session).await?;
    ensure!(
        sysproxy::Sysproxy::snapshot()?.matches_global(&target(45672)),
        "Stop cleared a later foreign setting"
    );

    wait_effective(Some(45672)).await?;
    let session = start(owner, 0x84).await?;
    apply(owner, &session).await?;
    let record = crate::core::runtime::read_core_runtime_record()
        .await?
        .context("shutdown core record missing")?;
    crate::stop_ipc_server().await?;
    assert_core_reaped(Some(&record)).await?;
    wait_effective(None).await?;
    ensure!(
        sysproxy::Sysproxy::snapshot()?.is_all_disabled(),
        "shutdown left its owned proxy enabled"
    );
    ensure!(
        !crate::service_paths()?.ipc_path().exists(),
        "shutdown left its listener socket"
    );
    Ok(())
}

#[test]
#[ignore = "changes native proxy settings only on a disposable root macOS CI runner"]
fn native_proxy_lifecycle() -> Result<()> {
    let root = fixture_root()?;
    let paths = crate::service_paths()?;
    ensure!(
        !paths.persistent_state_dir().exists(),
        "native service state already exists; refusing to touch it"
    );
    std::fs::create_dir_all(root.join("owner"))?;
    crate::prepare_core_install_directory()?;
    let _restore = RestoreOriginal;
    ENABLED.store(true, Ordering::SeqCst);
    publish_fixture("mock_binary")?;
    eprintln!(
        "native backend: version={}, test_feature=false, executable_sha256={}",
        crate::VERSION,
        crate::management::sha256_file(&std::env::current_exe()?)?
    );
    let owner = OwnerCredentials {
        identity: OwnerIdentity::Unix {
            uid: 0,
            gid: unsafe { platform_lib::getegid() },
        },
        app_data_dir: root.join("owner").to_string_lossy().into_owned(),
        token: None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let result = runtime.block_on(async {
        let _owner_lock = crate::acquire_service_owner()
            .await?
            .context("native helper namespace is occupied")?;
        crate::reconcile_service_startup().await?;
        let server = crate::run_ipc_server().await?;
        crate::set_config(Some(crate::IpcConfig {
            default_timeout: Duration::from_secs(1),
            max_retries: 20,
            retry_delay: Duration::from_millis(500),
        }))
        .await;
        let scene = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if crate::get_version().await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await?;
            native_scene(&owner).await
        }
        .await;
        let cleanup = crate::stop_ipc_server().await;
        let joined = server.await;
        scene?;
        cleanup?;
        joined??;
        Ok::<_, anyhow::Error>(())
    });
    ENABLED.store(false, Ordering::SeqCst);
    restore_original_proxy()?;
    result?;
    std::fs::remove_dir_all(paths.persistent_state_dir())?;
    eprintln!("native proxy lifecycle: Stop, recovery, exhaustion, foreign edit, shutdown passed");
    Ok(())
}

#[test]
#[ignore = "restores the disposable CI runner's saved native proxy dictionaries"]
fn native_proxy_restore() -> Result<()> {
    restore_original_proxy()
}
