use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(any(windows, test))]
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ProcessIdentity {
    pub(super) executable: String,
    pub(super) started_at: u64,
}

#[cfg(any(unix, test))]
fn checked_unix_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|pid| *pid > 0)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_process_stat(stat: &str) -> Result<(char, u64)> {
    let mut fields = stat
        .rsplit_once(')')
        .ok_or_else(|| anyhow::anyhow!("invalid process stat"))?
        .1
        .split_whitespace();
    let state = fields
        .next()
        .and_then(|value| value.chars().next())
        .ok_or_else(|| anyhow::anyhow!("missing process state"))?;
    let started_at = fields
        .nth(18)
        .ok_or_else(|| anyhow::anyhow!("missing process start time"))?
        .parse()?;
    Ok((state, started_at))
}

#[cfg(any(windows, test))]
fn termination_order(root_pid: u32, relations: &[(u32, u32)]) -> Vec<u32> {
    fn visit(
        pid: u32,
        children: &HashMap<u32, Vec<u32>>,
        visited: &mut HashSet<u32>,
        order: &mut Vec<u32>,
    ) {
        if pid == 0 || !visited.insert(pid) {
            return;
        }
        if let Some(child_pids) = children.get(&pid) {
            for &child_pid in child_pids {
                visit(child_pid, children, visited, order);
            }
        }
        order.push(pid);
    }

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(child_pid, parent_pid) in relations {
        children.entry(parent_pid).or_default().push(child_pid);
    }
    let mut order = Vec::new();
    visit(root_pid, &children, &mut HashSet::new(), &mut order);
    order
}

#[cfg(windows)]
struct OwnedWindowsHandle(windows_sys::Win32::Foundation::HANDLE);

// Windows kernel handles remain valid across threads until their final CloseHandle.
#[cfg(windows)]
unsafe impl Send for OwnedWindowsHandle {}

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn windows_process_identity_details_from_handle(
    handle: &OwnedWindowsHandle,
) -> Result<ProcessIdentity> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetProcessTimes, QueryFullProcessImageNameW};

    let mut path = vec![0u16; 32_768];
    let mut path_len = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, path.as_mut_ptr(), &mut path_len) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    path.truncate(path_len as usize);
    let executable = std::path::PathBuf::from(String::from_utf16(&path)?)
        .canonicalize()?
        .to_string_lossy()
        .into_owned();
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let started_at = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    Ok(ProcessIdentity {
        executable,
        started_at,
    })
}

#[cfg(windows)]
fn windows_process_identity_from_handle(
    handle: &OwnedWindowsHandle,
) -> Result<Option<ProcessIdentity>> {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    match unsafe { WaitForSingleObject(handle.0, 0) } {
        WAIT_OBJECT_0 => return Ok(None),
        WAIT_TIMEOUT => {}
        _ => return Err(std::io::Error::last_os_error().into()),
    }
    Ok(Some(windows_process_identity_details_from_handle(handle)?))
}

