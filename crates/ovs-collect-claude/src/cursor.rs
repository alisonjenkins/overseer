use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::{FileState, LiveEntry};

/// Everything the collector needs to resume after a restart.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub files: BTreeMap<PathBuf, FileState>,
    #[serde(default)]
    pub live: BTreeMap<String, LiveEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum CursorError {
    #[error("cannot read cursor {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cursor {path} is not valid JSON; delete it to re-read every transcript")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot create cursor directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot encode cursor")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot write cursor {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot move {from} to {to}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl Cursor {
    pub fn load(path: &Path) -> Result<Self, CursorError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(CursorError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        serde_json::from_str(&text).map_err(|source| CursorError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), CursorError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|source| CursorError::CreateDir {
                path: dir.to_owned(),
                source,
            })?;
        }
        let text = serde_json::to_string(self).map_err(|source| CursorError::Encode { source })?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text).map_err(|source| CursorError::Write {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| CursorError::Rename {
            from: tmp,
            to: path.to_owned(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn missing_cursor_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cursor = Cursor::load(&dir.path().join("none.json")).unwrap();
        assert!(cursor.files.is_empty() && cursor.live.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/cursor.json");
        let mut cursor = Cursor::default();
        cursor.files.insert(
            PathBuf::from("/p/s1.jsonl"),
            FileState {
                offset: 42,
                started: true,
                ..FileState::default()
            },
        );
        cursor.save(&path).unwrap();
        let loaded = Cursor::load(&path).unwrap();
        assert_eq!(loaded.files[&PathBuf::from("/p/s1.jsonl")].offset, 42);
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn corrupt_cursor_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor.json");
        fs::write(&path, "{").unwrap();
        assert!(Cursor::load(&path).is_err());
    }
}
