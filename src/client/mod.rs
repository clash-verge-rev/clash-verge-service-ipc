use std::{path::Path, sync::Arc, time::Duration};

#[cfg(windows)]
use anyhow::Result;
#[cfg(unix)]
use anyhow::{Result, anyhow};
use compact_str::CompactString;
use kode_bridge::{ClientConfig, IpcHttpClient};
use log::{debug, warn};
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

#[cfg(all(windows, any(not(feature = "test"), test)))]
mod windows_identity;

use crate::{
    AuthenticatedRequest, AuthenticatedSessionRequest, IPC_AUTH_EXPECT, IPC_PATH, IpcCommand,
    MIN_REQUIRED_SERVICE_REVISION, MacosProxyConfig, OwnerCredentials, OwnerSessionProof,
    ProtocolInfo, ProtocolVersion, ProxyApplyOutcome, ServiceStatusSnapshot, StartClashRequest,
    StartClashResult, WriterConfig,
    core::structure::{JsonConvert, Response},
};

static CLIENT_CONFIG: Lazy<Arc<RwLock<Option<IpcConfig>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

static IPC_AUTH_HEADER_KEY: &str = "X-IPC-Magic";
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(30);

fn protected<'a>(
    request: kode_bridge::HttpRequestBuilder<'a>,
) -> kode_bridge::HttpRequestBuilder<'a> {
    request.header(
        crate::SERVICE_PROTOCOL_HEADER,
        ProtocolVersion::current().header_value(),
    )
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
    debug!("Connecting to IPC at {}", IPC_PATH);

    #[cfg(unix)]
    {
        if let Err(err) = Path::metadata(IPC_PATH.as_ref()) {
            return Err(anyhow!("IPC path unavailable: {err}"));
        }
    }

    let c = { CLIENT_CONFIG.read().await.clone() }.unwrap_or_default();
    debug!("Using config: {:?}", c);
    #[cfg(all(windows, not(feature = "test")))]
    let _verified_windows_server = windows_identity::verify_registered_service_pipe(
        IPC_PATH,
        crate::WINDOWS_SERVICE_NAME,
        IPC_AUTH_EXPECT,
    )?;
    let client = kode_bridge::IpcHttpClient::with_config(
        IPC_PATH,
        ClientConfig {
            default_timeout: c.default_timeout,
            max_retries: c.max_retries,
            retry_delay: c.retry_delay,
            enable_pooling: true,
            require_windows_server_system: false,
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
    let client = connect().await?;
    let payload = AuthenticatedRequest {
        credentials: credentials.clone(),
        payload: (),
    }
    .to_json_value()?;
    let response = protected(client.get(IpcCommand::Status.as_ref()))
        .json_body(&payload)
        .send()
        .await?
        .json::<Response<ServiceStatusSnapshot>>()?;
    Ok(response)
}

pub async fn is_reinstall_service_needed() -> bool {
    is_ipc_path_exists()
        && match get_version().await {
            Ok(resp) => resp.data.is_none_or(|info| {
                !info.supports_client(ProtocolVersion::current(), MIN_REQUIRED_SERVICE_REVISION)
            }),
            Err(_) => true,
        }
}

pub async fn start_clash(
    credentials: &OwnerCredentials,
    body: &StartClashRequest,
) -> Result<Response<StartClashResult>> {
    let client = connect().await?;
    let payload = AuthenticatedRequest {
        credentials: credentials.clone(),
        payload: body.clone(),
    }
    .to_json_value()?;
    let response = protected(client.post(IpcCommand::StartClash.as_ref()))
        .timeout(LIFECYCLE_TIMEOUT)
        .json_body(&payload)
        .send()
        .await?
        .json::<Response<StartClashResult>>()?;
    Ok(response)
}

pub async fn get_clash_logs(
    credentials: &OwnerCredentials,
) -> Result<Response<Vec<CompactString>>> {
    let client = connect().await?;
    let payload = AuthenticatedRequest {
        credentials: credentials.clone(),
        payload: (),
    }
    .to_json_value()?;
    let response = protected(client.get(IpcCommand::GetClashLogs.as_ref()))
        .json_body(&payload)
        .send()
        .await?
        .json::<Response<Vec<CompactString>>>()?;
    Ok(response)
}

pub async fn get_clash_log_snapshot(credentials: &OwnerCredentials) -> Result<Response<String>> {
    let client = connect().await?;
    let payload = AuthenticatedRequest {
        credentials: credentials.clone(),
        payload: (),
    }
    .to_json_value()?;
    let response = protected(client.get(IpcCommand::GetClashLogSnapshot.as_ref()))
        .json_body(&payload)
        .send()
        .await?
        .json::<Response<String>>()?;
    Ok(response)
}

pub async fn stop_clash(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
) -> Result<Response<()>> {
    let client = connect().await?;
    let payload = AuthenticatedSessionRequest {
        credentials: credentials.clone(),
        session: session.clone(),
        payload: (),
    }
    .to_json_value()?;
    let response = protected(client.delete(IpcCommand::StopClash.as_ref()))
        .timeout(LIFECYCLE_TIMEOUT)
        .json_body(&payload)
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}

pub async fn update_writer(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    body: &WriterConfig,
) -> Result<Response<()>> {
    let client = connect().await?;
    let payload = AuthenticatedSessionRequest {
        credentials: credentials.clone(),
        session: session.clone(),
        payload: body.clone(),
    }
    .to_json_value()?;
    let response = protected(client.put(IpcCommand::UpdateWriter.as_ref()))
        .json_body(&payload)
        .send()
        .await?
        .json::<Response<()>>()?;
    Ok(response)
}

pub async fn set_system_proxy(
    credentials: &OwnerCredentials,
    session: &OwnerSessionProof,
    body: &MacosProxyConfig,
) -> Result<Response<ProxyApplyOutcome>> {
    let client = connect().await?;
    let payload = AuthenticatedSessionRequest {
        credentials: credentials.clone(),
        session: session.clone(),
        payload: body.clone(),
    }
    .to_json_value()?;
    let response = protected(client.put(IpcCommand::SetSystemProxy.as_ref()))
        .json_body(&payload)
        .send()
        .await?
        .json::<Response<ProxyApplyOutcome>>()?;
    Ok(response)
}
