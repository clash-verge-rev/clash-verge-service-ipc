#![cfg(all(feature = "standalone", feature = "test"))]

//! Holds a runtime file open so staging's refusal path can be exercised where it actually bites.
//!
//! A running core keeps handles on every provider file it loaded. On Unix that is invisible —
//! unlinking a file with open handles simply works — but on Windows a held handle turns a
//! deletion into a pending one and makes the name unusable until every handle closes. Staging is
//! required to decline in that situation rather than leave the generation half-corrected, and
//! this stand-in is what makes the situation happen on demand.
//!
//! It holds the handle with a stricter share mode than the real core does, which is deliberate:
//! a test needs the block to be deterministic, not to reproduce mihomo's exact flags. What is
//! being tested is our refusal, not the core's file handling.
//!
//! Usage: `asset_lock_holder <file-to-hold> <ready-sentinel>`. The sentinel is created once the
//! handle is open, and the process exits when it is removed.

use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(target), Some(sentinel)) = (args.next(), args.next()) else {
        eprintln!("usage: asset_lock_holder <file-to-hold> <ready-sentinel>");
        std::process::exit(2);
    };
    let target = PathBuf::from(target);
    let sentinel = PathBuf::from(sentinel);

    let held = match open_exclusively(&target) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("failed to hold {target:?}: {error}");
            std::process::exit(1);
        }
    };

    if let Err(error) = std::fs::write(&sentinel, b"held") {
        eprintln!("failed to signal readiness: {error}");
        std::process::exit(1);
    }

    while sentinel.exists() {
        std::thread::sleep(Duration::from_millis(25));
    }
    drop(held);
}

#[cfg(windows)]
fn open_exclusively(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    // Readers allowed, deletion and renaming denied: exactly the condition that makes a
    // replacement fail rather than merely queue behind a pending delete.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

#[cfg(not(windows))]
fn open_exclusively(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    // Nothing a Unix process can do to a file blocks unlinking it. Tests reach the same refusal
    // by making the containing directory unwritable instead; this branch exists so the binary
    // builds everywhere the test suite does.
    std::fs::File::open(path)
}