pub(super) fn process_identity(pid: u32) -> Result<Option<ProcessIdentity>> {
    #[cfg(unix)]
    if checked_unix_pid(pid).is_none() {
        return Ok(None);
    }

    #[cfg(target_os = "linux")]
    {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (state, started_at) = parse_linux_process_stat(&stat)?;
        if state == 'Z' {
            return Ok(None);
        }
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))?
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let confirmed_stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (confirmed_state, confirmed_started_at) = parse_linux_process_stat(&confirmed_stat)?;
        if confirmed_state == 'Z' {
            return Ok(None);
        }
        if confirmed_started_at != started_at {
            bail!("process {pid} changed while reading its identity");
        }
        Ok(Some(ProcessIdentity {
            executable,
            started_at,
        }))
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStringExt as _;

        let unix_pid = checked_unix_pid(pid).expect("Unix PID was validated above");
        let mut info = unsafe { std::mem::zeroed::<platform_lib::proc_bsdinfo>() };
        let info_len = unsafe {
            platform_lib::proc_pidinfo(
                unix_pid,
                platform_lib::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut platform_lib::proc_bsdinfo).cast(),
                std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32,
            )
        };
        if info_len != std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32 {
            let error = std::io::Error::last_os_error();
            return if is_process_alive(pid) {
                Err(error.into())
            } else {
                Ok(None)
            };
        }
        if info.pbi_status == platform_lib::SZOMB {
            return Ok(None);
        }
        let mut path = vec![0u8; platform_lib::PROC_PIDPATHINFO_MAXSIZE as usize];
        let path_len = unsafe {
            platform_lib::proc_pidpath(unix_pid, path.as_mut_ptr().cast(), path.len() as u32)
        };
        if path_len <= 0 {
            let error = std::io::Error::last_os_error();
            return if is_process_alive(pid) {
                Err(error.into())
            } else {
                Ok(None)
            };
        }
        path.truncate(path_len as usize);
        let executable = std::path::PathBuf::from(std::ffi::OsString::from_vec(path))
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let mut confirmed = unsafe { std::mem::zeroed::<platform_lib::proc_bsdinfo>() };
        let confirmed_len = unsafe {
            platform_lib::proc_pidinfo(
                unix_pid,
                platform_lib::PROC_PIDTBSDINFO,
                0,
                (&mut confirmed as *mut platform_lib::proc_bsdinfo).cast(),
                std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32,
            )
        };
        if confirmed_len != std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32 {
            let error = std::io::Error::last_os_error();
            return if is_process_alive(pid) {
                Err(error.into())
            } else {
                Ok(None)
            };
        }
        if confirmed.pbi_status == platform_lib::SZOMB {
            return Ok(None);
        }
        if (confirmed.pbi_start_tvsec, confirmed.pbi_start_tvusec)
            != (info.pbi_start_tvsec, info.pbi_start_tvusec)
        {
            bail!("process {pid} changed while reading its identity");
        }
        Ok(Some(ProcessIdentity {
            executable,
            started_at: info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
        }))
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error().map(|code| code as u32) == Some(ERROR_INVALID_PARAMETER)
            {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        let handle = OwnedWindowsHandle(handle);
        windows_process_identity_from_handle(&handle)
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = pid;
        bail!("process identity is unsupported on this Unix platform")
    }
}

pub(super) fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Some(unix_pid) = checked_unix_pid(pid) else {
            return false;
        };
        let result = unsafe { platform_lib::kill(unix_pid, 0) };
        let exists = result == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::EPERM);
        if !exists {
            return false;
        }

        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| parse_linux_process_stat(&stat).ok())
                .is_none_or(|(state, _)| state != 'Z')
        }

        #[cfg(target_os = "macos")]
        {
            let mut info = unsafe { std::mem::zeroed::<platform_lib::proc_bsdinfo>() };
            let info_len = unsafe {
                platform_lib::proc_pidinfo(
                    unix_pid,
                    platform_lib::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut platform_lib::proc_bsdinfo).cast(),
                    std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32,
                )
            };
            if info_len != std::mem::size_of::<platform_lib::proc_bsdinfo>() as i32 {
                return std::io::Error::last_os_error().raw_os_error() != Some(platform_lib::ESRCH);
            }
            info.pbi_status != platform_lib::SZOMB
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        true
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

        let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return std::io::Error::last_os_error()
                .raw_os_error()
                .is_some_and(|code| code as u32 == ERROR_ACCESS_DENIED);
        }
        let handle = OwnedWindowsHandle(handle);
        match unsafe { WaitForSingleObject(handle.0, 0) } {
            WAIT_OBJECT_0 => false,
            WAIT_TIMEOUT => true,
            _ => true,
        }
    }
}

#[cfg(windows)]
fn windows_process_relations() -> Result<Vec<(u32, u32)>> {
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let snapshot = OwnedWindowsHandle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error().map(|code| code as u32) == Some(ERROR_NO_MORE_FILES) {
            Ok(Vec::new())
        } else {
            Err(error.into())
        };
    }

    let mut relations = Vec::new();
    loop {
        relations.push((entry.th32ProcessID, entry.th32ParentProcessID));
        if unsafe { Process32NextW(snapshot.0, &mut entry) } != 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().map(|code| code as u32) == Some(ERROR_NO_MORE_FILES) {
            break;
        }
        return Err(error.into());
    }
    Ok(relations)
}

