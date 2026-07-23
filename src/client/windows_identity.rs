#[cfg(not(feature = "test"))]
use anyhow::{Context as _, Result, bail};
use std::ffi::OsStr;
#[cfg(not(feature = "test"))]
use std::os::windows::ffi::OsStrExt as _;
#[cfg(not(feature = "test"))]
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
#[cfg(not(feature = "test"))]
use std::time::{Duration, Instant};
#[cfg(not(feature = "test"))]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(not(feature = "test"))]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WriteFile,
};
#[cfg(not(feature = "test"))]
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, PeekNamedPipe};

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

fn identity_probe_request(auth_value: &str) -> Option<Vec<u8>> {
    if auth_value.contains(['\r', '\n']) {
        return None;
    }
    Some(
        format!(
            "GET /magic HTTP/1.1\r\nHost: localhost\r\nX-IPC-Magic: {auth_value}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
    )
}

fn completed_identity_probe_response(response: &[u8]) -> Option<bool> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&response[..header_end]).ok()?;
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    let body_start = header_end + 4;
    let response_length = body_start.checked_add(content_length)?;
    if response.len() < response_length {
        return None;
    }
    Some(
        headers.starts_with("HTTP/1.1 200 ")
            && response[body_start..response_length] == *b"Tunglies!",
    )
}

#[cfg(not(feature = "test"))]
pub(super) fn verify_registered_service_pipe(
    pipe_path: &str,
    service_name: &str,
    auth_value: &str,
) -> Result<()> {
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
            0,
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

    let request = identity_probe_request(auth_value)
        .context("IPC authentication value cannot contain HTTP line breaks")?;
    let request_length =
        u32::try_from(request.len()).context("IPC identity probe request is too large")?;
    let mut written = 0;
    if unsafe {
        WriteFile(
            pipe.as_raw_handle(),
            request.as_ptr().cast(),
            request_length,
            &mut written,
            std::ptr::null_mut(),
        )
    } == 0
        || written != request_length
    {
        return Err(std::io::Error::last_os_error())
            .context("failed to complete the verified service identity probe");
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut response = Vec::with_capacity(512);
    loop {
        if let Some(valid) = completed_identity_probe_response(&response) {
            if valid {
                return Ok(());
            }
            bail!("registered service returned an invalid identity probe response");
        }
        if response.len() > 64 * 1024 {
            bail!("registered service identity probe response is too large");
        }
        if Instant::now() >= deadline {
            bail!("registered service identity probe response timed out");
        }

        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect the registered service identity probe response");
        }
        if available == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        let chunk_length = available.min(4096);
        let mut chunk = vec![0_u8; chunk_length as usize];
        let mut read = 0;
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                pipe.as_raw_handle(),
                chunk.as_mut_ptr().cast(),
                chunk_length,
                &mut read,
                std::ptr::null_mut(),
            )
        } == 0
            || read == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("failed to read the registered service identity probe response");
        }
        response.extend_from_slice(&chunk[..read as usize]);
    }
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

    #[test]
    fn identity_probe_is_a_complete_connection_close_request() {
        let request = super::identity_probe_request("expected").expect("valid auth value");
        let request = String::from_utf8(request).expect("request is ASCII");

        assert!(request.starts_with("GET /magic HTTP/1.1\r\n"));
        assert!(request.contains("X-IPC-Magic: expected\r\n"));
        assert!(request.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn identity_probe_rejects_header_injection() {
        assert!(super::identity_probe_request("expected\r\nInjected: yes").is_none());
    }

    #[test]
    fn identity_probe_requires_a_complete_success_response() {
        let valid = b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nTunglies!";
        let partial = &valid[..valid.len() - 1];
        let rejected = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";

        assert_eq!(super::completed_identity_probe_response(partial), None);
        assert_eq!(super::completed_identity_probe_response(valid), Some(true));
        assert_eq!(
            super::completed_identity_probe_response(rejected),
            Some(false)
        );
    }
}
