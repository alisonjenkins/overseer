//! `ovs`: composable agent monitor commands.

use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ovs_collect_claude::{Collector, pid_alive};
use ovs_event::{Host, Timestamp};

/// One second keeps the live view current; re-statting ~1000 transcripts per poll costs little.
const DEFAULT_INTERVAL_MS: u64 = 1000;

#[derive(Debug, Parser)]
#[command(name = "ovs", version, about = "Composable agent monitor commands")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read one agent tool's local store and print events as NDJSON on stdout.
    #[command(subcommand)]
    Collect(CollectSource),
}

#[derive(Debug, Subcommand)]
enum CollectSource {
    /// Claude Code transcripts and live sessions.
    Claude(ClaudeArgs),
}

#[derive(Debug, Args)]
struct ClaudeArgs {
    /// Claude Code data directory [default: $HOME/.claude]
    #[arg(long, env = "OVS_CLAUDE_ROOT")]
    root: Option<PathBuf>,
    /// Cursor file [default: $XDG_STATE_HOME/ovs/collect-claude.json]
    #[arg(long, env = "OVS_CURSOR")]
    cursor: Option<PathBuf>,
    /// Host name stamped on every event [default: system hostname]
    #[arg(long, env = "OVS_HOST")]
    host: Option<String>,
    /// Milliseconds between polls.
    #[arg(long, default_value_t = DEFAULT_INTERVAL_MS)]
    interval_ms: u64,
    /// Poll once and exit.
    #[arg(long)]
    once: bool,
    /// Skip transcripts not seen before and last modified before this RFC 3339 time.
    #[arg(long)]
    since: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("OVS_LOG"))
        .with_writer(io::stderr)
        .init();
    match Cli::parse().command {
        Command::Collect(CollectSource::Claude(args)) => collect_claude(args),
    }
}

fn collect_claude(args: ClaudeArgs) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let root = match args.root {
        Some(root) => root,
        None => home
            .as_deref()
            .context("HOME is unset; pass --root")?
            .join(".claude"),
    };
    let cursor = match args.cursor {
        Some(cursor) => cursor,
        None => state_dir(home.as_deref())?.join("collect-claude.json"),
    };
    let host = Host::new(
        args.host
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned()),
    );
    let since = args
        .since
        .as_deref()
        .map(Timestamp::parse)
        .transpose()
        .context("--since is not an RFC 3339 time")?
        .map(Timestamp::to_system_time);

    let mut collector = Collector::open(root.clone(), host, cursor.clone(), since)
        .with_context(|| format!("cannot open cursor {}", cursor.display()))?;
    let mut out = io::stdout().lock();
    loop {
        let stats = collector
            .poll(&mut out, pid_alive)
            .with_context(|| format!("collecting from {}", root.display()))?;
        tracing::debug!(
            events = stats.events,
            parse_errors = stats.parse_errors,
            "poll finished"
        );
        if args.once {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(args.interval_ms));
    }
}

fn state_dir(home: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir).join("ovs"));
    }
    Ok(home
        .context("HOME is unset; pass --cursor")?
        .join(".local/state/ovs"))
}
