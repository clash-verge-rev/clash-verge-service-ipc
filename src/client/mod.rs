use anyhow::Context as _;
use std::{path::Path, sync::Arc, time::Duration};

#[cfg(windows)]
use anyhow::Result;
#[cfg(unix)]
use anyhow::{Result, anyhow};
use kode_bridge::{ClientConfig, IpcHttpClient};
use log::{debug, warn};
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

#[cfg(all(windows, any(not(feature = "test"), test)))]
mod windows_identity;

use crate::{
    AuthenticatedRequest, AuthenticatedSessionRequest, IPC_AUTH_EXPECT, IPC_PATH, IpcCommand,
    MIN_REQUIRED_SERVICE_REVISION, MacosProxyConfig, OwnerCredentials, OwnerSessionProof, ProtocolInfo,
    ProtocolVersion, ProxyApplyOutcome, RuntimeBundle, RuntimeFileOutcome, RuntimeFileRequest, ServiceStatusSnapshot,
    StageRuntimeOutcome, StartClashRequest, StartClashResult, StopOwnedClashRequest, WriterConfig,
    core::structure::{JsonConvert, Response},
};

static CLIENT_CONFIG: Lazy<Arc<RwLock<Option<IpcConfig>>>> = Lazy::new(|| Arc::new(RwLock::new(None)));

static IPC_AUTH_HEADER_KEY: &str = "X-IPC-Magic";
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(30);
// Large cache chunks need a longer timeout than control messages.
const RUNTIME_FILE_TIMEOUT: Duration = Duration::from_secs(15);

/// Confirms that the StartClash POST was never attempted.
#[derive(Debug)]
pub struct StartClashNotSent;

impl std::fmt::Display for StartClashNotSent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the service start request was not sent")
    }
}

impl std::error::Error for StartClashNotSent {}

fn protected<'a>(request: kode_bridge::HttpRequestBuilder<'a>) -> kode_bridge::HttpRequestBuilder<'a> {
    request.header(
        crate::SERVICE_PROTOCOL_HEADER,
        ProtocolVersion::current().header_value(),
    )
}

#[derive(Clone, Copy)]
enum Verb {
    Get,
    Post,
    Put,
    Delete,
}

/// Sends a versioned, authenticated request using the route's required envelope.
/// `session` selects owner-only or active-session authentication.
async fn protected_call<P, R>(
    verb: Verb,
    command: IpcCommand,
    credentials: &OwnerCredentials,
    session: Option<&OwnerSessionProof>,
    payload: P,
    timeout: Option<Duration>,
) -> Result<Response<R>>
where
    P: serde::Serialize + for<'de> serde::Deserialize<'de>,
    R: for<'de> serde::Deserialize<'de>,
{
    let (client, body) = async {
        let client = connect_for_command(Some(&command)).await?;
        let body = match session {
            None => AuthenticatedRequest {
                credentials: credentials.clone(),
                payload,
            }
            .to_json_value()?,
            Some(session) => AuthenticatedSessionRequest {
                credentials: credentials.clone(),
                session: session.clone(),
                payload,
            }
            .to_json_value()?,
        };
        Ok::<_, anyhow::Error>((client, body))
    }
    .await
    .map_err(|error| {
        if matches!(command, IpcCommand::StartClash) {
            error.context(StartClashNotSent)
        } else {
            error
        }
    })?;
    let path = command.as_ref();
    let request = protected(match verb {
        Verb::Get => client.get(path),
        Verb::Post => client.post(path),
        Verb::Put => client.put(path),
        Verb::Delete => client.delete(path),
    });
    let request = match timeout {
        Some(timeout) => request.timeout(timeout),
        None => request,
    };
    let response = request.json_body(&body).send().await?.json::<Response<R>>()?;
    Ok(response)
}

#[derive(Debug, Clone)]
pub struct IpcConfig {
    pub default_timeout: Duration,
    pub max_retries: usize,
    pub retry_delay: Duration,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_millis(50),
            max_retries: 8,
            retry_delay: Duration::from_millis(150),
        }
    }
}

pub async fn set_config(config: Option<IpcConfig>) {
    let mut guard = CLIENT_CONFIG.write().await;
    *guard = config;
}

pub async fn connect() -> Result<IpcHttpClient> {
    connect_for_command(None).await
}

