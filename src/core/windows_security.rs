use anyhow::{Context as _, Result, bail};
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError,
    INVALID_HANDLE_VALUE, LocalFree,
};
#[cfg(not(feature = "test"))]
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetSecurityInfo,
};
#[cfg(not(feature = "test"))]
use windows_sys::Win32::Security::{
    CreateWellKnownSid, EqualSid, OWNER_SECURITY_INFORMATION, SECURITY_MAX_SID_SIZE,
    WinLocalSystemSid,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    GetFileInformationByHandle, GetFileType, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};

#[cfg(not(feature = "test"))]
const PRIVATE_SERVICE_DIRECTORY_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
#[cfg(feature = "test")]
const PRIVATE_SERVICE_DIRECTORY_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
#[cfg(not(feature = "test"))]
const PRIVATE_SERVICE_FILE_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)";
#[cfg(feature = "test")]
const PRIVATE_SERVICE_FILE_SDDL: &str = "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)";

pub(crate) fn ensure_private_service_directory(path: &Path) -> Result<()> {
    let descriptor = LocalDescriptor::from_sddl(PRIVATE_SERVICE_DIRECTORY_SDDL)?;
    let wide = wide_path(path)?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide.as_ptr(), &mut attributes) } == 0
        && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to create private service directory {path:?}"));
    }

    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to open private service directory {path:?}"));
    }
    let handle = OwnedHandle(handle);

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || unsafe { GetFileType(handle.0) } != FILE_TYPE_DISK
    {
        bail!("service directory {path:?} is not an ordinary directory");
    }
    #[cfg(not(feature = "test"))]
    ensure_local_system_owner(handle.0, path)?;
    descriptor.apply_dacl(handle.0)?;
    Ok(())
}

pub(crate) fn secure_private_directory(path: &Path) -> Result<()> {
    ensure_private_service_directory(path)
}

pub(crate) fn secure_private_service_file_if_exists(path: &Path) -> Result<()> {
    let descriptor = LocalDescriptor::from_sddl(PRIVATE_SERVICE_FILE_SDDL)?;
    let wide = wide_path(path)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(()),
            _ => Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open private service file {path:?}")),
        };
    }
    let handle = OwnedHandle(handle);
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0
        || information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            != 0
        || unsafe { GetFileType(handle.0) } != FILE_TYPE_DISK
    {
        bail!("service file {path:?} is not an ordinary file");
    }
    #[cfg(not(feature = "test"))]
    ensure_local_system_owner(handle.0, path)?;
    descriptor.apply_dacl(handle.0)
}

#[cfg(not(feature = "test"))]
fn ensure_local_system_owner(handle: *mut std::ffi::c_void, path: &Path) -> Result<()> {
    let mut owner = std::ptr::null_mut();
    let mut security = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut security,
        )
    };
    if status != 0 || security.is_null() || owner.is_null() {
        bail!("failed to inspect service directory owner: Windows error {status}");
    }
    let security = LocalDescriptor(security);
    let words = (SECURITY_MAX_SID_SIZE as usize).div_ceil(std::mem::size_of::<usize>());
    let mut system_sid = vec![0_usize; words];
    let mut sid_size = SECURITY_MAX_SID_SIZE;
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            std::ptr::null_mut(),
            system_sid.as_mut_ptr().cast(),
            &mut sid_size,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("failed to create LocalSystem SID");
    }
    if unsafe { EqualSid(owner, system_sid.as_mut_ptr().cast()) } == 0 {
        bail!("service directory {path:?} is not owned by LocalSystem");
    }
    drop(security);
    Ok(())
}

fn wide_path(path: &Path) -> Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        bail!("service directory path contains NUL");
    }
    wide.push(0);
    Ok(wide)
}

struct OwnedHandle(*mut std::ffi::c_void);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct LocalDescriptor(*mut std::ffi::c_void);

impl LocalDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self> {
        let mut wide: Vec<u16> = sddl.encode_utf16().collect();
        wide.push(0);
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
            || descriptor.is_null()
        {
            return Err(std::io::Error::last_os_error())
                .context("failed to build private service directory security descriptor");
        }
        Ok(Self(descriptor))
    }

    fn apply_dacl(&self, handle: *mut std::ffi::c_void) -> Result<()> {
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = std::ptr::null_mut();
        if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) }
            == 0
            || present == 0
            || dacl.is_null()
        {
            bail!("private service directory descriptor has no DACL");
        }
        let status = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if status != 0 {
            bail!("failed to apply private service directory DACL: Windows error {status}");
        }
        Ok(())
    }
}

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::PRIVATE_SERVICE_DIRECTORY_SDDL;

    #[test]
    fn private_directory_dacl_excludes_ordinary_users() {
        assert!(PRIVATE_SERVICE_DIRECTORY_SDDL.starts_with("D:P"));
        assert!(PRIVATE_SERVICE_DIRECTORY_SDDL.contains(";;;SY)"));
        assert!(PRIVATE_SERVICE_DIRECTORY_SDDL.contains(";;;BA)"));
        assert!(!PRIVATE_SERVICE_DIRECTORY_SDDL.contains(";;;WD)"));
        assert!(!PRIVATE_SERVICE_DIRECTORY_SDDL.contains(";;;AU)"));
    }
}
