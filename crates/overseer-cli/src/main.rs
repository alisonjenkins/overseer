mod cli;
mod commands;
mod state;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    init_tracing();
    let cli = cli::Cli::parse();
    match commands::run(cli.state_dir, cli.command) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("overseer: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("OVERSEER_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
