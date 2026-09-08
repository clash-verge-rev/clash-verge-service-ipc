//! Decides whether a core binary sits where only a privileged account can write.
//!
//! The service runs as root or LocalSystem, so an owner who can name any file would otherwise also
//! name what root runs. Owner authentication cannot carry that weight: every local account may
//! legitimately become an owner, so the trust has to come from the location instead.
//!
//! This is applied to the copy the installer staged, not to the path the client named. Demanding
//! that the client's own install directory be unwritable held for `%ProgramFiles%` but rejected
//! every operator who installed anywhere else; see `runtime_generation::assets::validate_core_path`.
//! What remains here is a check on the service's own directory, which passes by construction and is
//! worth keeping for exactly that reason: a directory that fails it is one someone has been
//! rearranging.

use crate::ServiceErrorCode;
use crate::core::auth::ServiceError;
use std::path::{Path, PathBuf};

/// Rejects a core the requesting account could have written.
///
/// `canonical` is expected to already be a canonicalized ordinary file.
pub(crate) fn require_trusted_core_location(canonical: &Path) -> Result<(), ServiceError> {
    // Integration tests stage cores in temporary directories that deliberately fail this rule.
    if cfg!(feature = "test") {
        return Ok(());
    }
    check_platform_location(canonical)
}

/// Builds the error every location-trust refusal carries; shared with the core resolution in
/// `runtime_generation::assets`, which enforces the same property on the approved copy.
pub(crate) fn untrusted(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ServiceErrorCode::InvalidInstallLocation, message)
}

/// Decides whether a core SOURCE sits where only a privileged account can write, and returns the
/// canonical path the verdict was rendered for.
///
/// The installer consults this before auto-staging a bundled core. A source that fails carries
/// whatever bytes the directory's writers last put there, and an elevated copy would launder those
/// bytes into the directory root executes from; such a source is staged only through an explicit
/// `--install-core` carrying a `--sha256` attestation of the bytes the caller actually obtained.
///
/// Callers must copy FROM THE RETURNED PATH. The argument may travel through a link or junction a
/// non-privileged account can retarget between this check and the copy; the canonical path has
/// every link resolved, and the check just proved that only privileged accounts can rearrange any
/// of its components.
pub fn require_trusted_core_source(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| anyhow::anyhow!("failed to canonicalize core source {path:?}: {error}"))?;
    require_trusted_core_location(&canonical)?;
    Ok(canonical)
}

