//! Single-worker mutual exclusion via an exclusive non-blocking file lock.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WorkerLockError {
    #[error("another worker is already running{}", match pid {
        Some(pid) => format!(" (pid {pid})"),
        None => String::new(),
    })]
    AlreadyLocked { pid: Option<u32> },

    #[error("failed to open lock file at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to lock {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write pid to lock file at {path}")]
    WritePid {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// An acquired exclusive lock on `<state-dir>/worker.lock`.
/// The lock is released automatically when this struct is dropped or when
/// the process terminates.
#[derive(Debug)]
pub struct WorkerLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl WorkerLock {
    pub fn acquire(state_dir: &Path) -> Result<Self, WorkerLockError> {
        let path = state_dir.join("worker.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| WorkerLockError::Open {
                path: path.clone(),
                source,
            })?;

        match file.try_lock() {
            Ok(()) => {
                let pid_str = format!("{}\n", std::process::id());
                file.set_len(0)
                    .map_err(|source| WorkerLockError::WritePid {
                        path: path.clone(),
                        source,
                    })?;
                file.seek(SeekFrom::Start(0))
                    .map_err(|source| WorkerLockError::WritePid {
                        path: path.clone(),
                        source,
                    })?;
                file.write_all(pid_str.as_bytes())
                    .map_err(|source| WorkerLockError::WritePid {
                        path: path.clone(),
                        source,
                    })?;
                file.flush().map_err(|source| WorkerLockError::WritePid {
                    path: path.clone(),
                    source,
                })?;
                Ok(Self { _file: file, path })
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut content = String::new();
                let _ = std::fs::read_to_string(&path).map(|c| content = c);
                let pid = content.trim().parse::<u32>().ok();
                Err(WorkerLockError::AlreadyLocked { pid })
            }
            Err(std::fs::TryLockError::Error(source)) => {
                Err(WorkerLockError::Lock { path, source })
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkerLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

/// Check if another worker holds the lock, returning its PID if so.
pub fn check_worker_status(state_dir: &Path) -> Result<Option<u32>, std::io::Error> {
    let path = state_dir.join("worker.lock");
    if !path.exists() {
        return Ok(None);
    }
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(None)
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut content = String::new();
            let _ = std::fs::read_to_string(&path).map(|c| content = c);
            let pid = content.trim().parse::<u32>().ok();
            Ok(pid)
        }
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn single_worker_lock_and_release() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path();

        assert_eq!(check_worker_status(state).unwrap(), None);

        let lock1 = WorkerLock::acquire(state).unwrap();
        assert!(state.join("worker.lock").is_file());

        // Status shows active worker with our PID
        assert_eq!(
            check_worker_status(state).unwrap(),
            Some(std::process::id())
        );

        // Second lock attempt fails with AlreadyLocked
        let err = WorkerLock::acquire(state).unwrap_err();
        assert!(matches!(
            err,
            WorkerLockError::AlreadyLocked { pid: Some(p) } if p == std::process::id()
        ));

        // Release lock
        drop(lock1);

        // Status shows worker is no longer running
        assert_eq!(check_worker_status(state).unwrap(), None);

        // Can acquire again
        let lock2 = WorkerLock::acquire(state).unwrap();
        drop(lock2);
    }
}
