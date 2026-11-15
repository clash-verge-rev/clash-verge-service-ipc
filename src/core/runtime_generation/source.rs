use super::assets::{invalid_asset, validate_source};
use crate::core::auth::{AuthenticatedOwner, ServiceError};
use std::fs::File;
use std::path::{Component, Path};

pub(super) fn open_runtime_asset(
    owner: &AuthenticatedOwner,
    bundle_root: Option<&Path>,
    source: &str,
) -> Result<File, ServiceError> {
    let source = validate_source(owner, bundle_root, source)?;
    open_validated_asset(owner, bundle_root, &source)
}

#[cfg(unix)]
fn open_validated_asset(
    owner: &AuthenticatedOwner,
    bundle_root: Option<&Path>,
    source: &Path,
) -> Result<File, ServiceError> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _};

    let crate::OwnerIdentity::Unix { uid, .. } = owner.identity else {
        return Err(invalid_asset("Unix asset requires a Unix owner"));
    };
    let failure = |error| invalid_asset(format!("failed to open runtime asset {source:?}: {error}"));
    let mut parent = File::open("/").map_err(failure)?;
    let mut walked = std::path::PathBuf::from("/");
    let mut protected_bundle = bundle_root.is_some_and(|root| source.starts_with(root));
    let mut publicly_traversable = true;
    let mut components = source.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            if component == Component::RootDir {
                continue;
            }
            return Err(invalid_asset("runtime asset path is not canonical"));
        };
        let last = components.peek().is_none();
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid_asset("asset path contains NUL"))?;
        let flags = platform_lib::O_RDONLY
            | platform_lib::O_NOFOLLOW
            | platform_lib::O_CLOEXEC
            | if last {
                platform_lib::O_NONBLOCK
            } else {
                platform_lib::O_DIRECTORY
            };
        let fd = unsafe { platform_lib::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(failure(std::io::Error::last_os_error()));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(failure)?;
        walked.push(component.as_os_str());
        let caller_owned = metadata.uid() == uid;
        if walked == owner.app_data_root && (!metadata.is_dir() || !caller_owned) {
            return Err(invalid_asset("opened application root has an unexpected owner or type"));
        }
        if !last && !caller_owned {
            publicly_traversable &= metadata.mode() & 0o011 == 0o011 && no_extended_acl(&file);
        }
        if bundle_root.is_some_and(|root| walked.starts_with(root)) {
            protected_bundle &= metadata.uid() == 0 && metadata.mode() & 0o022 == 0 && no_extended_acl(&file);
        }
        if last {
            let publicly_readable = publicly_traversable && metadata.mode() & 0o044 == 0o044 && no_extended_acl(&file);
            if !metadata.is_file() || (!caller_owned && !publicly_readable && !protected_bundle) {
                return Err(invalid_asset(
                    "opened runtime asset is not caller-owned or in a protected bundle",
                ));
            }
            return Ok(file);
        }
        parent = file;
    }
    Err(invalid_asset("runtime asset has no file component"))
}

#[cfg(target_os = "linux")]
fn no_extended_acl(file: &File) -> bool {
    use std::os::fd::AsRawFd as _;
    (unsafe {
        platform_lib::fgetxattr(
            file.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    }) == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::ENODATA)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn no_extended_acl(_file: &File) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn no_extended_acl(file: &File) -> bool {
    use std::os::fd::AsRawFd as _;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: u32) -> *mut std::ffi::c_void;
        fn acl_get_entry(acl: *mut std::ffi::c_void, entry: i32, result: *mut *mut std::ffi::c_void) -> i32;
        fn acl_free(acl: *mut std::ffi::c_void) -> i32;
    }
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::ENOENT);
    }
    let mut entry = std::ptr::null_mut();
    let empty = unsafe { acl_get_entry(acl, 0, &mut entry) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::EINVAL);
    unsafe { acl_free(acl) };
    empty
}

