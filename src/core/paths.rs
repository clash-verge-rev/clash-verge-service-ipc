use crate::core::structure::{OwnerIdentity, owner_key};
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone)]
pub struct OwnerPaths {
    root: PathBuf,
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

    pub fn install_dir(&self) -> PathBuf {
        self.persistent_state_dir.join("bin")
    }

    pub fn active_owner_path(&self) -> PathBuf {
        self.persistent_state_dir.join("active-owner.json")
    }

    pub fn owner_generation_path(&self) -> PathBuf {
        self.persistent_state_dir.join("owner-generation.json")
    }

    pub fn for_owner(&self, identity: &OwnerIdentity) -> OwnerPaths {
        self.for_owner_key(&owner_key(identity))
    }

    pub fn for_owner_key(&self, owner_key: &str) -> OwnerPaths {
        OwnerPaths {
            root: self.persistent_state_dir.join("users").join(owner_key),
        }
    }
}

impl OwnerPaths {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn desired_state_path(&self) -> PathBuf {
        self.root.join("desired-state.json")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
}

pub fn service_paths() -> ServicePaths {
    let runtime_dir = runtime_dir();
    let persistent_state_dir = persistent_state_dir();
    ServicePaths {
        desired_state_path: persistent_state_dir.join("desired-state.json"),
        persistent_state_dir,
        ipc_path: PathBuf::from(crate::IPC_PATH),
        owner_lock_path: runtime_dir.join(format!("{}.owner.lock", crate::SERVICE_SLUG)),
        pid_file_path: runtime_dir.join(format!("{}.pid", crate::SERVICE_SLUG)),
        core_runtime_path: runtime_dir.join(format!("{}.core.json", crate::SERVICE_SLUG)),
        runtime_dir,
    }
}

#[cfg(feature = "standalone")]
pub(crate) fn ensure_persistent_state_layout() -> anyhow::Result<()> {
    let paths = service_paths();
    let root = paths.persistent_state_dir();
    #[cfg(unix)]
    crate::core::unix_security::ensure_private_service_directory(root)?;
    #[cfg(windows)]
    crate::core::windows_security::ensure_private_service_directory(root)?;

    let users = root.join("users");
    let install = paths.install_dir();
    #[cfg(unix)]
    crate::core::unix_security::ensure_private_service_directory(&users)?;
    #[cfg(windows)]
    crate::core::windows_security::ensure_private_service_directory(&users)?;
    #[cfg(unix)]
    crate::core::unix_security::ensure_private_service_directory(&install)?;
    #[cfg(windows)]
    crate::core::windows_security::ensure_private_service_directory(&install)?;
    #[cfg(unix)]
    crate::core::unix_security::secure_service_file_if_exists(&paths.active_owner_path())?;
    #[cfg(windows)]
    crate::core::windows_security::secure_private_service_file_if_exists(
        &paths.active_owner_path(),
    )?;
    #[cfg(unix)]
    crate::core::unix_security::secure_service_file_if_exists(&paths.owner_generation_path())?;
    #[cfg(windows)]
    crate::core::windows_security::secure_private_service_file_if_exists(
        &paths.owner_generation_path(),
    )?;
    Ok(())
}

#[cfg(feature = "standalone")]
pub fn prepare_service_install_directory() -> anyhow::Result<PathBuf> {
    let paths = service_paths();
    let root = paths.persistent_state_dir();
    let install = paths.install_dir();
    #[cfg(unix)]
    {
        crate::core::unix_security::ensure_private_service_directory(root)?;
        crate::core::unix_security::ensure_private_service_directory(&install)?;
    }
    #[cfg(windows)]
    {
        crate::core::windows_security::ensure_private_installer_directory(root)?;
        crate::core::windows_security::ensure_private_installer_directory(&install)?;
    }
    Ok(install)
}

#[cfg(feature = "standalone")]
pub(crate) fn ensure_owner_state_directory(identity: &OwnerIdentity) -> anyhow::Result<OwnerPaths> {
    ensure_persistent_state_layout()?;
    let owner = service_paths().for_owner(identity);
    #[cfg(unix)]
    crate::core::unix_security::ensure_private_service_directory(owner.root())?;
    #[cfg(windows)]
    crate::core::windows_security::ensure_private_service_directory(owner.root())?;
    Ok(owner)
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
        persistent_state_dir().join("runtime")
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
        PathBuf::from("/Library/Application Support").join(crate::SERVICE_SLUG)
    }

