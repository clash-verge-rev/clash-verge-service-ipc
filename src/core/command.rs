use serde::{Deserialize, Serialize};
use strum_macros::{AsRefStr, EnumString};

#[derive(Debug, Clone, Serialize, Deserialize, EnumString, AsRefStr)]
pub enum IpcCommand {
    #[strum(serialize = "/version")]
    GetVersion,
    // #[strum(serialize = "/clash")]
    // GetClash,

    // 用于日志界面加载上一次日志内容
    #[strum(serialize = "/clash/logs")]
    GetClashLogs,

    #[strum(serialize = "/clash/start")]
    StartClash,
    #[strum(serialize = "/clash/stop")]
    StopClash,
    #[strum(serialize = "/writer")]
    UpdateWriter,
    #[strum(serialize = "/magic")]
    Magic,
}
