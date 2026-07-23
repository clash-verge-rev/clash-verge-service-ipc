#[cfg(not(feature = "test"))]
use anyhow::{Context as _, Result, bail};
use std::ffi::OsStr;
#[cfg(not(feature = "test"))]
use std::os::windows::ffi::OsStrExt as _;
#[cfg(not(feature = "test"))]
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
#[cfg(not(feature = "test"))]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(not(feature = "test"))]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
#[cfg(not(feature = "test"))]
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;

#[cfg(not(feature = "test"))]
use platform_lib::service::ServiceAccess;
#[cfg(not(feature = "test"))]
use platform_lib::service_manager::{ServiceManager, ServiceManagerAccess};

#[cfg(not(feature = "test"))]
const FILE_READ_DATA: u32 = 0x0001;
#[cfg(not(feature = "test"))]
const FILE_WRITE_DATA: u32 = 0x0002;

fn is_local_system_account(account: &OsStr) -> bool {
    matches!(
        account.to_string_lossy().to_ascii_lowercase().as_str(),
        "localsystem" | "nt authority\\system"
    )
}

fn trusted_service_identity(
    pipe_process_id: u32,
    service_process_id: Option<u32>,
    account: Option<&OsStr>,
) -> bool {
    pipe_process_id != 0
        && service_process_id == Some(pipe_process_id)
        && account.is_some_and(is_local_system_account)
}

#[cfg(not(feature = "test"))]
pub(super) fn verify_registered_service_pipe(
    pipe_path: &str,
    service_name: &str,
) -> Result<OwnedHandle> {
    let mut wide: Vec<u16> = OsStr::new(pipe_path).encode_wide().collect();
    if wide.contains(&0) {
        bail!("Windows named-pipe path contains NUL");
    }
    wide.push(0);

    let pipe = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_DATA | FILE_WRITE_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .context("failed to open the registered service pipe for identity verification");
    }
    let pipe = unsafe { OwnedHandle::from_raw_handle(pipe) };

    let mut pipe_process_id = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pipe_process_id) } == 0
        || pipe_process_id == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("failed to identify the Windows named-pipe server");
    }

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to connect to the Windows Service Control Manager")?;
    let service = manager
        .open_service(
            service_name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        )
        .with_context(|| format!("failed to query registered service {service_name:?}"))?;
    let status = service
        .query_status()
        .with_context(|| format!("failed to query status for service {service_name:?}"))?;
    let config = service
        .query_config()
        .with_context(|| format!("failed to query configuration for service {service_name:?}"))?;

    if !trusted_service_identity(
        pipe_process_id,
        status.process_id,
        config.account_name.as_deref(),
    ) {
        bail!(
            "Windows named-pipe server does not match the registered LocalSystem service process"
        );
    }
    Ok(pipe)
}

#[cfg(test)]
mod tests {
    use super::trusted_service_identity;
    use std::ffi::OsStr;

    #[test]
    fn accepts_the_registered_local_system_service_process() {
        assert!(trusted_service_identity(
            4242,
            Some(4242),
            Some(OsStr::new("LocalSystem")),
        ));
    }

    #[test]
    fn rejects_a_different_pipe_process() {
        assert!(!trusted_service_identity(
            31337,
            Some(4242),
            Some(OsStr::new("LocalSystem")),
        ));
    }

    #[test]
    fn rejects_a_non_system_service_account() {
        assert!(!trusted_service_identity(
            4242,
            Some(4242),
            Some(OsStr::new(".\\ordinary-user")),
        ));
    }

    #[test]
    fn accepts_the_explicit_nt_authority_system_name() {
        assert!(trusted_service_identity(
            4242,
            Some(4242),
            Some(OsStr::new("NT AUTHORITY\\SYSTEM")),
        ));
    }
}
