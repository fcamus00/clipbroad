mod backend;
mod watch;

use std::io::IsTerminal;

use clap::{ArgAction, Parser, Subcommand};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Share the clipboard between your own machines.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// More log output on stderr: -v info, -vv debug, -vvv trace (default: warnings and errors).
    /// If `RUST_LOG` is set, it is used instead.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

// Next subcommands: `run` (the sync daemon) and `pair` in phase 2, `status` in phase 3.
#[derive(Debug, Subcommand)]
enum Command {
    /// Print every change of the local clipboard, one line per change, until Ctrl+C.
    ///
    /// Each line shows the time (UTC), the formats the clipboard offers and a preview of its text
    /// (first 80 characters, with newlines and control characters escaped). Nothing is sent to
    /// other machines.
    Watch(watch::Args),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match cli.command {
        Command::Watch(args) => watch::run(args).await,
    }
}

/// Logs go to stderr so they never mix with a command's output on stdout.
fn init_logging(verbose: u8) {
    let default_level = match verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    let filter = EnvFilter::builder()
        .with_default_directive(default_level.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}
