use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub fn acquire(_hub: &Path) -> Result<File> {
    // Every supported hub opens the first Codex Micro, so socket aliases cannot identify ownership.
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("Micro ownership requires XDG_RUNTIME_DIR or HOME")?;
    let directory = std::env::var_os("MICROD_CONTROLLER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("microd-controller"));
    acquire_lock(directory.join("ownership.lock"))
}

fn acquire_lock(lock: PathBuf) -> Result<File> {
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
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("another integration owns Codex Micro");
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn ownership_probe() {
        let Ok(hub) = std::env::var("MICROD_TEST_OWNERSHIP_HUB") else {
            return;
        };
        let blocked = std::env::var("MICROD_TEST_OWNERSHIP_BLOCKED").unwrap() == "true";
        assert_eq!(acquire(Path::new(&hub)).is_err(), blocked);
    }

    #[test]
    fn socket_aliases_and_daemon_directory_replacement_keep_one_owner() {
        let directory =
            std::env::temp_dir().join(format!("micro-ownership-{}", std::process::id()));
        let daemon_directory = directory.join("microd");
        std::fs::create_dir_all(&daemon_directory).unwrap();
        let hub = daemon_directory.join("microd.sock");
        let alias = directory.join("alias.sock");
        std::os::unix::fs::symlink(&hub, &alias).unwrap();
        let lock = directory.join("microd-controller/ownership.lock");
        let probe = |path: &Path, blocked: bool| {
            assert!(Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "ownership::tests::ownership_probe"])
                .env_remove("MICROD_CONTROLLER_DIR")
                .env("XDG_RUNTIME_DIR", &directory)
                .env("MICROD_TEST_OWNERSHIP_HUB", path)
                .env("MICROD_TEST_OWNERSHIP_BLOCKED", blocked.to_string())
                .status()
                .unwrap()
                .success());
        };
        for initial_hub in [&hub, &alias] {
            probe(initial_hub, false);
            let first = acquire_lock(lock.clone()).unwrap();
            probe(&hub, true);
            probe(&alias, true);
            std::fs::remove_dir_all(&daemon_directory).unwrap();
            std::fs::create_dir(&daemon_directory).unwrap();
            probe(&alias, true);
            assert!(Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "ownership::tests::ownership_probe"])
                .env("XDG_RUNTIME_DIR", &directory)
                .env(
                    "MICROD_CONTROLLER_DIR",
                    directory.join("synthetic-controller")
                )
                .env("MICROD_TEST_OWNERSHIP_HUB", &alias)
                .env("MICROD_TEST_OWNERSHIP_BLOCKED", "false")
                .status()
                .unwrap()
                .success());
            probe(&alias, true);
            #[cfg(target_os = "linux")]
            assert!(!Command::new("flock")
                .arg("--nonblock")
                .arg(&lock)
                .arg("true")
                .status()
                .unwrap()
                .success());
            drop(first);
            probe(&alias, false);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
