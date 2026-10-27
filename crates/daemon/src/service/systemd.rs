//! Linux: a systemd user unit that starts and stops with the graphical session.

use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use tracing::warn;

use super::{Status, remove_file, run_tool, succeeds, write_file};

const UNIT: &str = "clipbroad.service";

pub fn install(exe: &str, config: Option<&str>) -> Result<()> {
    write_file(&unit_path()?, &unit(exe, config))?;
    run_tool(systemctl().arg("daemon-reload"))?;
    run_tool(systemctl().args(["enable", UNIT]))?;
    // `restart` also starts a stopped unit, and picks up a changed unit file.
    run_tool(systemctl().args(["restart", UNIT]))?;
    check_session()
}

pub fn uninstall() -> Result<()> {
    // Fails if the unit was never installed: then there is nothing to stop.
    succeeds(systemctl().args(["disable", "--now", UNIT]))?;
    remove_file(&unit_path()?)?;
    run_tool(systemctl().arg("daemon-reload"))?;
    // Drops a "failed" state left behind, if any. Fails if there is none, which is fine.
    succeeds(systemctl().args(["reset-failed", UNIT]))?;
    Ok(())
}

pub fn status() -> Result<Status> {
    Ok(Status {
        installed: succeeds(systemctl().args(["is-enabled", "--quiet", UNIT]))?,
        running: succeeds(systemctl().args(["is-active", "--quiet", UNIT]))?,
        location: unit_path()?.display().to_string(),
        logs: "journalctl --user -u clipbroad".to_owned(),
    })
}

fn systemctl() -> Command {
    let mut command = Command::new("systemctl");
    command.arg("--user");
    command
}

/// `$XDG_CONFIG_HOME/systemd/user/clipbroad.service`, by default under `~/.config`.
fn unit_path() -> Result<PathBuf> {
    let config_dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| env::home_dir().map(|home| home.join(".config")))
        .context("cannot find the home directory")?;
    Ok(config_dir.join("systemd/user").join(UNIT))
}

/// Warns about session setups where the service would not work, with what to do about it.
fn check_session() -> Result<()> {
    // systemd sets XDG_RUNTIME_DIR itself; WAYLAND_DISPLAY comes only from the session.
    let environment = run_tool(systemctl().arg("show-environment"))?;
    if !environment
        .lines()
        .any(|line| line.starts_with("WAYLAND_DISPLAY="))
    {
        warn!(
            "the systemd user manager has no WAYLAND_DISPLAY, so the service cannot reach the \
             clipboard. Import it when the compositor starts, e.g. in hyprland.conf: \
             `exec-once = dbus-update-activation-environment --systemd WAYLAND_DISPLAY`"
        );
    }
    if !succeeds(systemctl().args(["is-active", "--quiet", "graphical-session.target"]))? {
        warn!(
            "graphical-session.target is not active, so the service will not start at login. \
             Start the compositor through a session manager that activates it (e.g. uwsm), or \
             add `exec-once = clipbroad run` to hyprland.conf instead of using the service"
        );
    }
    Ok(())
}

fn unit(exe: &str, config: Option<&str>) -> String {
    let mut exec_start = format!("{} run", quote(exe));
    if let Some(config) = config {
        write!(exec_start, " --config {}", quote(config)).expect("writing to a String never fails");
    }
    format!(
        "\
# Written by `clipbroad service install`. Run it again instead of editing this file.
[Unit]
Description=clipbroad: share the clipboard between your own machines
Documentation=https://github.com/fcamus00/clipbroad
# It needs the Wayland session, so it starts and stops with it.
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart={exec_start}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=graphical-session.target
"
    )
}

/// Quotes one `ExecStart` argument. systemd expands `%` specifiers and `$` variables even inside
/// quotes, so those are doubled.
fn quote(arg: &str) -> String {
    let mut quoted = String::from('"');
    for c in arg.chars() {
        match c {
            '\\' | '"' => quoted.extend(['\\', c]),
            '%' | '$' => quoted.extend([c, c]),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_the_executable_with_the_graphical_session() {
        let unit = unit("/home/me/.local/bin/clipbroad", None);
        assert!(
            unit.contains("\nExecStart=\"/home/me/.local/bin/clipbroad\" run\n"),
            "{unit}"
        );
        for line in [
            "PartOf=graphical-session.target",
            "After=graphical-session.target",
            "WantedBy=graphical-session.target",
            "Restart=on-failure",
            "RestartSec=5",
        ] {
            assert!(unit.lines().any(|l| l == line), "missing {line}:\n{unit}");
        }
    }

    #[test]
    fn unit_passes_the_config_only_when_given() {
        let unit = unit("/bin/clipbroad", Some("/tmp/test/config.toml"));
        assert!(
            unit.contains(
                "\nExecStart=\"/bin/clipbroad\" run --config \"/tmp/test/config.toml\"\n"
            ),
            "{unit}"
        );
    }

    #[test]
    fn special_characters_are_escaped() {
        assert_eq!(
            quote(r#"/home/me/my "100%" $HOME\bin"#),
            r#""/home/me/my \"100%%\" $$HOME\\bin""#
        );
    }
}
