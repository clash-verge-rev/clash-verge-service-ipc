use serde::{Deserialize, Serialize};
#[cfg(feature = "client")]
use serde_json::Value;

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
        } else {
            "/tmp/verge/verge-mihomo.sock".to_string()
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
