mod backend;
mod control;
mod menu;
mod mesh;
mod pair;
mod remove;
mod reset;
mod run;
mod service;
mod status;
mod tray;
mod update;
mod watch;

use std::{
    env,
    io::{self, IsTerminal},
};

use clap::{ArgAction, Parser, Subcommand};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{
    filter::{ParseError, Targets},
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

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

#[derive(Debug, Subcommand)]
enum Command {
    /// Pair with another machine, so that `clipbroad run` syncs with it and with its mesh.
    ///
    /// Run `clipbroad pair` on one machine: it shows a ticket and waits. Then run
    /// `clipbroad pair <TICKET>` on the other one. Both add each other to their members file
    /// (`members.toml` beside the identity file). The ticket works once and for 5 minutes.
    ///
    /// To add a machine to a mesh, pair it with any one member: the members tell each other about
    /// it, and it syncs with all of them once they meet. If `clipbroad run` is running, it does
    /// the pairing and syncs with the new machine right away, without a restart.
    Pair(pair::Args),
    /// Remove a machine from the mesh, for good.
    ///
    /// Give its name, as `clipbroad status` shows it, or the first characters of its id. Every
    /// member stops syncing with it once it hears of the removal from another member. The removed
    /// machine cannot join again with the same identity: run `clipbroad reset` on it, then pair
    /// it again.
    Remove(remove::Args),
    /// Give this machine a new identity, to join a mesh again after it was removed.
    ///
    /// It deletes `secret.key`, `members.toml` (beside it) and the `[[peers]]` in `config.toml`,
    /// which only know the old identity. The rest of `config.toml` stays. The next start creates a
    /// new identity. If `clipbroad run` is running, it restarts right away with the new identity. Then pair this machine with any member of the mesh. The tray
    /// menu does the same.
    Reset(reset::Args),
    /// Sync the clipboard with the members of the mesh until Ctrl+C, SIGTERM or the tray icon's
    /// "Quit".
    ///
    /// Each text, image or set of files copied here is sent to every connected peer, and what a
    /// peer copies is written to this clipboard. Files are downloaded into a local cache first,
    /// and the clipboard gets their paths there. Peers that are offline miss what is copied
    /// meanwhile. Logs show this
    /// node's id (peers need it in their config), connections, and each clip's size and short
    /// hash, never its content. Content a password manager marks as sensitive is never read or
    /// sent. The tray icon shows the peers and the last clip, and its menu pairs a machine, pauses sync
    /// and installs updates.
    Run(run::Args),
    /// Print every change of the local clipboard, one line per change, until Ctrl+C.
    ///
    /// Each line shows the time (UTC), the formats the clipboard offers and a preview of its text
    /// (first 80 characters, with newlines and control characters escaped), or the size of its
    /// image or files. Content marked as
    /// sensitive shows the marker instead, and is never read. Nothing is sent to other machines.
    Watch(watch::Args),
    /// Start `clipbroad run` at login: install, uninstall, or check the service.
    ///
    /// It uses each OS's own mechanism: a systemd user unit on Linux, a `LaunchAgent` on macOS, and
    /// a `Run` registry value on Windows. The service runs this executable from its current path,
    /// so after moving the binary, run `service install` again.
    Service(service::Args),
    /// Show what the running daemon is doing: its peers and the last clips sent and received.
    ///
    /// It reads the status file that `clipbroad run` keeps up to date (`status.toml` beside the
    /// identity file), so it shows the daemon started with the same `--config`. Clips appear as
    /// time, size and short hash, never their content.
    Status(status::Args),
    /// Install the latest release, if it is newer, and restart the running daemon.
    ///
    /// A Homebrew install updates through Homebrew. Any other install downloads the release
    /// archive, checks its signature and checksum, and replaces this executable. The tray menu
    /// does the same.
    Update(update::Args),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let default_level = match cli.command {
        Command::Run(_) => LevelFilter::INFO,
        Command::Pair(_)
        | Command::Remove(_)
        | Command::Reset(_)
        | Command::Watch(_)
        | Command::Service(_)
        | Command::Status(_)
        | Command::Update(_) => LevelFilter::WARN,
    };
    init_logging(default_level, cli.verbose);
    match cli.command {
        Command::Pair(args) => runtime()?.block_on(pair::run(args)),
        Command::Remove(args) => runtime()?.block_on(remove::run(args)),
        Command::Reset(args) => runtime()?.block_on(reset::run(args)),
        // It owns the main thread for the tray icon, and starts its own runtime.
        Command::Run(args) => run::start(args),
        Command::Watch(args) => runtime()?.block_on(watch::run(args)),
        Command::Service(args) => service::run(args),
        Command::Status(args) => status::run(args),
        Command::Update(args) => runtime()?.block_on(update::run(args)),
    }
}

/// A single-threaded tokio runtime: the daemon's work is mostly waiting on the network.
fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
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
    let rust_log = env::var("RUST_LOG").ok();
    let (filter, invalid) = match rust_log.as_deref().and_then(parse_rust_log) {
        Some(Ok(targets)) => (targets, None),
        Some(Err(error)) => (Targets::new().with_default(level), Some(error)),
        None => (Targets::new().with_default(level), None),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(io::stderr().is_terminal()),
        )
        .init();
    if let Some(error) = invalid {
        tracing::warn!(%error, "ignoring RUST_LOG, using the -v level");
    }
}

/// Parses `RUST_LOG` as `target=level` and bare `level` directives, comma separated. Empty
/// directives are skipped, so a trailing comma does not turn into a match-all target. `None` if
/// nothing is left.
fn parse_rust_log(value: &str) -> Option<Result<Targets, ParseError>> {
    let directives: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|directive| !directive.is_empty())
        .collect();
    (!directives.is_empty()).then(|| directives.join(",").parse())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_log_sets_levels_per_target() {
        let targets = parse_rust_log("info, iroh=debug,")
            .expect("not blank")
            .expect("valid");
        assert_eq!(targets.default_level(), Some(LevelFilter::INFO));
        assert_eq!(
            targets.iter().collect::<Vec<_>>(),
            [("iroh", LevelFilter::DEBUG)]
        );
    }

    #[test]
    fn blank_rust_log_is_ignored() {
        assert!(parse_rust_log("").is_none());
        assert!(parse_rust_log(" , ,").is_none());
    }

    #[test]
    fn invalid_rust_log_is_an_error() {
        assert!(matches!(parse_rust_log("iroh=loud"), Some(Err(_))));
    }
}
