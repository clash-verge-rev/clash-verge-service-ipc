#[cfg(unix)]
pub fn set_ipc_socket_permissions(ipc_path: &str) -> std::io::Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    if Path::new(ipc_path).exists() {
        fs::set_permissions(ipc_path, fs::Permissions::from_mode(0o666))?;
    }

    Ok(())
}

#[cfg(windows)]
pub fn set_ipc_socket_permissions(_ipc_path: &str) -> std::io::Result<()> {
    Ok(())
}
