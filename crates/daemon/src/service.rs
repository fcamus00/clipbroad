//! `clipbroad service`: starts `clipbroad run` at login through the OS's own service manager.
//!
//! The service runs this executable by its absolute path, so moving the binary needs a new
//! `service install`. Each platform module writes its service definition and drives the OS tool:
//! a systemd user unit on Linux, a `LaunchAgent` on macOS, a `Run` registry value on Windows.

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};

#[cfg_attr(target_os = "linux", path = "service/systemd.rs")]
#[cfg_attr(target_os = "macos", path = "service/launchd.rs")]
#[cfg_attr(windows, path = "service/windows.rs")]
mod platform;

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Install the service and (re)start it. Running it again updates the service.
    Install {
        /// Config file for the service, as in `run --config`. Without it, the service uses the
        /// default config file.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Stop the service and remove it.
    Uninstall,
    /// Show whether the service is installed and running.
    Status,
}

/// What `status` reports, in the same shape on every OS.
struct Status {
    installed: bool,
    running: bool,
    /// The service definition: a file, or the registry value on Windows.
    location: String,
    /// Where the daemon's logs go, or the command that shows them.
    logs: String,
}

pub fn run(args: Args) -> Result<()> {
    match args.action {
        Action::Install { config } => {
            let exe = std::env::current_exe().context("cannot find this executable's path")?;
            // The service does not start in the current directory: store an absolute path.
            let config = config
                .map(std::path::absolute)
                .transpose()
                .context("invalid --config path")?;
            let config = config.map(utf8).transpose()?;
            platform::install(&utf8(exe)?, config.as_deref())?;
        }
        Action::Uninstall => platform::uninstall()?,
        Action::Status => {}
    }
    print_status(&platform::status()?);
    Ok(())
}

fn print_status(status: &Status) {
    let yes_no = |value| if value { "yes" } else { "no" };
    println!("installed (starts at login): {}", yes_no(status.installed));
    println!("running: {}", yes_no(status.running));
    println!("service: {}", status.location);
    println!("logs: {}", status.logs);
}

/// Service definitions are text, so paths must be valid UTF-8.
fn utf8(path: PathBuf) -> Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|path| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))
}

fn output(command: &mut Command) -> Result<Output> {
    command
        .output()
        .with_context(|| format!("cannot run {command:?}"))
}

/// Runs an OS tool and returns its stdout. Fails with the tool's error output if it fails.
fn run_tool(command: &mut Command) -> Result<String> {
    let output = output(command)?;
    ensure!(
        output.status.success(),
        "{command:?} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether an OS tool succeeds. Its output is discarded.
fn succeeds(command: &mut Command) -> Result<bool> {
    Ok(output(command)?.status.success())
}

/// Writes a service file, creating its directory if needed.
#[cfg(unix)]
fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))
}

/// Removes a service file. A missing file is fine: there is nothing to remove.
#[cfg(unix)]
fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result.with_context(|| format!("cannot remove {}", path.display())),
    }
}
