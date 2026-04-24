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
            .unwrap_or_else(|| PathBuf::from("/tmp/verge"))
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

    #[cfg(all(unix, not(feature = "test")))]
    {
        if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(path).join(SERVICE_NAME);
        }

        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join(SERVICE_NAME);
        }

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
