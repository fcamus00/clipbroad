use clap::Parser;

/// Share the clipboard between your own machines.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