#[cfg(target_os = "macos")]
fn check_platform_location(canonical: &Path) -> Result<(), ServiceError> {
    // The copy the service actually runs is staged under the installer-owned state root, which is
    // created root-owned and 0700. The walk stops there rather than continuing to `/`: above it
    // sits `/Library/Application Support`, which macOS ships writable by the `admin` group. That
    // is the same exposure `/Applications` has always carried, and the reason ownership alone was
    // never able to carry this check on macOS.
    let paths = crate::core::paths::service_paths();
    let state_root = paths.persistent_state_dir();
    if canonical.starts_with(state_root) {
        return require_root_owned_chain(canonical, Some(state_root));
    }
    if !is_trusted_macos_location(canonical) {
        return Err(untrusted(format!(
            "core path {canonical:?} is under neither {state_root:?} nor {MACOS_APPLICATIONS_ROOT}"
        )));
    }
    // The prefix says which neighborhood the file lives in, not who could have rewritten it.
    // Below `/Applications`, refuse anything group- or other-writable; the root itself stays
    // exempt because macOS ships it admin-group-writable by design, which is the exposure this
    // prefix rule accepted from the start. Drag-installed bundles arrive 0755 and pass.
    use std::os::unix::fs::MetadataExt as _;
    for component in canonical.ancestors() {
        if component == Path::new(MACOS_APPLICATIONS_ROOT) {
            break;
        }
        let metadata = std::fs::symlink_metadata(component)
            .map_err(|error| untrusted(format!("failed to inspect core path {component:?}: {error}")))?;
        if metadata.mode() & 0o022 != 0 {
            return Err(untrusted(format!(
                "core path {component:?} is writable by group or other"
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
const MACOS_APPLICATIONS_ROOT: &str = "/Applications";

/// `/Applications` is writable by the `admin` group but not by ordinary accounts, and the bundle
/// underneath it is routinely owned by whoever dragged it there, so ownership cannot be required
/// on macOS the way it is on Linux. `~/Applications` used to be accepted as well; it is writable
/// by the very account making the request, which left the check with nothing to enforce.
#[cfg(target_os = "macos")]
fn is_trusted_macos_location(canonical: &Path) -> bool {
    canonical.starts_with(MACOS_APPLICATIONS_ROOT)
}

/// Every component from the core down to `/` must be root-owned and unwritable by group or other.
/// Packaged installs land in root-owned prefixes, so this holds for DEB and RPM builds.
#[cfg(target_os = "linux")]
fn check_platform_location(canonical: &Path) -> Result<(), ServiceError> {
    require_root_owned_chain(canonical, None)
}

/// Requires every component up the tree to be root-owned and unwritable by group or other.
///
/// `stop_after` bounds the walk at a directory whose own parents the operating system manages, for
/// platforms where those parents are deliberately group-writable.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_root_owned_chain(canonical: &Path, stop_after: Option<&Path>) -> Result<(), ServiceError> {
    use std::os::unix::fs::MetadataExt as _;

    for component in canonical.ancestors() {
        let metadata = std::fs::symlink_metadata(component)
            .map_err(|error| untrusted(format!("failed to inspect core path {component:?}: {error}")))?;
        if metadata.uid() != 0 {
            return Err(untrusted(format!("core path {component:?} is not owned by root")));
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(untrusted(format!(
                "core path {component:?} is writable by group or other"
            )));
        }
        if stop_after == Some(component) {
            break;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn check_platform_location(canonical: &Path) -> Result<(), ServiceError> {
    windows_location::check(canonical)
}

#[cfg(windows)]
mod windows_location {
    use super::{ServiceError, untrusted};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::Path;
    use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, IsValidSid, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };

    /// `NT SERVICE\TrustedInstaller`, the owner Windows leaves on `%ProgramFiles%`.
    const TRUSTED_INSTALLER_SID: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const INHERIT_ONLY_ACE_FLAG: u8 = 0x08;

    /// Rights that let a principal put different bytes behind the core's own path.
    const FILE_HIJACK_RIGHTS: u32 = DELETE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | WRITE_DAC
        | WRITE_OWNER
        | GENERIC_WRITE
        | GENERIC_ALL;

    /// Rights that let a principal swap out a directory on the way to the core. Adding entries to
    /// a directory is deliberately absent: the stock `C:\` DACL grants `Authenticated Users`
    /// add-subdirectory through `(A;;LC;;;AU)`, and that cannot replace a child that already
    /// exists. `BUILTIN\Users` holds only read and execute there, so it is not the reason.
    const DIRECTORY_HIJACK_RIGHTS: u32 = DELETE | FILE_DELETE_CHILD | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

    pub(super) fn check(canonical: &Path) -> Result<(), ServiceError> {
        let trusted = TrustedSids::new()?;
        // `ancestors` yields the core itself first, then every directory up to the volume root.
        for (index, component) in canonical.ancestors().enumerate() {
            let is_core = index == 0;
            let hijack_rights = if is_core {
                FILE_HIJACK_RIGHTS
            } else {
                DIRECTORY_HIJACK_RIGHTS
            };
            let handle = open_for_security_review(component, !is_core)?;
            review_component(handle.as_raw_handle(), component, hijack_rights, &trusted)?;
        }
        Ok(())
    }

    fn review_component(
        handle: *mut c_void,
        component: &Path,
        hijack_rights: u32,
        trusted: &TrustedSids,
    ) -> Result<(), ServiceError> {
        let security = SecurityInfo::read(handle, component)?;
        if !trusted.contains(security.owner) {
            return Err(untrusted(format!(
                "core path {component:?} is owned by an account other than SYSTEM, Administrators or TrustedInstaller"
            )));
        }
        // A null DACL grants everyone everything; an empty one denies everyone.
        if security.dacl.is_null() {
            return Err(untrusted(format!("core path {component:?} has no DACL")));
        }

        for index in 0..u32::from(unsafe { (*security.dacl).AceCount }) {
            let mut ace = std::ptr::null_mut();
            if unsafe { GetAce(security.dacl, index, &mut ace) } == 0 || ace.is_null() {
                return Err(untrusted(format!("core path {component:?} has an unreadable DACL")));
            }
            let header = unsafe { *ace.cast::<ACE_HEADER>() };
            // Inherit-only entries do not apply to this object, only to children it later gains.
            if header.AceFlags & INHERIT_ONLY_ACE_FLAG != 0 {
                continue;
            }
            match header.AceType {
                ACCESS_DENIED_ACE_TYPE => continue,
                ACCESS_ALLOWED_ACE_TYPE => {}
                // Callback and object ACEs place the SID at a different offset; refuse rather
                // than read the wrong bytes.
                _ => {
                    return Err(untrusted(format!(
                        "core path {component:?} carries an ACE type this check cannot evaluate"
                    )));
                }
            }
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            if allowed.Mask & hijack_rights == 0 {
                continue;
            }
            let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast::<c_void>();
            if !trusted.contains(sid) {
                return Err(untrusted(format!(
                    "core path {component:?} is writable by an account other than SYSTEM, Administrators or TrustedInstaller"
                )));
            }
        }
        Ok(())
    }

    fn open_for_security_review(path: &Path, directory: bool) -> Result<std::fs::File, ServiceError> {
        let wide = wide_path(path)?;
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(untrusted(format!("core path {path:?} could not be opened for review")));
        }
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>, ServiceError> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(untrusted("core path contains NUL"));
        }
        wide.push(0);
        Ok(wide)
    }

    struct TrustedSids {
        trusted_installer: LocalSid,
    }

    impl TrustedSids {
        fn new() -> Result<Self, ServiceError> {
            Ok(Self {
                trusted_installer: LocalSid::from_string(TRUSTED_INSTALLER_SID)?,
            })
        }

        fn contains(&self, sid: PSID) -> bool {
            if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
                return false;
            }
            unsafe {
                IsWellKnownSid(sid, WinLocalSystemSid) != 0
                    || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                    || EqualSid(sid, self.trusted_installer.as_ptr()) != 0
            }
        }
    }

    struct LocalSid(*mut c_void);

    impl LocalSid {
        fn from_string(value: &str) -> Result<Self, ServiceError> {
            let mut wide: Vec<u16> = value.encode_utf16().collect();
            wide.push(0);
            let mut sid = std::ptr::null_mut();
            if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 || unsafe { IsValidSid(sid) } == 0 {
                return Err(untrusted("the TrustedInstaller SID could not be built"));
            }
            Ok(Self(sid))
        }

        fn as_ptr(&self) -> PSID {
            self.0
        }
    }

    impl Drop for LocalSid {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    struct SecurityInfo {
        owner: PSID,
        dacl: *mut ACL,
        _descriptor: LocalSecurityDescriptor,
    }

    impl SecurityInfo {
        fn read(handle: *mut c_void, component: &Path) -> Result<Self, ServiceError> {
            let mut owner = std::ptr::null_mut();
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            let status = unsafe {
                GetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            if status != 0 || descriptor.is_null() {
                return Err(untrusted(format!(
                    "core path {component:?} security could not be inspected"
                )));
            }
            Ok(Self {
                owner,
                dacl,
                _descriptor: LocalSecurityDescriptor(descriptor),
            })
        }
    }

    struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::is_trusted_macos_location;
    use std::path::Path;

    #[test]
    fn accepts_a_core_inside_the_applications_bundle() {
        assert!(is_trusted_macos_location(Path::new(
            "/Applications/Clash Verge.app/Contents/MacOS/verge-mihomo"
        )));
    }

    #[test]
    fn rejects_the_user_writable_home_applications_directory() {
        assert!(!is_trusted_macos_location(Path::new(
            "/Users/someone/Applications/Clash Verge.app/Contents/MacOS/verge-mihomo"
        )));
    }

    #[test]
    fn rejects_a_sibling_directory_that_merely_shares_the_prefix() {
        assert!(!is_trusted_macos_location(Path::new(
            "/Applications-elsewhere/verge-mihomo"
        )));
    }

    #[test]
    fn rejects_an_arbitrary_writable_path() {
        assert!(!is_trusted_macos_location(Path::new("/tmp/verge-mihomo")));
    }
}

#[cfg(all(test, not(feature = "test"), target_os = "macos"))]
mod production_gate_tests {
    use super::require_trusted_core_location;
    use std::path::Path;

    #[test]
    fn the_production_gate_rejects_a_core_outside_applications() {
        assert!(require_trusted_core_location(Path::new("/tmp/verge-mihomo")).is_err());
        // Ships with macOS as root-owned 0755, so it exercises the accept path with a real chain;
        // a fictional bundle path would now fail the writability inspection on a missing file.
        assert!(require_trusted_core_location(Path::new("/Applications/Utilities")).is_ok());
        assert!(
            require_trusted_core_location(Path::new(
                "/Applications/does-not-exist.app/Contents/MacOS/verge-mihomo"
            ))
            .is_err()
        );
    }
}
