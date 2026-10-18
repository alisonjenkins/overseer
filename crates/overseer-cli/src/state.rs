use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use overseer_store::Store;

const DB_FILE_NAME: &str = "overseer.db";

pub fn resolve_state_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir),
        None => default_state_dir(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")),
    }
}

fn default_state_dir(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(xdg) = xdg_state_home.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("overseer"));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .context("cannot choose a default state directory: HOME is unset; pass --state-dir")?;
    Ok(PathBuf::from(home).join(".local/state/overseer"))
}

pub fn create_store(state_dir: &Path) -> Result<Store> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("failed to create state directory {}", state_dir.display()))?;
    let db = db_path(state_dir);
    Store::open(&db).with_context(|| format!("failed to open database {}", db.display()))
}

pub fn open_existing_store(state_dir: &Path) -> Result<Store> {
    let db = db_path(state_dir);
    if !db.is_file() {
        bail!(
            "no overseer database at {}; run `overseer init` first",
            db.display()
        );
    }
    Store::open(&db).with_context(|| format!("failed to open database {}", db.display()))
}

fn db_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DB_FILE_NAME)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn default_uses_xdg_state_home_when_set() {
        let dir = default_state_dir(Some("/xdg/state".into()), Some("/home/u".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/xdg/state/overseer"));
    }

    #[test]
    fn default_falls_back_to_home_local_state() {
        let dir = default_state_dir(None, Some("/home/u".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.local/state/overseer"));
    }

    #[test]
    fn empty_xdg_state_home_is_ignored() {
        let dir = default_state_dir(Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.local/state/overseer"));
    }

    #[test]
    fn no_xdg_and_no_home_is_an_error() {
        let err = default_state_dir(None, None).unwrap_err();
        assert!(err.to_string().contains("--state-dir"), "{err}");
    }
}