    #[cfg(all(unix, not(target_os = "macos"), not(feature = "test")))]
    {
        PathBuf::from("/var/lib").join(crate::SERVICE_SLUG)
    }

    #[cfg(all(windows, not(feature = "test")))]
    {
        windows_program_data()
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join(crate::SERVICE_SLUG)
    }
}

#[cfg(all(windows, not(feature = "test")))]
fn windows_program_data() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};

    let mut raw = std::ptr::null_mut();
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, std::ptr::null_mut(), &mut raw) };
    if status < 0 || raw.is_null() {
        return None;
    }
    let length = unsafe {
        let mut length = 0;
        while *raw.add(length) != 0 {
            length += 1;
        }
        length
    };
    let value = std::ffi::OsString::from_wide(unsafe { std::slice::from_raw_parts(raw, length) });
    unsafe { CoTaskMemFree(raw.cast()) };
    Some(PathBuf::from(value))
}

#[cfg(unix)]
pub(crate) fn unix_mihomo_ipc_path(runtime_root: &Path, uid: u32) -> PathBuf {
    runtime_root
        .join("users")
        .join(uid.to_string())
        .join("verge-mihomo.sock")
}

pub fn mihomo_ipc_path(identity: &OwnerIdentity) -> String {
    match identity {
        OwnerIdentity::Unix { uid: _uid, .. } => {
            #[cfg(windows)]
            {
                format!(
                    r"\\.\pipe\verge-mihomo-{}-{}",
                    crate::CHANNEL_IDENTITY.id,
                    owner_key(identity)
                )
            }

            #[cfg(unix)]
            {
                #[cfg(feature = "test")]
                let runtime_root = PathBuf::from("/tmp/clash-verge-service-ipc-test");
                #[cfg(not(feature = "test"))]
                let runtime_root = service_paths().runtime_dir().to_path_buf();

                unix_mihomo_ipc_path(&runtime_root, *_uid)
                    .to_string_lossy()
                    .into_owned()
            }
        }
        OwnerIdentity::Windows { .. } => {
            format!(
                r"\\.\pipe\verge-mihomo-{}-{}",
                crate::CHANNEL_IDENTITY.id,
                owner_key(identity)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::service_paths;
    #[cfg(unix)]
    use super::unix_mihomo_ipc_path;
    use crate::OwnerIdentity;
    #[cfg(unix)]
    use std::path::Path;

    #[cfg(unix)]
    #[test]
    fn unix_mihomo_ipc_path_is_owner_scoped_and_below_sun_path_limit() {
        let path = unix_mihomo_ipc_path(Path::new("/var/run/clash-verge-service"), 501);

        assert_eq!(
            path,
            Path::new("/var/run/clash-verge-service/users/501/verge-mihomo.sock")
        );
        assert!(path.as_os_str().as_encoded_bytes().len() < 104);
    }

    #[test]
    fn owner_paths_isolate_state_runtime_and_logs() {
        let paths = service_paths();
        let owner = paths.for_owner(&OwnerIdentity::Unix { uid: 501, gid: 20 });

        assert!(owner.root().ends_with("users/501"));
        assert_eq!(
            owner.desired_state_path(),
            owner.root().join("desired-state.json")
        );
        assert_eq!(owner.runtime_dir(), owner.root().join("runtime"));
        assert_eq!(owner.logs_dir(), owner.root().join("logs"));
        assert_eq!(
            paths.active_owner_path(),
            paths.persistent_state_dir().join("active-owner.json")
        );
    }
}
