use serde::{Deserialize, Serialize};
#[cfg(feature = "client")]
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClashConfig {
    pub core_config: CoreConfig,
    pub log_config: WriterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub core_path: String,
    pub config_path: String,
    pub config_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriterConfig {
    pub directory: String,
    pub max_log_size: u64,
    pub max_log_files: usize,
}

#[cfg(feature = "response")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response<T> {
    pub code: u16,
    pub message: String,
    pub data: Option<T>,
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