#[cfg(windows)]
fn windows_handle_is_signaled(handle: &OwnedWindowsHandle) -> Result<bool> {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    match unsafe { WaitForSingleObject(handle.0, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(std::io::Error::last_os_error().into()),
    }
}

#[cfg(windows)]
fn open_windows_termination_handle(pid: u32) -> Result<Option<OwnedWindowsHandle>> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
            0,
            pid,
        )
    };
    if !handle.is_null() {
        return Ok(Some(OwnedWindowsHandle(handle)));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error().map(|value| value as u32) == Some(ERROR_INVALID_PARAMETER) {
        Ok(None)
    } else {
        Err(error.into())
    }
}

#[cfg(windows)]
fn terminate_windows_process_tree(
    pid: u32,
    expected_identity: Option<&ProcessIdentity>,
) -> Result<Vec<(u32, OwnedWindowsHandle)>> {
    let Some(root_handle) = open_windows_termination_handle(pid)? else {
        return Ok(Vec::new());
    };
    terminate_windows_process_tree_with_root_handle(pid, root_handle, expected_identity)
}

#[cfg(windows)]
fn terminate_windows_process_tree_with_root_handle(
    pid: u32,
    root_handle: OwnedWindowsHandle,
    expected_identity: Option<&ProcessIdentity>,
) -> Result<Vec<(u32, OwnedWindowsHandle)>> {
    use windows_sys::Win32::System::Threading::TerminateProcess;

    if let Some(expected_identity) = expected_identity {
        let current_identity = windows_process_identity_details_from_handle(&root_handle)?;
        if &current_identity != expected_identity {
            bail!("process {pid} identity changed before termination");
        }
    }

    let relations = windows_process_relations()?;
    let order = termination_order(pid, &relations);
    let mut targets = Vec::with_capacity(order.len());
    let mut root_handle = Some(root_handle);
    for candidate in order {
        if candidate == pid {
            targets.push((
                candidate,
                root_handle.take().expect("root PID is ordered once"),
            ));
        } else if let Some(handle) = open_windows_termination_handle(candidate)? {
            targets.push((candidate, handle));
        }
    }

    let terminate = |candidate: u32, handle: &OwnedWindowsHandle| -> Result<()> {
        if windows_handle_is_signaled(handle)? {
            return Ok(());
        }
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            let error = std::io::Error::last_os_error();
            return Err(anyhow::anyhow!(
                "failed to terminate process {candidate}: {error}"
            ));
        }
        Ok(())
    };
    let (_, root_handle) = targets
        .iter()
        .find(|(candidate, _)| *candidate == pid)
        .expect("root PID is always ordered");
    terminate(pid, root_handle)?;
    for (candidate, handle) in &targets {
        if *candidate != pid {
            terminate(*candidate, handle)?;
        }
    }
    Ok(targets)
}

