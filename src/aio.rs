//! AIO's external lock is advisory, not a bidirectional mutex. Its check and
//! creation of backup-is-running are separate operations; always double-check.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use tokio::time::{Instant, sleep};

use crate::{config::AioBorgLockConfig, error::Error, shutdown::Shutdown};

pub struct AioLockGuard {
    path: PathBuf,
    identity: (u64, u64),
    released: bool,
    // Retain the original inode so a removed/recreated lock cannot reuse it.
    file: fs::File,
}

impl AioLockGuard {
    pub fn try_acquire(path: &Path) -> io::Result<Self> {
        // Never remove an existing lock, including malformed or unknown locks.
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let metadata = file.metadata()?;
        let mut guard = Self {
            path: path.to_owned(),
            identity: (metadata.dev(), metadata.ino()),
            released: false,
            file,
        };
        // Construct the guard before fallible writes so errors also remove our lock.
        let metadata = serde_json::json!({
            "owner": "nerd-backup", "pid": std::process::id(),
            "created_at": humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
        });
        guard.file.write_all(metadata.to_string().as_bytes())?;
        guard.file.sync_all()?;
        tracing::info!(lockfile = %path.display(), "Acquired external AIO lock");
        Ok(guard)
    }

    pub fn release(&mut self) -> io::Result<()> {
        if self.released {
            return Ok(());
        }
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if (metadata.dev(), metadata.ino()) == self.identity => {
                fs::remove_file(&self.path)?
            }
            Ok(_) => {
                return Err(io::Error::other(
                    "AIO lock was replaced; refusing to remove another owner's lock",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.released = true;
        tracing::info!(lockfile = %self.path.display(), "Released external AIO lock");
        Ok(())
    }
}

impl Drop for AioLockGuard {
    fn drop(&mut self) {
        // RAII covers ordinary returns, cancellation and unwinding. SIGKILL,
        // kernel panic, host crash and power loss do not run destructors.
        if let Err(error) = self.release() {
            tracing::warn!(lockfile = %self.path.display(), %error, "Failed to release external AIO lock; manual inspection required");
        }
    }
}

fn exists(path: &Path) -> io::Result<bool> {
    // Treat dangling symlinks as blockers too, and fail closed on permissions/I/O
    // errors rather than interpreting inaccessible signals as absent.
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // ENOENT also means a missing volume mount or parent directory.
            // Only an absent leaf in an accessible directory means AIO is idle.
            require_directory(path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "AIO signal has no parent directory",
                )
            })?)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn require_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Cannot access AIO signal directory {}: {error}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!(
                "AIO signal directory is not a directory: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

pub fn signal_path(mountpoint: &str, relative_path: &Path) -> io::Result<PathBuf> {
    let mountpoint = Path::new(mountpoint);
    if !mountpoint.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Docker must provide an absolute mountpoint for an AIO volume",
        ));
    }
    require_directory(mountpoint)?;
    Ok(mountpoint.join(relative_path))
}

pub async fn acquire(
    volume: &str,
    lock_path: &Path,
    running_path: &Path,
    config: &AioBorgLockConfig,
    shutdown: &Shutdown,
) -> Result<AioLockGuard, Error> {
    acquire_with_check(volume, lock_path, running_path, config, shutdown, exists).await
}

