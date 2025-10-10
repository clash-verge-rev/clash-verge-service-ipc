use serde::{Deserialize, Serialize};
use strum_macros::{AsRefStr, EnumString};

#[derive(Debug, Clone, Serialize, Deserialize, EnumString, AsRefStr)]
pub enum IpcCommand {
    #[strum(serialize = "/version")]
    GetVersion,
    // #[strum(serialize = "/clash")]
    // GetClash,
    #[strum(serialize = "/clash/start")]
    StartClash,
    #[strum(serialize = "/clash/stop")]
    StopClash,
    #[strum(serialize = "/magic")]
    Magic,
}
