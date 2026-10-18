use std::path::PathBuf;

/// A task found by an external source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcedTask {
    pub external_id: String,
    pub title: String,
    pub body: String,
    pub profile: Option<String>,
    pub ready: bool,
    pub done: bool,
}

/// An external source of tasks, such as speckit specifications.
pub trait TaskSource: Send + Sync {
    fn name(&self) -> &str;
    fn watch_paths(&self) -> Vec<PathBuf>;
    fn scan(&self) -> Result<Vec<SourcedTask>, SourceError>;
}

/// Errors produced when scanning a task source.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
}