fn client_for_command(path: &Path, c: IpcConfig, command: Option<&IpcCommand>) -> Result<IpcHttpClient> {
    Ok(kode_bridge::IpcHttpClient::with_config(
        path,
        ClientConfig {
            default_timeout: c.default_timeout,
            // Only the lifecycle owner can preserve uncertainty across repeated Start requests.
            max_retries: if matches!(command, Some(IpcCommand::StartClash)) {
                1
            } else {
                c.max_retries
            },
            retry_delay: c.retry_delay,
            enable_pooling: true,
            require_windows_server_system: false,
            #[cfg(all(windows, not(feature = "test")))]
            windows_server_pid_verifier: Some(windows_identity::verify_registered_service_process_id),
            #[cfg(all(windows, feature = "test"))]
            windows_server_pid_verifier: None,
            ..Default::default()
        },
    )?)
}

async fn connect_for_command(command: Option<&IpcCommand>) -> Result<IpcHttpClient> {
    debug!("Connecting to IPC at {}", IPC_PATH);

    #[cfg(unix)]
    {
        if let Err(err) = Path::metadata(IPC_PATH.as_ref()) {
            return Err(anyhow!("IPC path unavailable: {err}"));
        }
    }

    let c = { CLIENT_CONFIG.read().await.clone() }.unwrap_or_default();
    debug!("Using config: {:?}", c);
    let client = client_for_command(Path::new(IPC_PATH), c, command)?;

    if let Err(e) = client
        .get(IpcCommand::Magic.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await
    {
        warn!("Failed to connect to IPC server: {}", e);
        return Err(anyhow::anyhow!("Failed to connect to IPC server: {}", e));
    }

    Ok(client)
}

pub fn is_ipc_path_exists() -> bool {
    Path::new(IPC_PATH).exists()
}

pub async fn get_version() -> Result<Response<ProtocolInfo>> {
    let client = connect().await?;
    let response = client
        .get(IpcCommand::GetVersion.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<ProtocolInfo>>()?;
    Ok(response)
}

pub async fn get_status(credentials: &OwnerCredentials) -> Result<Response<ServiceStatusSnapshot>> {
    protected_call(Verb::Get, IpcCommand::Status, credentials, None, (), None).await
}

pub async fn is_reinstall_service_needed() -> bool {
    is_ipc_path_exists()
        && match get_version().await {
            Ok(resp) => resp
                .data
                .is_none_or(|info| !info.supports_client(ProtocolVersion::current(), MIN_REQUIRED_SERVICE_REVISION)),
            Err(_) => true,
        }
}

pub async fn start_clash(
    credentials: &OwnerCredentials,
    body: &StartClashRequest,
) -> Result<Response<StartClashResult>> {
    if body.tun_guard.is_some() {
        async {
            let protocol = get_version()
                .await?
                .data
                .context("service omitted its protocol capabilities")?;
            anyhow::ensure!(
                protocol.supports_service_tun_guard(),
                "service does not support Windows TUN forwarding protection; install the matching service"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await
        .context(StartClashNotSent)?;
    }
    protected_call(
        Verb::Post,
        IpcCommand::StartClash,
        credentials,
        None,
        body.clone(),
        Some(LIFECYCLE_TIMEOUT),
    )
    .await
}

pub async fn get_clash_logs(credentials: &OwnerCredentials) -> Result<Response<Vec<String>>> {
    protected_call(Verb::Get, IpcCommand::GetClashLogs, credentials, None, (), None).await
}

pub async fn get_clash_log_snapshot(credentials: &OwnerCredentials) -> Result<Response<String>> {
    protected_call(Verb::Get, IpcCommand::GetClashLogSnapshot, credentials, None, (), None).await
}

pub async fn stop_clash(credentials: &OwnerCredentials, session: &OwnerSessionProof) -> Result<Response<()>> {
    protected_call(
        Verb::Delete,
        IpcCommand::StopClash,
        credentials,
        Some(session),
        (),
        Some(LIFECYCLE_TIMEOUT),
    )
    .await
}

/// Stops the authenticated owner's observed generation without a retained session token.
/// Requires [`ProtocolInfo::supports_owned_core_stop`]; a replacement generation is refused.
pub async fn stop_owned_clash(credentials: &OwnerCredentials, request: &StopOwnedClashRequest) -> Result<Response<()>> {
    protected_call(
        Verb::Delete,
        IpcCommand::StopOwnedClash,
        credentials,
        None,
        request.clone(),
        Some(LIFECYCLE_TIMEOUT),
    )
    .await
}

/// Stages `body` into the running core's generation without restarting it.
/// Call only when [`ProtocolInfo::supports_runtime_staging`] is true; `RestartRequired` is a
/// successful response that asks the caller to fall back to stop and start.
pub async fn stage_runtime(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    body: &RuntimeBundle,
) -> Result<Response<StageRuntimeOutcome>> {
    protected_call(
        Verb::Put,
        IpcCommand::StageRuntime,
        credentials,
        Some(session),
        body.clone(),
        Some(LIFECYCLE_TIMEOUT),
    )
    .await
}

/// Requires [`ProtocolInfo::supports_runtime_file_read`]; advance `offset` until `len` is reached.
pub async fn read_runtime_file(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    body: &RuntimeFileRequest,
) -> Result<Response<RuntimeFileOutcome>> {
    protected_call(
        Verb::Get,
        IpcCommand::ReadRuntimeFile,
        credentials,
        Some(session),
        body.clone(),
        Some(RUNTIME_FILE_TIMEOUT),
    )
    .await
}

pub async fn update_writer(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    body: &WriterConfig,
) -> Result<Response<()>> {
    protected_call(
        Verb::Put,
        IpcCommand::UpdateWriter,
        credentials,
        Some(session),
        body.clone(),
        None,
    )
    .await
}

pub async fn set_system_proxy(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    body: &MacosProxyConfig,
) -> Result<Response<ProxyApplyOutcome>> {
    protected_call(
        Verb::Put,
        IpcCommand::SetSystemProxy,
        credentials,
        Some(session),
        body.clone(),
        None,
    )
    .await
}

/// Inspects approved cores and global occupancy without exposing another owner's session.
pub async fn inspect_installation(requirements: &[crate::CoreRequirement]) -> Result<crate::InstallationStatus> {
    inspect_installation_with_digest(requirements, false).await
}

pub(crate) async fn inspect_installation_with_digest(
    requirements: &[crate::CoreRequirement],
    include_service_digest: bool,
) -> Result<crate::InstallationStatus> {
    let client = connect().await?;
    let response = protected(client.post(IpcCommand::InspectInstallation.as_ref()))
        .timeout(LIFECYCLE_TIMEOUT)
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .header(
            "X-Service-Digest",
            if include_service_digest { "true" } else { "false" },
        )
        .json_body(&serde_json::to_value(requirements)?)
        .send()
        .await?
        .json::<Response<crate::InstallationStatus>>()?;
    anyhow::ensure!(
        response.code == 0,
        "installation inspection rejected: {} ({})",
        response.message,
        response.code
    );
    response
        .data
        .ok_or_else(|| anyhow::anyhow!("service omitted installation status"))
}

#[cfg(all(test, feature = "test"))]
mod tests {
    use super::{IpcConfig, client_for_command};
    use crate::IpcCommand;
    use kode_bridge::ipc_http_server::{HttpResponse, IpcHttpServer, Router};
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn start_timeout_is_not_hidden_by_a_wire_retry() -> anyhow::Result<()> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\verge-start-retry-test-{}-{nonce}", std::process::id());
        #[cfg(unix)]
        let endpoint = std::env::temp_dir().join(format!("verge-start-retry-test-{}-{nonce}.sock", std::process::id()));
        let start_count = Arc::new(AtomicUsize::new(0));
        let status_count = Arc::new(AtomicUsize::new(0));
        let starts = Arc::clone(&start_count);
        let statuses = Arc::clone(&status_count);
        let router = Router::new()
            .get("/ready", |_| async { HttpResponse::json(&serde_json::json!({})) })
            .post(IpcCommand::StartClash.as_ref(), move |_| {
                let first = starts.fetch_add(1, Ordering::Relaxed) == 0;
                async move {
                    if first {
                        std::future::pending::<()>().await;
                    }
                    HttpResponse::json(&serde_json::json!({"code": 1, "message": "late refusal"}))
                }
            })
            .get(IpcCommand::Status.as_ref(), move |_| {
                let first = statuses.fetch_add(1, Ordering::Relaxed) == 0;
                async move {
                    if first {
                        std::future::pending::<()>().await;
                    }
                    HttpResponse::json(&serde_json::json!({"code": 0}))
                }
            });
        let mut server = IpcHttpServer::new(&endpoint)?.router(router);
        let server = tokio::spawn(async move { server.serve().await });
        let outcome = async {
            let config = IpcConfig {
                default_timeout: Duration::from_millis(100),
                max_retries: 2,
                retry_delay: Duration::ZERO,
            };
            let start = client_for_command(Path::new(&endpoint), config.clone(), Some(&IpcCommand::StartClash))?;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if start.get("/ready").send().await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;

            assert!(start.post(IpcCommand::StartClash.as_ref()).send().await.is_err());
            assert_eq!(start_count.load(Ordering::Relaxed), 1);
            let status = client_for_command(Path::new(&endpoint), config, Some(&IpcCommand::Status))?;
            assert!(status.get(IpcCommand::Status.as_ref()).send().await.is_ok());
            assert_eq!(status_count.load(Ordering::Relaxed), 2);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        server.abort();
        let _ = server.await;
        #[cfg(unix)]
        let _ = std::fs::remove_file(&endpoint);
        outcome
    }
}
