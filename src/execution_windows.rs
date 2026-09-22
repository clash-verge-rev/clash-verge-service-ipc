use anyhow::{Context as _, Result, bail};
use platform_lib::{
    Error as WindowsServiceError,
    service::{ServiceAccess, ServiceState},
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use windows_sys::Win32::{
    Foundation::{ERROR_NO_MORE_FILES, ERROR_SERVICE_DOES_NOT_EXIST, INVALID_HANDLE_VALUE},
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    },
};

pub(super) fn require_stopped_service() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service(crate::WINDOWS_SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => {
            let status = service.query_status()?;
            if status.current_state != ServiceState::Stopped || status.process_id.is_some_and(|pid| pid != 0) {
                bail!(
                    "service is {:?}; Sidecar fallback requires a stopped service",
                    status.current_state
                );
            }
            Ok(())
        }
        Err(WindowsServiceError::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) =>
        {
            Ok(())
        }
        Err(error) => Err(error).context("cannot confirm that the service has stopped"),
    }
}

pub(super) fn require_no_core_process(include_service: bool) -> Result<()> {
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("cannot inspect existing core processes");
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        let length = entry
            .szExeFile
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
        // SCM can be stopped while a core from an earlier service process remains alive.
        if ["verge-mihomo", "verge-mihomo-alpha"]
            .iter()
            .any(|core| name.eq_ignore_ascii_case(&format!("{core}.exe")))
            || (include_service && name.eq_ignore_ascii_case("clash-verge-service.exe"))
        {
            bail!(
                "process {name} (PID {}) is still running; refusing a second core",
                entry.th32ProcessID
            );
        }
        found = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
        return Err(error).context("core process enumeration did not complete");
    }
    Ok(())
}