async fn terminate_process_inner(
    pid: u32,
    expected_identity: Option<&ProcessIdentity>,
) -> Result<()> {
    #[cfg(unix)]
    {
        let unix_pid = checked_unix_pid(pid)
            .ok_or_else(|| anyhow::anyhow!("invalid Unix process ID {pid}"))?;
        let termination_identity = if let Some(expected_identity) = expected_identity {
            let Some(current_identity) = process_identity(pid)? else {
                return Ok(());
            };
            if &current_identity != expected_identity {
                bail!("process {pid} identity changed before termination");
            }
            Some(expected_identity.clone())
        } else {
            process_identity(pid).ok().flatten()
        };
        warn!("Terminating process {}", pid);
        if unsafe { platform_lib::kill(unix_pid, platform_lib::SIGTERM) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(platform_lib::ESRCH)
        {
            return Err(std::io::Error::last_os_error().into());
        }

        for _ in 0..10 {
            if !is_process_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        if let Some(termination_identity) = &termination_identity {
            let Some(current_identity) = process_identity(pid)? else {
                return Ok(());
            };
            if &current_identity != termination_identity {
                bail!("process {pid} identity changed before SIGKILL");
            }
        }
        warn!("Process {} did not exit, sending SIGKILL", pid);
        if unsafe { platform_lib::kill(unix_pid, platform_lib::SIGKILL) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(platform_lib::ESRCH)
        {
            return Err(std::io::Error::last_os_error().into());
        }
        for _ in 0..10 {
            if !is_process_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("process {pid} is still alive after SIGKILL");
    }

    #[cfg(windows)]
    {
        warn!("Terminating process {}", pid);
        if pid == 0 {
            bail!("invalid Windows process ID 0");
        }
        let targets = terminate_windows_process_tree(pid, expected_identity)?;
        for _ in 0..20 {
            let mut all_signaled = true;
            for (_, handle) in &targets {
                all_signaled &= windows_handle_is_signaled(handle)?;
            }
            if all_signaled {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let mut remaining = Vec::new();
        for (pid, handle) in &targets {
            if !windows_handle_is_signaled(handle)? {
                remaining.push(*pid);
            }
        }
        bail!("processes {remaining:?} are still alive after native termination");
    }
}

pub(super) async fn terminate_process(pid: u32) -> Result<()> {
    terminate_process_inner(pid, None).await
}

pub(super) async fn terminate_process_if_identity(
    pid: u32,
    expected_identity: &ProcessIdentity,
) -> Result<()> {
    terminate_process_inner(pid, Some(expected_identity)).await
}

#[cfg(test)]
mod tests {
    use super::{
        checked_unix_pid, is_process_alive, parse_linux_process_stat, process_identity,
        terminate_process_if_identity, termination_order,
    };
    use std::hint::black_box;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const PROCESS_HELPER_MODE: &str = "CLASH_VERGE_PROCESS_TEST_MODE";
    const PROCESS_HELPER_PID_FILE: &str = "CLASH_VERGE_PROCESS_TEST_PID_FILE";
    const PROCESS_HELPER_TEST: &str = "core::process::tests::subprocess_helper";

    fn spawn_helper(mode: &str, pid_file: Option<&std::path::Path>) -> anyhow::Result<Child> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([PROCESS_HELPER_TEST, "--exact", "--ignored"])
            .env(PROCESS_HELPER_MODE, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(pid_file) = pid_file {
            command.env(PROCESS_HELPER_PID_FILE, pid_file);
        }
        Ok(command.spawn()?)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn legacy_cli_process_alive(pid: u32) -> anyhow::Result<bool> {
        #[cfg(unix)]
        {
            let output = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()?;
            Ok(output.status.success()
                && !String::from_utf8_lossy(&output.stdout)
                    .trim_start()
                    .starts_with('Z'))
        }

        #[cfg(windows)]
        {
            let filter = format!("PID eq {pid}");
            let output = Command::new("tasklist")
                .args(["/FI", &filter, "/FO", "CSV", "/NH"])
                .output()?;
            Ok(output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
        }
    }

    fn timed_probe(mut probe: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<u128> {
        let started = Instant::now();
        assert!(black_box(probe()?));
        Ok(started.elapsed().as_nanos())
    }

    fn percentile(samples: &mut [u128], percent: usize) -> u128 {
        samples.sort_unstable();
        let index = (samples.len() * percent).div_ceil(100).saturating_sub(1);
        samples[index]
    }

    #[test]
    #[ignore]
    fn subprocess_helper() {
        match std::env::var(PROCESS_HELPER_MODE).as_deref() {
            Ok("exit") => {}
            Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
            Ok("tree") => {
                let pid_file = std::env::var_os(PROCESS_HELPER_PID_FILE)
                    .expect("tree helper requires a PID file");
                let mut child = spawn_helper("sleep", None).expect("failed to spawn leaf helper");
                std::fs::write(pid_file, child.id().to_string())
                    .expect("failed to publish leaf PID");
                let _ = child.wait();
            }
            Ok("tree-root") => {
                let pid_file = std::env::var_os(PROCESS_HELPER_PID_FILE)
                    .expect("tree root helper requires a PID file");
                let mut child = spawn_helper("tree", Some(std::path::Path::new(&pid_file)))
                    .expect("failed to spawn branch helper");
                let _ = child.wait();
            }
            mode => panic!("unexpected process helper mode: {mode:?}"),
        }
    }

    #[test]
    fn unix_pid_validation_rejects_process_group_values() {
        assert_eq!(checked_unix_pid(0), None);
        assert_eq!(checked_unix_pid(i32::MAX as u32), Some(i32::MAX));
        assert_eq!(checked_unix_pid(i32::MAX as u32 + 1), None);
    }

    #[test]
    fn linux_stat_parser_handles_spaces_and_parentheses_in_command() -> anyhow::Result<()> {
        let stat = "42 (name with ) parens) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 98765";
        assert_eq!(parse_linux_process_stat(stat)?, ('S', 98765));
        Ok(())
    }

    #[test]
    fn process_tree_termination_orders_descendants_before_parent() {
        let relations = [(20, 10), (30, 20), (40, 10), (10, 1)];
        let order = termination_order(10, &relations);
        let position = |pid| {
            order
                .iter()
                .position(|candidate| *candidate == pid)
                .unwrap()
        };
        assert!(position(30) < position(20));
        assert!(position(20) < position(10));
        assert!(position(40) < position(10));
    }

    #[test]
    fn process_tree_termination_tolerates_cycles() {
        let relations = [(20, 10), (10, 20)];
        let order = termination_order(10, &relations);
        assert_eq!(order.len(), 2);
        assert_eq!(order.last(), Some(&10));
    }

    #[test]
    fn current_process_is_alive_and_has_an_identity() -> anyhow::Result<()> {
        let pid = std::process::id();
        assert!(is_process_alive(pid));
        assert!(process_identity(pid)?.is_some());
        Ok(())
    }

    #[test]
    fn exited_unreaped_process_is_not_alive() -> anyhow::Result<()> {
        let mut child = spawn_helper("exit", None)?;
        let pid = child.id();
        assert!(wait_until(|| !is_process_alive(pid)));
        assert!(process_identity(pid)?.is_none());
        child.wait()?;
        Ok(())
    }

    #[test]
    fn mismatched_identity_does_not_terminate_process() -> anyhow::Result<()> {
        let mut child = spawn_helper("sleep", None)?;
        let pid = child.id();
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(wait_until(|| is_process_alive(pid)), "helper did not start");
            let mut wrong_identity = process_identity(pid)?.expect("helper identity");
            wrong_identity.started_at = wrong_identity.started_at.wrapping_add(1);
            let error = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(terminate_process_if_identity(pid, &wrong_identity))
                .expect_err("mismatched identity must be rejected");
            anyhow::ensure!(
                error.to_string().contains("identity changed"),
                "unexpected rejection: {error:#}"
            );
            anyhow::ensure!(is_process_alive(pid), "mismatched identity killed helper");
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        result
    }

    #[test]
    #[ignore]
    fn benchmark_native_process_probe_against_legacy_cli() -> anyhow::Result<()> {
        const WARMUP_ITERATIONS: usize = 3;
        const ITERATIONS: usize = 30;
        let pid = std::process::id();

        for _ in 0..WARMUP_ITERATIONS {
            assert!(is_process_alive(pid));
            assert!(legacy_cli_process_alive(pid)?);
        }

        let mut native = Vec::with_capacity(ITERATIONS);
        let mut legacy = Vec::with_capacity(ITERATIONS);
        for iteration in 0..ITERATIONS {
            if iteration % 2 == 0 {
                native.push(timed_probe(|| Ok(is_process_alive(pid)))?);
                legacy.push(timed_probe(|| legacy_cli_process_alive(pid))?);
            } else {
                legacy.push(timed_probe(|| legacy_cli_process_alive(pid))?);
                native.push(timed_probe(|| Ok(is_process_alive(pid)))?);
            }
        }

        let native_median = percentile(&mut native, 50);
        let native_p95 = percentile(&mut native, 95);
        let legacy_median = percentile(&mut legacy, 50);
        let legacy_p95 = percentile(&mut legacy, 95);
        anyhow::ensure!(
            native_median < legacy_median,
            "native median {native_median}ns did not beat legacy CLI median {legacy_median}ns"
        );
        let report = serde_json::json!({
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "git_sha": std::env::var("GITHUB_SHA").ok(),
            "runner": {
                "arch": std::env::var("RUNNER_ARCH").ok(),
                "image_os": std::env::var("ImageOS").ok(),
            },
            "iterations": ITERATIONS,
            "native": {
                "median_ns": native_median,
                "p95_ns": native_p95,
                "samples_ns": native,
            },
            "legacy_cli": {
                "median_ns": legacy_median,
                "p95_ns": legacy_p95,
                "samples_ns": legacy,
            },
            "median_speedup": legacy_median as f64 / native_median.max(1) as f64,
        });
        let report = serde_json::to_string_pretty(&report)?;
        println!("PROCESS_PROBE_BENCHMARK={report}");
        if let Some(output) = std::env::var_os("CLASH_VERGE_PROCESS_BENCH_OUTPUT") {
            std::fs::write(output, format!("{report}\n"))?;
        }
        Ok(())
    }

    #[test]
    fn production_process_control_does_not_spawn_system_cli() {
        let production = include_str!("process.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for command in ["tasklist", "taskkill", "Command::new(\"ps\""] {
            assert!(
                !production.contains(command),
                "found forbidden CLI: {command}"
            );
        }
    }

    #[test]
    fn production_installers_do_not_spawn_unbounded_system_cli() {
        let install = include_str!("../bin/install_service.rs");
        let uninstall = include_str!("../bin/uninstall_service.rs");
        let shared = include_str!("../bin/shared/mod.rs");

        for source in [install, uninstall, shared] {
            assert!(!source.contains("systemctl"));
        }
        for source in [install, uninstall] {
            assert!(!source.contains("std::process::Command"));
        }
        assert!(
            shared.contains("run_command_output_with_timeout(cmd, args, debug, COMMAND_TIMEOUT)")
        );
        assert!(shared.contains("tokio::time::timeout(timeout"));
        assert!(shared.contains("tokio::process::Command::new(cmd)"));
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_termination_stops_descendant_tree() -> anyhow::Result<()> {
        use super::terminate_process;

        let pid_file = std::env::temp_dir().join(format!(
            "clash-verge-process-tree-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let mut root = spawn_helper("tree-root", Some(&pid_file))?;
        let root_pid = root.id();
        let result = (|| -> anyhow::Result<()> {
            assert!(wait_until(|| pid_file.exists()));
            let leaf_pid: u32 = std::fs::read_to_string(&pid_file)?.parse()?;
            assert!(is_process_alive(root_pid));
            assert!(is_process_alive(leaf_pid));

            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(terminate_process(root_pid))?;
            assert!(wait_until(|| !is_process_alive(root_pid)));
            assert!(wait_until(|| !is_process_alive(leaf_pid)));
            Ok(())
        })();

        if is_process_alive(root_pid) {
            let _ = root.kill();
        }
        let _ = root.wait();
        let _ = std::fs::remove_file(pid_file);
        result
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_termination_stops_descendant_after_root_exit() -> anyhow::Result<()> {
        use super::{
            open_windows_termination_handle, terminate_windows_process_tree_with_root_handle,
            windows_handle_is_signaled,
        };
        use windows_sys::Win32::System::Threading::TerminateProcess;

        let pid_file = std::env::temp_dir().join(format!(
            "clash-verge-exited-process-tree-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let mut root = spawn_helper("tree", Some(&pid_file))?;
        let root_pid = root.id();
        let result = (|| -> anyhow::Result<()> {
            assert!(wait_until(|| pid_file.exists()));
            let leaf_pid: u32 = std::fs::read_to_string(&pid_file)?.parse()?;
            let root_handle = open_windows_termination_handle(root_pid)?.expect("root handle");
            anyhow::ensure!(
                unsafe { TerminateProcess(root_handle.0, 1) } != 0,
                "failed to terminate root fixture: {}",
                std::io::Error::last_os_error()
            );
            assert!(wait_until(
                || windows_handle_is_signaled(&root_handle).unwrap_or(false)
            ));
            assert!(is_process_alive(leaf_pid));

            let targets =
                terminate_windows_process_tree_with_root_handle(root_pid, root_handle, None)?;
            assert!(wait_until(|| targets.iter().all(|(_, handle)| {
                windows_handle_is_signaled(handle).unwrap_or(false)
            })));
            assert!(wait_until(|| !is_process_alive(leaf_pid)));
            Ok(())
        })();

        if is_process_alive(root_pid) {
            let _ = root.kill();
        }
        let _ = root.wait();
        let _ = std::fs::remove_file(pid_file);
        result
    }
}
