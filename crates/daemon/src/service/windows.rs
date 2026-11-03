//! Windows: a value under the user's `Run` registry key, which Windows runs at every login.
//!
//! A `Run` value needs no admin rights and shows up in Task Manager's startup apps. A scheduled
//! task could restart clipbroad after a crash, but `schtasks /sc onlogon` needs an admin
//! prompt, and its defaults stop the task on battery and after 3 days.
//!
//! clipbroad is a console program, so Windows would open a console window for it at login.
//! `conhost.exe --headless` runs it with a hidden console instead. Its log output goes nowhere.

use std::fmt::Write as _;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, io};

use anyhow::{Context, Result};
use clipbroad_config::Paths;

use super::{Status, run_tool, succeeds};
use crate::control;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "clipbroad";
/// Win32 process creation flag: the new process gets its own console without a window, so it
/// shows nothing and closing this terminal does not stop it.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long a daemon that agreed to quit gets before it is killed.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn install(exe: &str, config: Option<&str>) -> Result<()> {
    let conhost = conhost_path();
    let args = conhost_args(exe, config);
    run_tool(Command::new("reg").args([
        "add",
        RUN_KEY,
        "/v",
        VALUE_NAME,
        "/t",
        "REG_SZ",
        "/d",
        &format!("\"{conhost}\" {args}"),
        "/f",
    ]))?;
    let paths = match config {
        Some(config) => Paths::beside_config(config.into()),
        None => Paths::discover()?,
    };
    stop_daemon(&paths)?;
    // Start it now. Not through conhost: started from here, `conhost --headless` stays up but the
    // daemon under it does not run (seen in CI, packaging/windows/test.ps1). A Run value cannot pass
    // creation flags, so at login conhost is still the way to hide the console.
    let mut daemon = Command::new(exe);
    daemon.arg("run");
    if let Some(config) = config {
        daemon.args(["--config", config]);
    }
    start_hidden(&mut daemon).with_context(|| format!("cannot start {exe}"))?;
    // Give it a moment to show up in `status`.
    thread::sleep(Duration::from_secs(1));
    Ok(())
}

/// Starts `daemon` without a window, apart from this process and its console.
pub fn start_hidden(daemon: &mut Command) -> io::Result<Child> {
    daemon
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

pub fn uninstall() -> Result<()> {
    // Fails if the value does not exist: then there is nothing to remove.
    succeeds(Command::new("reg").args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]))?;
    stop_daemon(&Paths::discover()?)
}

pub fn status() -> Result<Status> {
    Ok(Status {
        installed: succeeds(Command::new("reg").args(["query", RUN_KEY, "/v", VALUE_NAME]))?,
        running: running()?,
        location: format!(r"registry value {RUN_KEY}\{VALUE_NAME}"),
        logs: "not kept on Windows. To see them, run `clipbroad service uninstall`, then \
               `clipbroad run` in a terminal"
            .to_owned(),
    })
}

/// Whether another process of this executable runs.
fn running() -> Result<bool> {
    let exe_name = exe_name()?;
    let tasks = run_tool(Command::new("tasklist").args([
        "/fo",
        "csv",
        "/nh",
        "/fi",
        &format!("imagename eq {exe_name}"),
        "/fi",
        &format!("pid ne {}", std::process::id()),
    ]))?;
    // One CSV line per process, starting with its quoted image name. With no match, tasklist
    // prints a (translated) message instead.
    Ok(tasks.to_lowercase().contains(&format!("\"{exe_name}\"")))
}

/// Stops the daemon on `paths`, then every other process of this executable, including a
/// `clipbroad run` started by hand.
fn stop_daemon(paths: &Paths) -> Result<()> {
    // Killing it would leave its icon in the notification area until the mouse goes over it, so
    // ask first. A daemon on another config, or from an older version, does not get asked.
    if crate::runtime()?.block_on(control::quit(paths)) {
        let deadline = Instant::now() + QUIT_TIMEOUT;
        while running()? && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }
    // Fails if none is running, which is fine.
    succeeds(Command::new("taskkill").args([
        "/f",
        "/im",
        &exe_name()?,
        "/fi",
        &format!("pid ne {}", std::process::id()),
    ]))?;
    Ok(())
}

/// This executable's file name in lowercase, e.g. `clipbroad.exe`.
fn exe_name() -> Result<String> {
    let exe = env::current_exe().context("cannot find this executable's path")?;
    let name = exe
        .file_name()
        .context("this executable's path has no file name")?;
    Ok(name.to_string_lossy().to_lowercase())
}

fn conhost_path() -> String {
    let root = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    format!(r"{root}\System32\conhost.exe")
}

/// The arguments for `conhost.exe`. Windows paths cannot contain `"`, so quoting is enough.
fn conhost_args(exe: &str, config: Option<&str>) -> String {
    let mut args = format!(r#"--headless "{exe}" run"#);
    if let Some(config) = config {
        write!(args, r#" --config "{config}""#).expect("writing to a String never fails");
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_the_executable_with_a_hidden_console() {
        assert_eq!(
            conhost_args(r"C:\Users\Me Too\bin\clipbroad.exe", None),
            r#"--headless "C:\Users\Me Too\bin\clipbroad.exe" run"#
        );
    }

    #[test]
    fn passes_the_config_only_when_given() {
        assert_eq!(
            conhost_args(
                r"C:\bin\clipbroad.exe",
                Some(r"C:\Users\me\test\config.toml")
            ),
            r#"--headless "C:\bin\clipbroad.exe" run --config "C:\Users\me\test\config.toml""#
        );
    }

    #[test]
    fn conhost_is_in_the_system_directory() {
        assert!(
            conhost_path()
                .to_lowercase()
                .ends_with(r"\system32\conhost.exe"),
            "{}",
            conhost_path()
        );
    }
}