async fn acquire_with_check(
    volume: &str,
    lock_path: &Path,
    running_path: &Path,
    config: &AioBorgLockConfig,
    shutdown: &Shutdown,
    mut running_exists: impl FnMut(&Path) -> io::Result<bool>,
) -> Result<AioLockGuard, Error> {
    let started = Instant::now();
    let mut next_progress = started;
    let mut last_blocker = "aio-lockfile";
    loop {
        if shutdown.is_requested() {
            return Err(Error::Cancelled);
        }
        let waited = started.elapsed();
        if waited >= config.wait_timeout {
            tracing::error!(volume, blocker = last_blocker, waited = %humantime::format_duration(waited), "Timed out waiting for AIO");
            return Err(Error::AioTimeout {
                volume: volume.to_owned(),
                blocker: last_blocker,
                waited,
            });
        }

        let running = running_exists(running_path)?;
        if running {
            last_blocker = if exists(lock_path)? {
                "aio-lockfile and backup-is-running"
            } else {
                "backup-is-running"
            };
        } else {
            match AioLockGuard::try_acquire(lock_path) {
                Ok(mut guard) => {
                    if !running_exists(running_path)? {
                        if shutdown.is_requested() {
                            return Err(Error::Cancelled);
                        }
                        return Ok(guard);
                    }
                    tracing::warn!(
                        volume,
                        "AIO backup-is-running appeared after acquiring the lock; releasing and retrying"
                    );
                    guard.release()?;
                    last_blocker = "backup-is-running";
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_blocker = "aio-lockfile"
                }
                Err(error) => return Err(error.into()),
            }
        }
        if Instant::now() >= next_progress {
            tracing::info!(volume, blocker = last_blocker, waited = %humantime::format_duration(started.elapsed()), "Waiting for AIO consistency signals");
            next_progress = Instant::now() + Duration::from_secs(60);
        }
        let remaining = config.wait_timeout.saturating_sub(started.elapsed());
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err(Error::Cancelled),
            _ = sleep(config.wait_interval.min(remaining)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> AioBorgLockConfig {
        AioBorgLockConfig {
            lockfile: "aio-lockfile".into(),
            running_marker_volume: "dump".into(),
            running_marker_path: "backup-is-running".into(),
            wait_interval: Duration::from_secs(30),
            wait_timeout: Duration::from_secs(65),
        }
    }

    #[test]
    fn aio_mountpoints_must_be_absolute_accessible_directories() {
        let dir = tempfile::tempdir().unwrap();
        let marker = Path::new("backup-is-running");
        assert_eq!(
            signal_path(dir.path().to_str().unwrap(), marker).unwrap(),
            dir.path().join(marker)
        );
        for mountpoint in ["", "relative-volume"] {
            assert!(signal_path(mountpoint, marker).is_err());
        }
        let missing = dir.path().join("missing");
        assert!(signal_path(missing.to_str().unwrap(), marker).is_err());
        fs::write(&missing, "not a directory").unwrap();
        assert!(signal_path(missing.to_str().unwrap(), marker).is_err());
    }
    #[test]
    fn atomic_create_and_drop_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aio-lockfile");
        let guard = AioLockGuard::try_acquire(&path).unwrap();
        assert!(path.exists());
        assert_eq!(
            AioLockGuard::try_acquire(&path).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(guard);
        assert!(!path.exists());
    }
    #[test]
    fn unknown_lock_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aio-lockfile");
        fs::write(&path, "unknown or malformed").unwrap();
        assert_eq!(
            AioLockGuard::try_acquire(&path).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "unknown or malformed");
    }
    #[test]
    fn replacement_lock_is_not_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aio-lockfile");
        let guard = AioLockGuard::try_acquire(&path).unwrap();
        // Keep the inode allocated so the replacement cannot reuse it.
        let _original = fs::File::open(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "other owner").unwrap();
        drop(guard);
        assert_eq!(fs::read_to_string(path).unwrap(), "other owner");
    }
    #[test]
    fn absent_destructors_leave_a_lock_for_manual_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aio-lockfile");
        // Simulates the missing destructor after SIGKILL or host failure. No
        // automatic stale-lock removal is attempted even for our own metadata.
        std::mem::forget(AioLockGuard::try_acquire(&path).unwrap());
        assert!(path.exists());
        assert!(AioLockGuard::try_acquire(&path).is_err());
    }
    #[tokio::test(start_paused = true)]
    async fn timeout_reports_lock_running_marker_or_both_without_deleting_them() {
        for (locked, running, blocker) in [
            (true, false, "aio-lockfile"),
            (false, true, "backup-is-running"),
            (true, true, "aio-lockfile and backup-is-running"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let lock = dir.path().join("aio-lockfile");
            let marker = dir.path().join("backup-is-running");
            if locked {
                fs::write(&lock, "unknown").unwrap();
            }
            if running {
                fs::write(&marker, "").unwrap();
            }
            let error = acquire("custom-borg", &lock, &marker, &config(), &Shutdown::new())
                .await
                .err()
                .unwrap();
            match &error {
                Error::AioTimeout {
                    volume,
                    blocker: actual,
                    waited,
                } => {
                    assert_eq!(volume, "custom-borg");
                    assert_eq!(*actual, blocker);
                    assert_eq!(*waited, Duration::from_secs(65));
                }
                _ => panic!("{error}"),
            }
            assert!(error.to_string().contains(blocker));
            assert_eq!(lock.exists(), locked);
            assert_eq!(marker.exists(), running);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn waits_for_running_marker_to_disappear() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("aio-lockfile");
        let marker = dir.path().join("backup-is-running");
        fs::write(&marker, "").unwrap();
        let started = Instant::now();
        let guard = acquire_with_check(
            "borg",
            &lock,
            &marker,
            &config(),
            &Shutdown::new(),
            |path| {
                if started.elapsed() >= Duration::from_secs(30) && path.exists() {
                    fs::remove_file(path)?;
                }
                exists(path)
            },
        )
        .await
        .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        drop(guard);
    }
    #[tokio::test(start_paused = true)]
    async fn double_check_releases_and_retries_when_marker_appears() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("aio-lockfile");
        let marker = dir.path().join("backup-is-running");
        let mut checks = 0;
        let started = Instant::now();
        let guard = acquire_with_check(
            "borg",
            &lock,
            &marker,
            &config(),
            &Shutdown::new(),
            |path| {
                checks += 1;
                match checks {
                    1 => assert!(!lock.exists()),
                    2 => {
                        assert!(lock.exists());
                        fs::write(path, "")?;
                    }
                    3 => {
                        assert!(
                            !lock.exists(),
                            "racing acquisition must release before retry"
                        );
                        fs::remove_file(path)?;
                    }
                    _ => {}
                }
                exists(path)
            },
        )
        .await
        .unwrap();
        assert_eq!(checks, 4);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        drop(guard);
        assert!(!lock.exists());
    }
    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_wait_without_removing_unknown_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("aio-lockfile");
        let marker = dir.path().join("backup-is-running");
        fs::write(&lock, "unknown").unwrap();
        let shutdown = Shutdown::new();
        let cancel = shutdown.clone();
        let request = async {
            sleep(Duration::from_secs(5)).await;
            cancel.request();
        };
        let config = config();
        let (result, _) =
            tokio::join!(acquire("borg", &lock, &marker, &config, &shutdown), request);
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(fs::read_to_string(lock).unwrap(), "unknown");
    }
    #[tokio::test]
    async fn second_check_error_releases_owned_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("aio-lockfile");
        let mut checks = 0;
        let result = acquire_with_check(
            "borg",
            &lock,
            &dir.path().join("marker"),
            &config(),
            &Shutdown::new(),
            |_| {
                checks += 1;
                if checks == 2 {
                    Err(io::Error::other("failed marker check"))
                } else {
                    Ok(false)
                }
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn missing_running_marker_directory_does_not_mean_aio_is_idle() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("aio-lockfile");
        let marker = dir.path().join("unmounted-dump/backup-is-running");
        let result = acquire("borg", &lock, &marker, &config(), &Shutdown::new()).await;
        assert!(
            result.is_err(),
            "an unavailable marker directory must fail closed"
        );
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn disappearing_marker_directory_releases_lock_and_fails() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("aio-lockfile");
        let dump = dir.path().join("dump");
        fs::create_dir(&dump).unwrap();
        let marker = dump.join("backup-is-running");
        let mut checks = 0;
        let result = acquire_with_check(
            "borg",
            &lock,
            &marker,
            &config(),
            &Shutdown::new(),
            |path| {
                checks += 1;
                if checks == 2 {
                    fs::remove_dir(&dump)?;
                }
                exists(path)
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!lock.exists());
    }
}
