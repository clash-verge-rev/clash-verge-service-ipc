use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ProcessIdentity {
    pub(super) executable: String,
    pub(super) started_at: u64,
}

#[cfg(unix)]
fn checked_unix_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|pid| *pid > 0)
}

#[cfg(target_os = "linux")]
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

#[cfg(windows)]
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
        if windows_handle_is_signaled(&handle)? {
            Ok(None)
        } else {
            Ok(Some(windows_process_identity_details_from_handle(&handle)?))
        }
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
    use windows_sys::Win32::System::Threading::TerminateProcess;

    if let Some(expected_identity) = expected_identity {
        let current_identity = windows_process_identity_details_from_handle(&root_handle)?;
        if &current_identity != expected_identity {
            bail!("process {pid} identity changed before termination");
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
    terminate(pid, &root_handle)?;

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
    use super::is_process_alive;
    use std::hint::black_box;
    use std::process::Command;
    use std::time::Instant;

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
            "git_sha": std::env::var("CLASH_VERGE_PROCESS_BENCH_GIT_SHA").ok(),
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
}
