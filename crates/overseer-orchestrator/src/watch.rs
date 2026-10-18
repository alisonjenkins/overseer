use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use notify::Watcher;
use overseer_core::TaskSource;
use tokio::sync::mpsc;

const QUIET_PERIOD: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("watch path does not exist: {0}")]
    PathNotFound(PathBuf),

    #[error("notify watch failed on {path}: {source}")]
    Notify {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },

    #[error("failed to initialize watcher: {0}")]
    Init(#[source] notify::Error),
}

pub struct SourceWatcher {
    _watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<()>,
    pending: bool,
}

impl std::fmt::Debug for SourceWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceWatcher").finish_non_exhaustive()
    }
}

pub fn start(sources: &[Box<dyn TaskSource>]) -> Result<SourceWatcher, WatchError> {
    let (tx, rx) = mpsc::channel(64);
    let mut watcher = notify::recommended_watcher(move |_res| {
        let _ = tx.try_send(());
    })
    .map_err(WatchError::Init)?;

    let mut watched = HashSet::new();
    for source in sources {
        for path in source.watch_paths() {
            if !path.exists() {
                return Err(WatchError::PathNotFound(path));
            }
            if watched.insert(path.clone()) {
                watcher
                    .watch(&path, notify::RecursiveMode::Recursive)
                    .map_err(|source| WatchError::Notify {
                        path: path.clone(),
                        source,
                    })?;
            }
        }
    }

    Ok(SourceWatcher {
        _watcher: watcher,
        rx,
        pending: false,
    })
}

impl SourceWatcher {
    pub fn start(sources: &[Box<dyn TaskSource>]) -> Result<Self, WatchError> {
        start(sources)
    }

    pub async fn next_change(&mut self) {
        if !self.pending {
            if self.rx.recv().await.is_none() {
                // A closed watcher stops producing changes, and the periodic rescan in the worker still runs.
                std::future::pending::<()>().await;
            }
            self.pending = true;
        }

        loop {
            tokio::select! {
                () = tokio::time::sleep(QUIET_PERIOD) => {
                    self.pending = false;
                    break;
                }
                res = self.rx.recv() => {
                    if res.is_none() {
                        self.pending = false;
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use std::path::PathBuf;
    use std::time::Duration;

    use overseer_core::{SourceError, SourcedTask, TaskSource};

    use super::*;

    struct FakeSource {
        paths: Vec<PathBuf>,
    }

    impl TaskSource for FakeSource {
        fn name(&self) -> &str {
            "fake"
        }

        fn watch_paths(&self) -> Vec<PathBuf> {
            self.paths.clone()
        }

        fn scan(&self) -> Result<Vec<SourcedTask>, SourceError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn watch_detects_file_write_in_temp_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source: Box<dyn TaskSource> = Box::new(FakeSource {
            paths: vec![temp_dir.path().to_path_buf()],
        });

        let mut watcher = start(&[source]).unwrap();

        let file_path = temp_dir.path().join("task.md");
        std::fs::write(&file_path, "test content").unwrap();

        let res = tokio::time::timeout(Duration::from_secs(10), watcher.next_change()).await;
        assert!(res.is_ok(), "next_change timed out waiting for file write");
    }

    #[tokio::test]
    async fn next_change_coalesces_writes_within_200ms() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source: Box<dyn TaskSource> = Box::new(FakeSource {
            paths: vec![temp_dir.path().to_path_buf()],
        });

        let mut watcher = start(&[source]).unwrap();

        let file_path = temp_dir.path().join("task.md");
        std::fs::write(&file_path, "write 1").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&file_path, "write 2").unwrap();

        let first = tokio::time::timeout(Duration::from_secs(10), watcher.next_change()).await;
        assert!(first.is_ok(), "first next_change timed out");

        let second = tokio::time::timeout(Duration::from_secs(3), watcher.next_change()).await;
        assert!(
            second.is_err(),
            "second next_change should have timed out due to debouncing"
        );
    }

    #[tokio::test]
    async fn start_returns_error_when_watch_path_missing() {
        let non_existent = PathBuf::from("/nonexistent/path/for/sure/12345");
        let source: Box<dyn TaskSource> = Box::new(FakeSource {
            paths: vec![non_existent.clone()],
        });

        let res = start(&[source]);
        match res {
            Err(WatchError::PathNotFound(path)) => {
                assert_eq!(path, non_existent);
            }
            other => panic!("expected PathNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn next_change_times_out_when_channel_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(tx);

        let watcher = notify::recommended_watcher(|_| {}).unwrap();
        let mut watcher = SourceWatcher {
            _watcher: watcher,
            rx,
            pending: false,
        };

        let res = tokio::time::timeout(Duration::from_millis(300), watcher.next_change()).await;
        assert!(
            res.is_err(),
            "next_change should time out when channel is closed"
        );
    }
}
