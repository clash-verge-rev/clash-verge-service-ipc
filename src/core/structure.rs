use serde::{Deserialize, Serialize};
#[cfg(feature = "client")]
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub const OWNER_TOKEN_FILE_NAME: &str = ".clash-verge-service-owner-token";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnerIdentity {
    Unix { uid: u32, gid: u32 },
    Windows { sid: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerCredentials {
    pub identity: OwnerIdentity,
    pub app_data_dir: String,
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticatedRequest<T> {
    pub credentials: OwnerCredentials,
    pub payload: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAsset {
    pub source: String,
    pub destination: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeBundle {
    pub yaml: String,
    pub assets: Vec<RuntimeAsset>,
    pub core_path: String,
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceErrorCode {
    UnauthorizedOwner = 1001,
    NotActive = 1002,
    InvalidInstallLocation = 1003,
    InvalidRuntimeAsset = 1004,
    LegacyCleanupFailed = 1005,
    OwnerSwitchFailed = 1006,
}

pub fn owner_key(identity: &OwnerIdentity) -> String {
    match identity {
        OwnerIdentity::Unix { uid, .. } => uid.to_string(),
        OwnerIdentity::Windows { sid } => Sha256::digest(sid.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClashConfig {
    pub core_config: CoreConfig,
    pub log_config: WriterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub core_path: String,
    pub core_ipc_path: String,
    pub config_path: String,
    pub config_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriterConfig {
    pub directory: String,
    pub max_log_size: u64,
    pub max_log_files: usize,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceLifecycleState {
    Starting = 0,
    Running = 1,
    RecoveringCore = 2,
    RecoveringIpc = 3,
    Fatal = 4,
}

impl ServiceLifecycleState {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Running,
            2 => Self::RecoveringCore,
            3 => Self::RecoveringIpc,
            4 => Self::Fatal,
            _ => Self::Starting,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatusSnapshot {
    pub is_active: bool,
    pub service_state: ServiceLifecycleState,
    pub core_pid: Option<u32>,
    pub core_started_at: Option<u64>,
    pub last_core_exit_reason: Option<String>,
    pub restart_count: u32,
    pub last_recovery_at: Option<u64>,
    pub desired_core_should_be_running: bool,
    pub desired_generation: u64,
    pub desired_updated_at: u64,
}

#[cfg(feature = "response")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response<T> {
    pub code: u16,
    pub message: String,
    pub data: Option<T>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        let core_ipc_path = if cfg!(windows) {
            r"\\.\pipe\verge-mihomo".to_string()
        } else if cfg!(feature = "test") {
            "/tmp/clash-verge-service-ipc-test/mihomo.sock".to_string()
        } else if cfg!(target_os = "macos") {
            "/var/run/clash-verge-service/users/0/verge-mihomo.sock".to_string()
        } else {
            "/run/clash-verge-service/users/0/verge-mihomo.sock".to_string()
        };
        Self {
            core_path: "./clash".to_string(),
            core_ipc_path,
            config_path: "./config.yaml".to_string(),
            config_dir: "./configs".to_string(),
        }
    }
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            directory: "./logs".to_string(),
            max_log_size: 10 * 1024 * 1024, // 10 MB
            max_log_files: 8,
        }
    }
}

#[cfg(feature = "client")]
pub trait JsonConvert: Serialize + for<'de> Deserialize<'de> {
    /// 转换为 JSON Value
    fn to_json_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    // /// 从 JSON Value 转换
    // fn from_json_value(value: Value) -> Result<Self, serde_json::Error> {
    //     serde_json::from_value(value)
    // }

    // /// 序列化为 JSON 字符串
    // fn to_json_string(&self) -> Result<String, serde_json::Error> {
    //     serde_json::to_string(self)
    // }

    // /// 从 JSON 字符串转换
    // fn from_json_string(json: &str) -> Result<Self, serde_json::Error> {
    //     serde_json::from_str(json)
    // }
}
#[cfg(feature = "client")]
impl<T> JsonConvert for T where T: Serialize + for<'de> Deserialize<'de> {}

#[cfg(test)]
mod tests {
    use super::{OwnerIdentity, owner_key};

    #[test]
    fn unix_owner_key_is_decimal_uid() {
        let identity = OwnerIdentity::Unix { uid: 501, gid: 20 };

        assert_eq!(owner_key(&identity), "501");
    }

    #[test]
    fn windows_owner_key_is_stable_and_does_not_embed_sid() {
        let identity = OwnerIdentity::Windows {
            sid: "S-1-5-21-1-2-3-1001".to_string(),
        };

        let first = owner_key(&identity);
        let second = owner_key(&identity);

        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert!(!first.contains("S-1-5"));
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
}