#[cfg(windows)]
fn open_windows_asset_component(
    parent: Option<&File>,
    name: &std::ffi::OsStr,
    directory: bool,
) -> Result<File, ServiceError> {
    use std::os::windows::{
        ffi::OsStrExt as _,
        io::{AsRawHandle as _, FromRawHandle as _},
    };
    use windows_sys::Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
            FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    };
    use windows_sys::Win32::{
        Foundation::{OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, UNICODE_STRING},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_GENERIC_READ,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_TYPE_DISK,
            GetFileInformationByHandle, GetFileType, READ_CONTROL, SYNCHRONIZE,
        },
        System::IO::IO_STATUS_BLOCK,
    };
    let mut wide: Vec<u16> = name.encode_wide().collect();
    let length = u16::try_from(wide.len() * 2).map_err(|_| invalid_asset("asset component name is too long"))?;
    let mut name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.map_or(std::ptr::null_mut(), |file| file.as_raw_handle()),
        ObjectName: &mut name,
        Attributes: OBJ_CASE_INSENSITIVE | if parent.is_some() { OBJ_DONT_REPARSE } else { 0 },
        SecurityDescriptor: std::ptr::null_mut(),
        SecurityQualityOfService: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            READ_CONTROL
                | SYNCHRONIZE
                | FILE_READ_ATTRIBUTES
                | if directory { FILE_TRAVERSE } else { FILE_GENERIC_READ },
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | if directory {
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE
                },
            std::ptr::null(),
            0,
        )
    };
    if status < 0 || handle.is_null() {
        return Err(invalid_asset(format!(
            "failed to open pinned asset component: NTSTATUS {:#x}",
            status as u32
        )));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || unsafe { GetFileType(handle) } != FILE_TYPE_DISK
    {
        return Err(invalid_asset(
            "opened asset component has an unexpected type or reparse point",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_validated_asset(
    owner: &AuthenticatedOwner,
    bundle_root: Option<&Path>,
    source: &Path,
) -> Result<File, ServiceError> {
    use std::os::windows::io::AsRawHandle as _;
    use std::path::Prefix;
    let crate::OwnerIdentity::Windows { sid } = &owner.identity else {
        return Err(invalid_asset("Windows asset requires a Windows owner"));
    };
    let mut components = source.components().peekable();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(invalid_asset("runtime assets require a local volume")),
        },
        _ => return Err(invalid_asset("runtime asset has no volume root")),
    };
    if components.next() != Some(Component::RootDir) {
        return Err(invalid_asset("runtime asset path is not absolute"));
    }
    let root = format!("\\??\\{}:\\", char::from(drive));
    let mut parent = open_windows_asset_component(None, std::ffi::OsStr::new(&root), true)?;
    let mut walked = source.ancestors().last().unwrap().to_path_buf();
    let mut protected_bundle = bundle_root.is_some_and(|root| source.starts_with(root))
        && crate::core::trusted_core_location::review_open_asset_component(parent.as_raw_handle(), &walked, true)
            .is_ok();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(invalid_asset("runtime asset path is not canonical"));
        };
        let last = components.peek().is_none();
        let file = open_windows_asset_component(Some(&parent), name, !last)?;
        walked.push(name);
        let caller_owned = crate::core::auth::validate_open_file_owner(&file, sid).is_ok();
        if walked == owner.app_data_root && !caller_owned {
            return Err(invalid_asset("opened application root has an unexpected owner"));
        }
        if protected_bundle {
            protected_bundle &=
                crate::core::trusted_core_location::review_open_asset_component(file.as_raw_handle(), &walked, !last)
                    .is_ok();
        }
        if last {
            if !caller_owned && !protected_bundle {
                return Err(invalid_asset(
                    "opened runtime asset is not caller-owned or in a protected bundle",
                ));
            }
            return Ok(file);
        }
        parent = file;
    }
    Err(invalid_asset("runtime asset has no file component"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn another_users_public_bundle_asset_is_allowed_but_a_private_hardlink_is_not() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let root = Path::new("/tmp").join(format!("service-asset-multiuser-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
        let source = root.join("asset");
        std::fs::write(&source, b"public bundle fixture")?;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644))?;
        let owner = AuthenticatedOwner {
            key: "another-user".into(),
            app_data_root: root.join("caller"),
            identity: crate::OwnerIdentity::Unix {
                uid: unsafe { platform_lib::geteuid() }.wrapping_add(1),
                gid: unsafe { platform_lib::getegid() },
            },
        };
        assert!(open_runtime_asset(&owner, Some(&root), source.to_str().unwrap()).is_ok());
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o604))?;
        assert!(open_runtime_asset(&owner, Some(&root), source.to_str().unwrap()).is_err());
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644))?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o701))?;
        assert!(open_runtime_asset(&owner, Some(&root), source.to_str().unwrap()).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
        let link = root.join("private-link");
        std::fs::hard_link(&source, &link)?;
        assert!(open_runtime_asset(&owner, Some(&root), link.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extended_acl_cannot_authorize_a_foreign_owned_bundle_asset() -> anyhow::Result<()> {
        let path = std::env::temp_dir().join(format!("service-asset-acl-{}", std::process::id()));
        std::fs::write(&path, b"harmless ACL fixture")?;
        let file = File::open(&path)?;
        assert!(no_extended_acl(&file));
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone deny read"])
            .arg(&path)
            .status()?;
        anyhow::ensure!(status.success(), "could not create fixture ACL");
        assert!(!no_extended_acl(&file));
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn source_and_ancestor_replacement_cannot_redirect_an_opened_asset() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("service-asset-handle-{}", std::process::id()));
        std::fs::create_dir_all(root.join("data"))?;
        let root = root.canonicalize()?;
        let source = root.join("data/asset");
        std::fs::write(&source, b"approved fixture")?;
        let owner = AuthenticatedOwner {
            key: "fixture".into(),
            app_data_root: root.clone(),
            identity: crate::OwnerIdentity::Unix {
                uid: unsafe { platform_lib::geteuid() },
                gid: unsafe { platform_lib::getegid() },
            },
        };
        let validated = validate_source(&owner, None, source.to_str().unwrap())?;
        let mut opened = open_runtime_asset(&owner, None, source.to_str().unwrap())?;
        std::fs::rename(root.join("data"), root.join("original"))?;
        std::fs::create_dir(root.join("outside"))?;
        std::fs::write(root.join("outside/asset"), b"unrelated harmless fixture")?;
        std::os::unix::fs::symlink(root.join("outside"), root.join("data"))?;
        assert!(open_validated_asset(&owner, None, &validated).is_err());
        let mut content = String::new();
        opened.read_to_string(&mut content)?;
        assert_eq!(content, "approved fixture");
        std::fs::remove_file(root.join("data"))?;
        std::fs::create_dir(root.join("data"))?;
        std::os::unix::fs::symlink(root.join("outside/asset"), &source)?;
        assert!(open_validated_asset(&owner, None, &validated).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt as _,
        io::{AsRawHandle as _, FromRawHandle as _},
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, OPEN_EXISTING,
        },
        System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT, SystemServices::IO_REPARSE_TAG_MOUNT_POINT},
    };

    #[test]
    fn converting_a_held_ancestor_to_a_junction_cannot_redirect_a_child_open() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("service-asset-junction-{}", std::process::id()));
        std::fs::create_dir_all(root.join("data"))?;
        std::fs::create_dir_all(root.join("outside"))?;
        let root = root.canonicalize()?;
        std::fs::write(root.join("outside/asset"), b"unrelated harmless fixture")?;
        let nt_root = root.to_string_lossy().replace(r"\\?\", r"\??\");
        let root_handle = open_windows_asset_component(None, std::ffi::OsStr::new(&nt_root), true)?;
        let parent = open_windows_asset_component(Some(&root_handle), std::ffi::OsStr::new("data"), true)?;
        let wide: Vec<_> = root.join("data").as_os_str().encode_wide().chain(Some(0)).collect();
        let mutation = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_WRITE_ATTRIBUTES | FILE_WRITE_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        anyhow::ensure!(
            mutation != INVALID_HANDLE_VALUE,
            "fixture mutation handle: {}",
            std::io::Error::last_os_error()
        );
        let mutation = unsafe { File::from_raw_handle(mutation) };
        let target = root.join("outside");
        let print: Vec<u16> = target.as_os_str().encode_wide().collect();
        let substitute: Vec<u16> = target
            .to_string_lossy()
            .replace(r"\\?\", r"\??\")
            .encode_utf16()
            .collect();
        let mut bytes = Vec::new();
        bytes.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        bytes.extend((8_u16 + ((substitute.len() + print.len() + 2) * 2) as u16).to_le_bytes());
        for value in [
            0_u16,
            0,
            (substitute.len() * 2) as u16,
            ((substitute.len() + 1) * 2) as u16,
            (print.len() * 2) as u16,
        ] {
            bytes.extend(value.to_le_bytes());
        }
        for value in substitute.into_iter().chain(Some(0)).chain(print).chain(Some(0)) {
            bytes.extend(value.to_le_bytes());
        }
        let mut returned = 0;
        anyhow::ensure!(
            unsafe {
                DeviceIoControl(
                    mutation.as_raw_handle(),
                    FSCTL_SET_REPARSE_POINT,
                    bytes.as_ptr().cast(),
                    bytes.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } != 0,
            "fixture reparse mutation: {}",
            std::io::Error::last_os_error()
        );
        assert!(open_windows_asset_component(Some(&parent), std::ffi::OsStr::new("asset"), false).is_err());
        drop(mutation);
        drop(parent);
        drop(root_handle);
        std::fs::remove_dir(root.join("data"))?;
        assert_eq!(
            std::fs::read(root.join("outside/asset"))?,
            b"unrelated harmless fixture"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
