use std::{path::Path, sync::Arc, time::Duration};

use anyhow::Result;
use compact_str::CompactString;
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

use crate::{
    ClashConfig, IPC_AUTH_EXPECT, IPC_PATH, IpcCommand, WriterConfig,
    core::structure::{JsonConvert, Response},
};

static DEFAULT_IPC_CONFIG: Lazy<IpcConfig> = Lazy::new(IpcConfig::default);
static CLIENT_CONFIG: Lazy<Arc<RwLock<Option<IpcConfig>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

static IPC_AUTH_HEADER_KEY: &str = "X-IPC-Magic";

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

pub fn is_ipc_path_exists() -> bool {
    Path::new(IPC_PATH).exists()
}

pub async fn connect() -> Result<reqwest::Client> {
    build_client().await.map_err(Into::into)
}

pub async fn get_version() -> Result<Response<String>> {
    let response = build_client()
        .await?
        .get(IpcCommand::GetVersion.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<String>>()
        .await?;
    Ok(response)
}

pub async fn is_reinstall_service_needed() -> bool {
    is_ipc_path_exists()
        && match get_version().await {
            Ok(resp) => {
                if let Some(ver) = resp.data {
                    ver != crate::VERSION
                } else {
                    true
                }
            }
            Err(_) => true,
        }
}

pub async fn start_clash(body: &ClashConfig) -> Result<Response<()>> {
    let client = build_client().await?;
    let payload = body.to_json_value()?;
    let response = client
        .post(IpcCommand::StartClash.as_ref())
        .json(&payload)
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<()>>()
        .await?;
    Ok(response)
}

pub async fn get_clash_logs() -> Result<Response<Vec<CompactString>>> {
    let client = build_client().await?;
    let response = client
        .get(IpcCommand::GetClashLogs.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<Vec<CompactString>>>()
        .await?;
    Ok(response)
}

pub async fn stop_clash() -> Result<Response<()>> {
    let client = build_client().await?;
    let response = client
        .delete(IpcCommand::StopClash.as_ref())
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<()>>()
        .await?;
    Ok(response)
}

pub async fn update_writer(body: &WriterConfig) -> Result<Response<()>> {
    let client = build_client().await?;
    let payload = body.to_json_value()?;
    let response = client
        .put(IpcCommand::UpdateWriter.as_ref())
        .json(&payload)
        .header(IPC_AUTH_HEADER_KEY, IPC_AUTH_EXPECT)
        .send()
        .await?
        .json::<Response<()>>()
        .await?;
    Ok(response)
}

async fn build_client() -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();

    #[cfg(unix)]
    {
        builder = builder.unix_socket(IPC_PATH);
    }

    #[cfg(windows)]
    {
        builder = builder.windows_named_pipe(IPC_PATH);
    }

    let config_guard = CLIENT_CONFIG.read().await;
    let config = config_guard.as_ref().unwrap_or(&DEFAULT_IPC_CONFIG);

    let retry_policy = reqwest::retry::for_host("localhost")
        .max_retries_per_request(config.max_retries as u32)
        .no_budget()
        .classify_fn(|req_rep| {
            if req_rep.error().is_some() {
                return req_rep.retryable();
            }

            match req_rep.status() {
                Some(s) if s.is_server_error() => req_rep.retryable(),
                Some(http::StatusCode::REQUEST_TIMEOUT) => req_rep.retryable(),
                Some(http::StatusCode::TOO_MANY_REQUESTS) => req_rep.retryable(),
                Some(http::StatusCode::SERVICE_UNAVAILABLE) => req_rep.retryable(),
                _ => req_rep.success(),
            }
        });

    builder
        .timeout(config.default_timeout)
        .retry(retry_policy)
        .build()
}
