mod backend;
mod run;
mod watch;

use std::io::IsTerminal;

use clap::{ArgAction, Parser, Subcommand};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Share the clipboard between your own machines.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// More log output on stderr, one level per -v: warn, info, debug, trace. `run` starts at info
    /// (it logs its node id and every clip sent or received), other commands at warn. If
    /// `RUST_LOG` is set, it is used instead.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

// Next subcommands: `pair` in phase 2, `status` in phase 3.
#[derive(Debug, Subcommand)]
enum Command {
    /// Sync the clipboard with the peers in the config file until Ctrl+C or SIGTERM.
    ///
    /// Each text copied here is sent to every connected peer, and text copied on a peer is written
    /// to this clipboard. Peers that are offline miss what is copied meanwhile. Logs show this
    /// node's id (peers need it in their config), connections, and each clip's size and short
    /// hash, never its content. Sensitive content is not filtered yet: passwords copied from a
    /// password manager are synced too.
    Run(run::Args),
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
    let default_level = match cli.command {
        Command::Run(_) => LevelFilter::INFO,
        Command::Watch(_) => LevelFilter::WARN,
    };
    init_logging(default_level, cli.verbose);
    match cli.command {
        Command::Run(args) => run::run(args).await,
        Command::Watch(args) => watch::run(args).await,
    }
}

/// Logs go to stderr so they never mix with a command's output on stdout.
fn init_logging(default_level: LevelFilter, verbose: u8) {
    const LEVELS: [LevelFilter; 4] = [
        LevelFilter::WARN,
        LevelFilter::INFO,
        LevelFilter::DEBUG,
        LevelFilter::TRACE,
    ];
    let start = LEVELS
        .iter()
        .position(|&level| level == default_level)
        .unwrap_or(0);
    let level = LEVELS[(start + usize::from(verbose)).min(LEVELS.len() - 1)];
    let filter = EnvFilter::builder()
        .with_default_directive(level.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}
