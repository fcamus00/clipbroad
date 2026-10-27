//! Windows: a value under the user's `Run` registry key, which Windows runs at every login.
//!
//! A `Run` value needs no admin rights and shows up in Task Manager's startup apps. A scheduled
//! task could restart clipbroad after a crash, but `schtasks /sc onlogon` needs an admin
//! prompt, and its defaults stop the task on battery and after 3 days.
//!
//! clipbroad is a console program, so Windows would open a console window for it at login.
//! `conhost.exe --headless` runs it with a hidden console instead. Its log output goes nowhere.

use std::env;
use std::fmt::Write as _;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{Status, run_tool, succeeds};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "clipbroad";
/// Win32 process creation flag: the new process does not share this terminal's console, so
/// closing the terminal does not stop it.
const DETACHED_PROCESS: u32 = 0x0000_0008;

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
    stop_daemon()?;
    // Start it now with the same command Windows runs at login.
    Command::new(&conhost)
        .raw_arg(&args)
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {conhost}"))?;
    // Give it a moment to show up in `status`.
    thread::sleep(Duration::from_secs(1));
    Ok(())
}

pub fn uninstall() -> Result<()> {
    // Fails if the value does not exist: then there is nothing to remove.
    succeeds(Command::new("reg").args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]))?;
    stop_daemon()
}

pub fn status() -> Result<Status> {
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
    Ok(Status {
        installed: succeeds(Command::new("reg").args(["query", RUN_KEY, "/v", VALUE_NAME]))?,
        // One CSV line per process, starting with its quoted image name. With no match, tasklist
        // prints a (translated) message instead.
        running: tasks.to_lowercase().contains(&format!("\"{exe_name}\"")),
        location: format!(r"registry value {RUN_KEY}\{VALUE_NAME}"),
        logs: "not kept on Windows. To see them, run `clipbroad service uninstall`, then \
               `clipbroad run` in a terminal"
            .to_owned(),
    })
}

/// Stops every other process of this executable, including a `clipbroad run` started by hand.
fn stop_daemon() -> Result<()> {
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
