use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn acquire(hub: &Path) -> Result<File> {
    let lock = hub.with_extension("integration.lock");
    if let Some(parent) = lock.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(lock)
        .context("opening Micro integration ownership lock")?;
    // The file remains open for the bridge's lifetime; other controller clients use the same flock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("another integration owns Codex Micro");
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::process::Command;

    #[test]
    fn ownership_is_exclusive_and_released_after_close() {
        let directory =
            std::env::temp_dir().join(format!("micro-ownership-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let hub = directory.join("microd.sock");
        let first = acquire(&hub).unwrap();
        assert!(acquire(&hub).is_err());
        #[cfg(target_os = "linux")]
        assert!(!Command::new("flock")
            .args(["--nonblock"])
            .arg(hub.with_extension("integration.lock"))
            .arg("true")
            .status()
            .unwrap()
            .success());
        drop(first);
        assert!(acquire(&hub).is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
