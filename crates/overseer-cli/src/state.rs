use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use overseer_core::Config;
use overseer_store::Store;

const DB_FILE_NAME: &str = "overseer.db";
const CONFIG_FILE_NAME: &str = "overseer.toml";

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

pub fn config_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CONFIG_FILE_NAME)
}

/// Write `text` to `overseer.toml` only if no such file exists. Returns
/// `false`, leaving the file untouched, when it already exists.
pub fn write_starter_config(state_dir: &Path, text: &str) -> Result<bool> {
    let path = config_path(state_dir);
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create {}", path.display()));
        }
    };
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

pub fn load_config(state_dir: &Path) -> Result<Config> {
    let path = config_path(state_dir);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no overseer config at {}; run `overseer init` first",
            path.display()
        )
    })?;
    Config::from_toml_str(&text).with_context(|| format!("invalid config {}", path.display()))
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

    #[test]
    fn starter_config_is_written_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let text = Config::starter_toml(&PathBuf::from("/work/project"));

        assert!(write_starter_config(dir.path(), &text).unwrap());
        let written = std::fs::read_to_string(config_path(dir.path())).unwrap();
        assert_eq!(written, text);

        std::fs::write(config_path(dir.path()), "# edited by hand\n").unwrap();
        assert!(!write_starter_config(dir.path(), &text).unwrap());
        let kept = std::fs::read_to_string(config_path(dir.path())).unwrap();
        assert_eq!(kept, "# edited by hand\n");
    }

    #[test]
    fn load_config_explains_how_to_fix_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_config(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("run `overseer init` first"),
            "{err:#}"
        );
    }
}
