//! GPU inhibition detection and gating.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use tokio::sync::watch;

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
pub const INHIBIT_FLAG_FILE: &str = "gpu-inhibit";

static SHUTDOWN: RwLock<Option<watch::Receiver<bool>>> = RwLock::new(None);

#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("interrupted")]
    Interrupted,
}

pub fn register_shutdown(rx: watch::Receiver<bool>) {
    if let Ok(mut lock) = SHUTDOWN.write() {
        *lock = Some(rx);
    }
}

pub fn is_shutting_down() -> bool {
    SHUTDOWN
        .read()
        .ok()
        .and_then(|l| l.as_ref().map(|rx| *rx.borrow()))
        .unwrap_or(false)
}

fn get_shutdown() -> Option<watch::Receiver<bool>> {
    SHUTDOWN.read().ok().and_then(|l| l.as_ref().cloned())
}

pub fn inhibit_flag_path(state_dir: &Path) -> PathBuf {
    state_dir.join(INHIBIT_FLAG_FILE)
}

pub fn now_iso8601() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

pub fn set_inhibit(state_dir: &Path, reason: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(
        inhibit_flag_path(state_dir),
        format!("{} {reason}\n", now_iso8601()),
    )
}

pub fn clear_inhibit(state_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(inhibit_flag_path(state_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn read_inhibit_flag(state_dir: &Path) -> Option<(String, String)> {
    let content = std::fs::read_to_string(inhibit_flag_path(state_dir)).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Some(("unknown".into(), "manual".into()));
    }
    let (ts, reason) = trimmed.split_once(' ').unwrap_or((trimmed, "manual"));
    Some((
        if ts.is_empty() {
            "unknown".into()
        } else {
            ts.into()
        },
        if reason.is_empty() {
            "manual".into()
        } else {
            reason.into()
        },
    ))
}

pub fn check_inhibit_command(cmd: &str) -> bool {
    if cmd.trim().is_empty() {
        return false;
    }
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(s) if s.code() == Some(126) || s.code() == Some(127) => {
            eprintln!("warning: inhibit command cannot be run: exit status {s}");
            false
        }
        Ok(s) => s.success(),
        Err(err) => {
            eprintln!("warning: failed to run inhibit command `{cmd}`: {err}");
            false
        }
    }
}

pub fn check_inhibit(state_dir: &Path, inhibit_command: Option<&str>) -> Option<String> {
    if let Some((_, reason)) = read_inhibit_flag(state_dir) {
        return Some(format!("flag file ({reason})"));
    }
    inhibit_command
        .filter(|cmd| check_inhibit_command(cmd))
        .map(|cmd| format!("command `{cmd}` exited 0"))
}

pub fn is_inhibited(state_dir: &Path, inhibit_command: Option<&str>) -> bool {
    check_inhibit(state_dir, inhibit_command).is_some()
}

pub async fn gate(
    state_dir: &Path,
    inhibit_command: Option<&str>,
    poll_interval: Duration,
) -> Result<(), GpuError> {
    loop {
        if is_shutting_down() {
            return Err(GpuError::Interrupted);
        }
        match check_inhibit(state_dir, inhibit_command) {
            Some(why) => {
                tracing::info!("run is waiting for GPU: {why}");
                if let Some(ref mut rx) = get_shutdown() {
                    tokio::select! {
                        () = tokio::time::sleep(poll_interval) => {}
                        _ = rx.wait_for(|&val| val) => return Err(GpuError::Interrupted),
                    }
                } else {
                    tokio::time::sleep(poll_interval).await;
                }
            }
            None => return Ok(()),
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

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn flag_present_means_inhibited() {
        let dir = tempdir().unwrap();
        set_inhibit(dir.path(), "manual").unwrap();
        assert!(is_inhibited(dir.path(), None));
    }

    #[test]
    fn flag_absent_means_clear() {
        assert!(!is_inhibited(tempdir().unwrap().path(), None));
    }

    #[test]
    fn command_exiting_zero_means_inhibited() {
        assert!(check_inhibit_command("exit 0"));
        assert!(is_inhibited(tempdir().unwrap().path(), Some("exit 0")));
    }

    #[test]
    fn command_exiting_one_means_clear() {
        assert!(!check_inhibit_command("exit 1"));
        assert!(!is_inhibited(tempdir().unwrap().path(), Some("exit 1")));
    }

    #[test]
    fn inhibit_command_cannot_run_means_clear() {
        assert!(!check_inhibit_command("nonexistent_binary_xyz_12345"));
        assert!(!is_inhibited(
            tempdir().unwrap().path(),
            Some("nonexistent_binary_xyz_12345")
        ));
    }

    #[tokio::test]
    async fn gate_waits_until_flag_removed() {
        let dir = tempdir().unwrap();
        set_inhibit(dir.path(), "test").unwrap();
        let path = dir.path().to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = clear_inhibit(&path);
        });
        gate(dir.path(), None, Duration::from_millis(5))
            .await
            .unwrap();
    }
}
