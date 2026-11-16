use std::{
    path::Path,
    sync::{Arc, Weak},
    time::Duration,
};

#[cfg(windows)]
use anyhow::Result;
#[cfg(unix)]
use anyhow::{Result, anyhow};
use kode_bridge::{ClientConfig, IpcHttpClient};
use log::{debug, warn};
use once_cell::sync::Lazy;
use tokio::{sync::Mutex, task::JoinHandle};

#[cfg(all(windows, any(not(feature = "test"), test)))]
mod windows_identity;

use crate::{
    AuthenticatedRequest, AuthenticatedSessionRequest, IPC_AUTH_EXPECT, IPC_PATH, IpcCommand,
    MIN_REQUIRED_SERVICE_REVISION, MacosProxyConfig, OwnerCredentials, OwnerSessionProof, ProtocolInfo,
    ProtocolVersion, ProxyApplyOutcome, RuntimeBundle, RuntimeFileOutcome, RuntimeFileRequest, ServiceStatusSnapshot,
    StageRuntimeOutcome, StartClashRequest, StartClashResult, WriterConfig,
    core::structure::{JsonConvert, Response},
};

static CLIENT_STATE: Lazy<Mutex<ClientState>> = Lazy::new(|| Mutex::new(ClientState::default()));

#[derive(Default)]
struct ClientState {
    config: Option<IpcConfig>,
    cached: Option<CachedClient>,
}

struct CachedClient {
    runtime: tokio::runtime::Id,
    client: Weak<IpcHttpClient>,
    // The runtime owns the client lifetime; this slot owns the task and aborts it on replacement.
    keeper: JoinHandle<()>,
}

impl Drop for CachedClient {
    fn drop(&mut self) {
        self.keeper.abort();
    }
}

async fn shared_client() -> Result<Arc<IpcHttpClient>> {
    let runtime = tokio::runtime::Handle::current().id();
    let mut state = CLIENT_STATE.lock().await;
    if let Some(cached) = &state.cached
        && cached.runtime == runtime
        && !cached.keeper.is_finished()
        && let Some(client) = cached.client.upgrade()
    {
        return Ok(client);
    }
    state.cached = None;
    let client = Arc::new(connect_with_config(state.config.clone().unwrap_or_default()).await?);
    let retained = Arc::clone(&client);
    let keeper = tokio::spawn(async move {
        let _retained = retained;
        std::future::pending::<()>().await;
    });
    state.cached = Some(CachedClient {
        runtime,
        client: Arc::downgrade(&client),
        keeper,
    });
    Ok(client)
}

async fn send_cached(
    client: &Arc<IpcHttpClient>,
    request: kode_bridge::HttpRequestBuilder<'_>,
) -> Result<kode_bridge::HttpResponse> {
    match request.send().await {
        Ok(response) => Ok(response),
        Err(error) => {
            let mut state = CLIENT_STATE.lock().await;
            if state
                .cached
                .as_ref()
                .and_then(|entry| entry.client.upgrade())
                .is_some_and(|cached| Arc::ptr_eq(&cached, client))
            {
                state.cached = None;
            }
            Err(error.into())
        }
    }
}

static IPC_AUTH_HEADER_KEY: &str = "X-IPC-Magic";
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(30);
// Large cache chunks need a longer timeout than control messages.
const RUNTIME_FILE_TIMEOUT: Duration = Duration::from_secs(15);

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
    let client = shared_client().await?;
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
    let response = send_cached(&client, request.json_body(&body))
        .await?
        .json::<Response<R>>()?;
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
    let mut state = CLIENT_STATE.lock().await;
    state.cached = None;
    state.config = config;
}

pub async fn connect() -> Result<IpcHttpClient> {
    let config = CLIENT_STATE.lock().await.config.clone().unwrap_or_default();
    connect_with_config(config).await
}

async fn connect_with_config(c: IpcConfig) -> Result<IpcHttpClient> {
    debug!("Connecting to IPC at {}", IPC_PATH);

    #[cfg(unix)]
    {
        if let Err(err) = Path::metadata(IPC_PATH.as_ref()) {
            return Err(anyhow!("IPC path unavailable: {err}"));
        }
    }

    debug!("Using config: {:?}", c);
    let client = kode_bridge::IpcHttpClient::with_config(
        IPC_PATH,
        ClientConfig {
            default_timeout: c.default_timeout,
            max_retries: c.max_retries,
            retry_delay: c.retry_delay,
            enable_pooling: true,
            require_windows_server_system: false,
            #[cfg(all(windows, not(feature = "test")))]
            windows_server_pid_verifier: Some(windows_identity::verify_registered_service_process_id),
            #[cfg(all(windows, feature = "test"))]
            windows_server_pid_verifier: None,
            ..Default::default()
        },
    )?;

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
    let client = shared_client().await?;
    let request = client
        .get(IpcCommand::GetVersion.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT);
    let response = send_cached(&client, request).await?.json::<Response<ProtocolInfo>>()?;
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
    let client = shared_client().await?;
    let request = protected(client.post(IpcCommand::InspectInstallation.as_ref()))
        .timeout(LIFECYCLE_TIMEOUT)
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .header(
            "X-Service-Digest",
            if include_service_digest { "true" } else { "false" },
        )
        .json_body(&serde_json::to_value(requirements)?);
    let response = send_cached(&client, request)
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

#[cfg(all(test, feature = "standalone", feature = "test"))]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn pooled_client_is_scoped_to_runtime_and_configuration() -> Result<()> {
        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }
        async fn start_server() -> Result<JoinHandle<kode_bridge::Result<()>>> {
            let mut server = crate::run_ipc_server().await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if server.is_finished() {
                        let result = (&mut server).await;
                        anyhow::bail!("IPC listener exited before readiness: {result:?}");
                    }
                    if connect().await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            Ok(server)
        }
        let first_runtime = runtime();
        let weak = first_runtime.block_on(async {
            let server = start_server().await?;
            set_config(Some(IpcConfig {
                default_timeout: Duration::from_secs(1),
                max_retries: 20,
                retry_delay: Duration::from_millis(500),
            }))
            .await;
            let first = shared_client().await?;
            assert!(Arc::ptr_eq(&first, &shared_client().await?));
            get_version().await?;
            crate::stop_ipc_server().await?;
            server.await??;
            let restarted = start_server().await?;
            get_version().await?;
            assert!(Arc::ptr_eq(&first, &shared_client().await?));
            set_config(None).await;
            let configured = shared_client().await?;
            assert!(!Arc::ptr_eq(&first, &configured));
            crate::stop_ipc_server().await?;
            restarted.await??;
            Ok::<_, anyhow::Error>(Arc::downgrade(&configured))
        })?;
        drop(first_runtime);
        assert!(weak.upgrade().is_none(), "client outlived its runtime");
        runtime().block_on(async {
            let server = start_server().await?;
            get_version().await?;
            crate::stop_ipc_server().await?;
            server.await??;
            set_config(None).await;
            Ok::<_, anyhow::Error>(())
        })
    }
}
