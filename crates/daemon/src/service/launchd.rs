//! macOS: a `LaunchAgent`, which launchd starts at every login into the user's GUI session.

use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{Status, output, remove_file, run_tool, succeeds, write_file};

const LABEL: &str = "io.github.fcamus00.clipbroad";

pub fn install(exe: &str, config: Option<&str>) -> Result<()> {
    let plist = plist_path()?;
    write_file(
        &plist,
        &plist_xml(exe, config, &log_path()?.display().to_string()),
    )?;
    let domain = gui_domain()?;
    // `bootstrap` refuses a loaded service, so unload the old one first. Fails if none is loaded.
    succeeds(launchctl().args(["bootout", &format!("{domain}/{LABEL}")]))?;
    // Right after a `bootout`, launchd may still be removing the old one: retry for a moment.
    let mut attempts = 1;
    loop {
        match run_tool(launchctl().arg("bootstrap").arg(&domain).arg(&plist)) {
            Ok(_) => return Ok(()),
            Err(_) if attempts < 3 => {
                attempts += 1;
                thread::sleep(Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn uninstall() -> Result<()> {
    // Fails if the service is not loaded: then there is nothing to stop.
    succeeds(launchctl().args(["bootout", &format!("{}/{LABEL}", gui_domain()?)]))?;
    remove_file(&plist_path()?)
}

pub fn status() -> Result<Status> {
    let plist = plist_path()?;
    let print = output(launchctl().args(["print", &format!("{}/{LABEL}", gui_domain()?)]))?;
    Ok(Status {
        installed: plist.exists(),
        running: print.status.success()
            && String::from_utf8_lossy(&print.stdout).contains("state = running"),
        location: plist.display().to_string(),
        logs: log_path()?.display().to_string(),
    })
}

fn launchctl() -> Command {
    Command::new("launchctl")
}

/// The domain of the logged-in user's GUI session, where the clipboard is available.
fn gui_domain() -> Result<String> {
    let uid = run_tool(Command::new("id").arg("-u"))?;
    Ok(format!("gui/{}", uid.trim()))
}

fn home() -> Result<PathBuf> {
    env::home_dir().context("cannot find the home directory")
}

fn plist_path() -> Result<PathBuf> {
    Ok(home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn log_path() -> Result<PathBuf> {
    Ok(home()?.join("Library/Logs/clipbroad.log"))
}

fn plist_xml(exe: &str, config: Option<&str>, log: &str) -> String {
    let config = config.into_iter().flat_map(|config| ["--config", config]);
    let mut args = String::new();
    for arg in [exe, "run"].into_iter().chain(config) {
        write!(args, "\n    <string>{}</string>", escape(arg))
            .expect("writing to a String never fails");
    }
    let log = escape(log);
    // KeepAlive restarts it only after a crash or an error exit, every 10 s at most (launchd's
    // default throttle). Interactive keeps macOS from slowing it down in the background.
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `clipbroad service install`. Run it again instead of editing this file. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>{args}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// Escapes text for an XML element.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_executable_at_login() {
        let plist = plist_xml(
            "/usr/local/bin/clipbroad",
            None,
            "/Users/me/Library/Logs/clipbroad.log",
        );
        assert!(
            plist.contains(
                "<key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/clipbroad</string>
    <string>run</string>
  </array>"
            ),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/Users/me/Library/Logs/clipbroad.log</string>"),
            "{plist}"
        );
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"), "{plist}");
        assert!(
            plist.contains(&format!("<string>{LABEL}</string>")),
            "{plist}"
        );
    }

    #[test]
    fn plist_passes_the_config_only_when_given_and_escapes_it() {
        let plist = plist_xml(
            "/Users/me/bin/clipbroad",
            Some("/Users/me/R&D <test>/config.toml"),
            "/tmp/log",
        );
        assert!(
            plist.contains(
                "<string>run</string>
    <string>--config</string>
    <string>/Users/me/R&amp;D &lt;test&gt;/config.toml</string>
  </array>"
            ),
            "{plist}"
        );
    }
}
