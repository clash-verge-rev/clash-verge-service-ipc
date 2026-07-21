use crate::core::structure::{OwnerIdentity, owner_key};
use std::path::{Path, PathBuf};

const SERVICE_NAME: &str = "clash-verge-service";

#[derive(Debug, Clone)]
pub struct ServicePaths {
    runtime_dir: PathBuf,
    persistent_state_dir: PathBuf,
    ipc_path: PathBuf,
    owner_lock_path: PathBuf,
    pid_file_path: PathBuf,
    core_runtime_path: PathBuf,
    desired_state_path: PathBuf,
}

impl ServicePaths {
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn persistent_state_dir(&self) -> &Path {
        &self.persistent_state_dir
    }

    pub fn ipc_path(&self) -> &Path {
        &self.ipc_path
    }

    pub fn owner_lock_path(&self) -> &Path {
        &self.owner_lock_path
    }

    pub fn pid_file_path(&self) -> &Path {
        &self.pid_file_path
    }

    pub fn core_runtime_path(&self) -> &Path {
        &self.core_runtime_path
    }

    pub fn desired_state_path(&self) -> &Path {
        &self.desired_state_path
    }
}

pub fn service_paths() -> ServicePaths {
    let runtime_dir = runtime_dir();
    let persistent_state_dir = persistent_state_dir();
    ServicePaths {
        desired_state_path: persistent_state_dir.join("desired-state.json"),
        persistent_state_dir,
        ipc_path: PathBuf::from(crate::IPC_PATH),
        owner_lock_path: runtime_dir.join(format!("{SERVICE_NAME}.owner.lock")),
        pid_file_path: runtime_dir.join(format!("{SERVICE_NAME}.pid")),
        core_runtime_path: runtime_dir.join(format!("{SERVICE_NAME}.core.json")),
        runtime_dir,
    }
}

fn runtime_dir() -> PathBuf {
    #[cfg(unix)]
    {
        Path::new(crate::IPC_PATH)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/run/clash-verge-service"))
    }

    #[cfg(windows)]
    {
        std::env::temp_dir().join(SERVICE_NAME)
    }
}

fn persistent_state_dir() -> PathBuf {
    #[cfg(feature = "test")]
    {
        std::env::temp_dir().join("clash-verge-service-ipc-test-state")
    }

    // macOS：系统 daemon 以 root 运行,状态目录用系统级稳定位置,不依赖 launchd 下不可靠的
    // HOME/XDG —— 否则 desired-state 可能写一处读另一处而丢失(issue #7333)。
    #[cfg(all(target_os = "macos", not(feature = "test")))]
    {
        PathBuf::from("/Library/Application Support").join(SERVICE_NAME)
    }

    #[cfg(all(unix, not(target_os = "macos"), not(feature = "test")))]
    {
        PathBuf::from("/var/lib").join(SERVICE_NAME)
    }

    #[cfg(all(windows, not(feature = "test")))]
    {
        if let Some(path) = std::env::var_os("ProgramData") {
            return PathBuf::from(path).join(SERVICE_NAME);
        }

        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path).join(SERVICE_NAME);
        }

        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(SERVICE_NAME)
    }
}

pub(crate) fn unix_mihomo_ipc_path(runtime_root: &Path, uid: u32) -> PathBuf {
    runtime_root
        .join("users")
        .join(uid.to_string())
        .join("verge-mihomo.sock")
}

pub fn mihomo_ipc_path(identity: &OwnerIdentity) -> String {
    match identity {
        OwnerIdentity::Unix { uid, .. } => {
            #[cfg(feature = "test")]
            let runtime_root = std::env::temp_dir().join("clash-verge-service-ipc-test");
            #[cfg(all(target_os = "macos", not(feature = "test")))]
            let runtime_root = PathBuf::from("/var/run/clash-verge-service");
            #[cfg(all(unix, not(target_os = "macos"), not(feature = "test")))]
            let runtime_root = PathBuf::from("/run/clash-verge-service");

            unix_mihomo_ipc_path(&runtime_root, *uid)
                .to_string_lossy()
                .into_owned()
        }
        OwnerIdentity::Windows { .. } => {
            format!(r"\\.\pipe\verge-mihomo-{}", owner_key(identity))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::unix_mihomo_ipc_path;
    use std::path::Path;

    #[test]
    fn unix_mihomo_ipc_path_is_owner_scoped_and_below_sun_path_limit() {
        let path = unix_mihomo_ipc_path(Path::new("/var/run/clash-verge-service"), 501);

        assert_eq!(
            path,
            Path::new("/var/run/clash-verge-service/users/501/verge-mihomo.sock")
        );
        assert!(path.as_os_str().as_encoded_bytes().len() < 104);
    }
}
