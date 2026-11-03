//! Updating clipbroad, from `clipbroad update` and from the tray menu.
//!
//! The daemon looks at the latest GitHub release, and only a person installs it. How depends on
//! the [`Channel`]: Homebrew keeps track of what it installed, so it does the work. Every other
//! install gets the release archive verified and its binary swapped in.
//!
//! A signature made outside GitHub covers `SHA256SUMS`, which lists each archive with its version
//! in the name. So a change to a release on GitHub fails the signature, and an old signed release
//! does not pass as a new one.
//!
//! The running daemon is replaced by [`restart`], after it has shut down: `exec` keeps the process
//! that systemd and launchd know, and Windows has no `exec`, so there a new process starts.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Output, Stdio};
use std::str::FromStr;
use std::time::Duration;
use std::{env, fmt, fs, str, thread};

use anyhow::{Context, Result, anyhow, ensure};
use clipbroad_config::Paths;
use minisign_verify::{PublicKey, Signature};
use reqwest::{Client, Response, header, redirect};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::time;
use tracing::{debug, info, warn};

use crate::control::{self, Asked};

/// This binary's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How long the daemon waits after starting to look for a new version, so a restart loop or a
/// login does not start with a request.
pub const FIRST_CHECK: Duration = Duration::from_mins(1);
pub const CHECK_EVERY: Duration = Duration::from_hours(24);

#[cfg(not(feature = "update-smoke"))]
const RELEASES: &str = "https://github.com/fcamus00/clipbroad/releases";
/// The key that signs each release's `SHA256SUMS`. Its secret half never reaches GitHub.
#[cfg(any(not(feature = "update-smoke"), test))]
const PUBLIC_KEY: &str = include_str!("../../../packaging/minisign.pub");
const HOMEBREW_FORMULA: &str = "fcamus00/tap/clipbroad";
/// Where the Windows installer records what it installed.
const INSTALLER_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\fcamus00.clipbroad_is1";
/// Well above the size of a release archive, to stop a download that never ends.
const MAX_DOWNLOAD_BYTES: usize = 256 * 1024 * 1024;
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(5);
const TOOL_TIMEOUT: Duration = Duration::from_mins(1);
/// `brew` may refresh every tap first.
const BREW_TIMEOUT: Duration = Duration::from_mins(10);

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Config file of the daemon to restart, as given to `run --config`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

/// `clipbroad update`: installs the latest release, if newer, and restarts the running daemon.
pub async fn run(args: Args) -> Result<()> {
    let paths = match args.config {
        Some(config_file) => Paths::beside_config(config_file),
        None => Paths::discover()?,
    };
    let updater = Updater::new(Installation::current()?)?;
    let Some(version) = updater.check().await? else {
        println!("clipbroad {VERSION} is up to date");
        return Ok(());
    };
    println!("Updating to {version}...");
    updater.install(version).await?;
    println!("Updated to {version}");
    match control::restart(&paths).await {
        Asked::NoDaemon => {}
        Asked::Agreed => println!("The daemon is restarting on the new version"),
        Asked::Refused => println!(
            "The daemon could not restart now, and still runs the old version. Run \
             `clipbroad service install` to restart it"
        ),
    }
    Ok(())
}

/// A release version. Tags with a suffix (`v0.3.0-rc.1`) are not versions to update to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version([u64; 3]);

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let parts = text
            .split('.')
            .map(str::parse)
            .collect::<Result<Vec<u64>, _>>()
            .ok()
            .and_then(|parts| parts.try_into().ok());
        parts
            .map(Self)
            .with_context(|| format!("not a version: {text}"))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.0;
        write!(f, "{major}.{minor}.{patch}")
    }
}

/// Who installs new versions on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    /// Swapping the binary under Homebrew would leave it reporting the old version.
    Homebrew {
        /// Homebrew's own folder. `launchd` gives the daemon a `PATH` without it.
        prefix: PathBuf,
    },
    /// `install.sh`, the Windows installer or an archive unpacked by hand.
    Direct,
}

impl Channel {
    /// The channel of the binary at `exe`, with symlinks resolved. A Homebrew binary lives in
    /// `<prefix>/Cellar/clipbroad/<version>/bin`.
    fn of(exe: &Path) -> Self {
        exe.ancestors()
            .find(|dir| dir.ends_with("Cellar/clipbroad"))
            .and_then(|dir| dir.parent()?.parent())
            .map_or(Self::Direct, |prefix| Self::Homebrew {
                prefix: prefix.to_owned(),
            })
    }
}

/// This binary, found when it starts: once an update replaces the file, Linux reports its path
/// with ` (deleted)` at the end.
#[derive(Debug, Clone)]
pub struct Installation {
    /// The file to replace, and what a restart runs.
    exe: PathBuf,
    channel: Channel,
}

impl Installation {
    pub fn current() -> Result<Self> {
        let exe = env::current_exe().context("cannot find this executable's path")?;
        let resolved =
            fs::canonicalize(&exe).with_context(|| format!("cannot resolve {}", exe.display()))?;
        let channel = Channel::of(&resolved);
        let exe = match &channel {
            // The folder of the current version goes away with an upgrade, and this link stays.
            Channel::Homebrew { prefix } => prefix.join("opt/clipbroad/bin/clipbroad"),
            // Canonical paths on Windows start with `\\?\`, and there are no symlinks to follow.
            Channel::Direct if cfg!(windows) => exe,
            Channel::Direct => resolved,
        };
        Ok(Self { exe, channel })
    }

    /// Removes the executable a Windows update left aside. A running one cannot be deleted, and
    /// the daemon that started this one may take a moment to exit, so this keeps trying.
    pub fn remove_replaced(&self) {
        const ATTEMPTS: u32 = 30;
        let old = replaced(&self.exe);
        if !old.exists() {
            return;
        }
        thread::spawn(move || {
            for _ in 0..ATTEMPTS {
                if fs::remove_file(&old).is_ok() {
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }
            warn!("cannot remove {}", old.display());
        });
    }
}

/// Where a Windows update moves the running executable.
fn replaced(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// Runs the binary again, with the same arguments. Call it once the daemon has shut down.
///
/// An error ends the process with a failure, so a service manager starts it again.
#[cfg(unix)]
pub fn restart(installation: &Installation) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = std::process::Command::new(&installation.exe)
        .args(env::args_os().skip(1))
        .exec();
    Err(error).with_context(|| format!("cannot run {}", installation.exe.display()))
}

/// Starts the binary again, with the same arguments. Call it once the daemon has shut down, and
/// then exit.
#[cfg(windows)]
pub fn restart(installation: &Installation) -> Result<()> {
    let mut daemon = std::process::Command::new(&installation.exe);
    daemon.args(env::args_os().skip(1));
    crate::service::start_hidden(&mut daemon)
        .with_context(|| format!("cannot start {}", installation.exe.display()))?;
    Ok(())
}

/// Finds and installs new versions.
pub struct Updater {
    installation: Installation,
    releases: String,
    public_key: PublicKey,
    current: Version,
    /// Does not follow redirects: the one from `latest` is the answer.
    check_client: Client,
    download_client: Client,
}

impl fmt::Debug for Updater {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Updater")
            .field("installation", &self.installation)
            .field("releases", &self.releases)
            .field("current", &self.current)
            .finish_non_exhaustive()
    }
}

/// Where releases come from, the key that signs them and this binary's version.
#[cfg(not(feature = "update-smoke"))]
fn source() -> Result<(String, PublicKey, Version)> {
    let key = PublicKey::decode(PUBLIC_KEY).context("invalid embedded public key")?;
    Ok((RELEASES.to_owned(), key, VERSION.parse()?))
}

#[cfg(feature = "update-smoke")]
fn source() -> Result<(String, PublicKey, Version)> {
    let setting = |name| env::var(name).with_context(|| format!("{name} is not set"));
    let key = PublicKey::from_base64(&setting("CLIPBROAD_SMOKE_KEY")?).context("invalid key")?;
    let current = setting("CLIPBROAD_SMOKE_CURRENT")?.parse()?;
    Ok((setting("CLIPBROAD_SMOKE_RELEASES")?, key, current))
}

impl Updater {
    pub fn new(installation: Installation) -> Result<Self> {
        let (releases, public_key, current) = source()?;
        // reqwest takes rustls's crypto from here, and iroh does not set it. It fails if already
        // set, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Sends no version, OS or machine id: GitHub learns only that someone asked.
        let client = || {
            Client::builder()
                .user_agent("clipbroad")
                .https_only(releases.starts_with("https://"))
                .connect_timeout(CONNECT_TIMEOUT)
        };
        Ok(Self {
            check_client: client()
                .redirect(redirect::Policy::none())
                .timeout(CHECK_TIMEOUT)
                .build()
                .context("cannot build the HTTP client")?,
            download_client: client()
                .timeout(DOWNLOAD_TIMEOUT)
                .build()
                .context("cannot build the HTTP client")?,
            installation,
            releases,
            public_key,
            current,
        })
    }

    /// The latest release, if it is newer than this binary.
    pub async fn check(&self) -> Result<Option<Version>> {
        Ok(Some(self.latest().await?).filter(|latest| *latest > self.current))
    }

    /// The latest stable release, from where GitHub redirects `releases/latest`. It needs no JSON,
    /// and the GitHub API would allow only 60 requests an hour per IP.
    async fn latest(&self) -> Result<Version> {
        let url = format!("{}/latest", self.releases);
        let response = self
            .check_client
            .head(url)
            .send()
            .await
            .context("cannot reach GitHub")?;
        let location = response
            .headers()
            .get(header::LOCATION)
            .context("GitHub did not name the latest release")?
            .to_str()
            .context("GitHub did not name the latest release")?;
        version_in(location)
    }

    pub async fn install(&self, version: Version) -> Result<()> {
        match &self.installation.channel {
            Channel::Homebrew { prefix } => self.install_with_homebrew(prefix, version).await,
            Channel::Direct => self.install_direct(version).await,
        }
    }

    async fn install_with_homebrew(&self, prefix: &Path, version: Version) -> Result<()> {
        let brew = prefix.join("bin/brew");
        // `brew upgrade` refreshes taps by itself only once a day.
        output(Command::new(&brew).arg("update"), BREW_TIMEOUT)
            .await
            .context("`brew update` failed")?;
        output(
            Command::new(&brew).args(["upgrade", HOMEBREW_FORMULA]),
            BREW_TIMEOUT,
        )
        .await
        .context("`brew upgrade` failed")?;
        // A new tag reaches the tap a few minutes after the release, and `brew` does not fail then.
        check_version(&self.installation.exe, version)
            .await
            .with_context(|| format!("Homebrew does not have {version} yet"))
    }

    async fn install_direct(&self, version: Version) -> Result<()> {
        let tag = format!("v{version}");
        let sums = self.download(&tag, "SHA256SUMS").await?;
        let signature = self
            .download(&tag, "SHA256SUMS.minisig")
            .await
            .with_context(|| format!("{version} is not signed yet"))?;
        verify(&self.public_key, &sums, &signature)?;
        let sums = str::from_utf8(&sums).context("invalid SHA256SUMS")?;
        let folder = format!("clipbroad-{tag}-{}-{}", env::consts::OS, env::consts::ARCH);
        let archive = format!("{folder}.{}", if cfg!(windows) { "zip" } else { "tar.gz" });
        let expected =
            checksum(sums, &archive).with_context(|| format!("no {archive} in this release"))?;
        let bytes = self.download(&tag, &archive).await?;
        ensure!(
            sha256(&bytes) == expected,
            "{archive} has the wrong checksum"
        );

        let exe = &self.installation.exe;
        let dir = exe.parent().context("this executable has no folder")?;
        // In the executable's folder, so the final rename stays on one disk.
        let work = tempfile::Builder::new()
            .prefix(".clipbroad-update-")
            .tempdir_in(dir)
            .with_context(|| format!("cannot write to {}", dir.display()))?;
        fs::write(work.path().join(&archive), &bytes).context("cannot save the archive")?;
        output(
            Command::new(tar())
                .arg("-xf")
                .arg(&archive)
                .current_dir(work.path()),
            TOOL_TIMEOUT,
        )
        .await
        .with_context(|| format!("cannot unpack {archive}"))?;
        let name = format!("clipbroad{}", env::consts::EXE_SUFFIX);
        let new = work.path().join(&folder).join(name);
        // Catches a binary that does not run here while the old one is still in place.
        check_version(&new, version).await?;
        swap(&new, exe).with_context(|| format!("cannot replace {}", exe.display()))?;
        if exe.with_file_name("unins000.exe").exists() {
            // Only what Settings > Apps shows, so a failure changes nothing that matters.
            let mut reg = Command::new("reg");
            reg.args(["add", INSTALLER_KEY, "/v", "DisplayVersion", "/d"])
                .arg(version.to_string())
                .arg("/f");
            if let Err(error) = output(&mut reg, TOOL_TIMEOUT).await {
                warn!("cannot update the installed version in Settings: {error:#}");
            }
        }
        Ok(())
    }

    async fn download(&self, tag: &str, name: &str) -> Result<Vec<u8>> {
        let url = format!("{}/download/{tag}/{name}", self.releases);
        let mut response = self
            .download_client
            .get(url)
            .send()
            .await
            .and_then(Response::error_for_status)
            .with_context(|| format!("cannot download {name}"))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("cannot download {name}"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= MAX_DOWNLOAD_BYTES,
                "{name} is too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// The version in a release URL such as `.../releases/tag/v0.3.1`.
fn version_in(url: &str) -> Result<Version> {
    url.rsplit('/')
        .next()
        .and_then(|tag| tag.strip_prefix('v'))
        .with_context(|| format!("no release version in {url}"))?
        .parse()
}

/// Checks that `signature`, a minisign file, covers `sums`.
fn verify(key: &PublicKey, sums: &[u8], signature: &[u8]) -> Result<()> {
    let signature = str::from_utf8(signature).context("invalid release signature")?;
    let signature = Signature::decode(signature).context("invalid release signature")?;
    key.verify(sums, &signature, false)
        .context("the release signature does not match")
}

/// The checksum of `name` in a `SHA256SUMS` file. The whole name must match, so that the version
/// in it counts.
fn checksum<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines()
        .find_map(|line| line.split_once("  ").filter(|(_, file)| *file == name))
        .map(|(sum, _)| sum)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |hex, byte| format!("{hex}{byte:02x}"))
}

/// The `tar` that comes with the OS. On Windows it also unpacks `.zip`, and the full path skips
/// any GNU tar on `PATH`, which does not.
fn tar() -> PathBuf {
    if cfg!(windows) {
        let root = env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join(r"System32\tar.exe")
    } else {
        "tar".into()
    }
}

/// Runs a tool and returns its output, or an error with what it said.
async fn output(command: &mut Command, limit: Duration) -> Result<Output> {
    command.stdin(Stdio::null()).kill_on_drop(true);
    let output = time::timeout(limit, command.output())
        .await
        .map_err(|_| anyhow!("{command:?} took too long"))?
        .with_context(|| format!("cannot run {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

/// Checks that the binary at `exe` runs and is `version`.
async fn check_version(exe: &Path, version: Version) -> Result<()> {
    let reported = output(Command::new(exe).arg("--version"), TOOL_TIMEOUT).await?;
    let reported = String::from_utf8_lossy(&reported.stdout);
    let reported = reported.trim();
    ensure!(
        reported == format!("clipbroad {version}"),
        "{} reports `{reported}`, not {version}",
        exe.display()
    );
    Ok(())
}

/// Puts the binary at `new` in place of `exe`, which may be running.
///
/// A rename, and never a write into the file: Linux refuses to write to a running executable, and
/// macOS can kill a binary overwritten in place. Windows does not let a running executable be
/// replaced or deleted, but it can be renamed.
fn swap(new: &Path, exe: &Path) -> std::io::Result<()> {
    if !cfg!(windows) {
        return fs::rename(new, exe);
    }
    let old = replaced(exe);
    // Left by an update whose daemon never cleaned up. Fails if there is none, which is fine.
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    fs::rename(new, exe).inspect_err(|_| {
        // Puts the old one back, so the next start finds a binary.
        let _ = fs::rename(&old, exe);
    })
}

/// What the update item of the menu is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Checking,
    Available(Version),
    Installing(Version),
}

/// A check or an install on its way. The main loop polls it next to everything else, so sync
/// goes on meanwhile.
pub type Task<'a> = Pin<Box<dyn Future<Output = Finished> + Send + 'a>>;

/// The end of a [`Task`].
#[derive(Debug)]
pub enum Finished {
    Checked {
        /// Whether a person asked, and so expects an answer.
        asked: bool,
        latest: Result<Option<Version>>,
    },
    Installed {
        version: Version,
        result: Result<()>,
    },
}

/// What the main loop does after a [`Finished`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// To show in the menu.
    pub note: Option<String>,
    pub restart: bool,
}

/// The daemon's side of updating: at most one [`Task`], and what the menu shows about it.
pub struct Updates<'a> {
    /// `None` for `run --fake`, which never looks for updates.
    updater: Option<&'a Updater>,
    state: State,
    pub task: Option<Task<'a>>,
}

impl<'a> Updates<'a> {
    pub fn new(updater: Option<&'a Updater>) -> Self {
        Self {
            updater,
            state: State::Idle,
            task: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The new version, if there is one.
    pub fn available(&self) -> Option<Version> {
        match self.state {
            State::Available(version) | State::Installing(version) => Some(version),
            State::Idle | State::Checking => None,
        }
    }

    /// A check from the timer. It says nothing unless it finds something.
    pub fn check_on_schedule(&mut self) {
        if self.task.is_none() {
            self.start_check(false);
        }
    }

    /// The update item was chosen: checks, or installs what a check found. `blocked` means that a
    /// restart would get in the way now. Returns a note for the menu.
    pub fn choose(&mut self, blocked: bool) -> Option<String> {
        if self.updater.is_none() {
            return Some("Updates are not available on this machine".to_owned());
        }
        // Either one replaces a scheduled check that is running.
        match self.state {
            State::Idle => self.start_check(true),
            State::Available(version) if !blocked => self.start_install(version),
            State::Available(_) | State::Checking | State::Installing(_) => {}
        }
        None
    }

    fn start_check(&mut self, asked: bool) {
        let Some(updater) = self.updater else {
            return;
        };
        if asked {
            self.state = State::Checking;
        }
        self.task = Some(Box::pin(async move {
            Finished::Checked {
                asked,
                latest: updater.check().await,
            }
        }));
    }

    fn start_install(&mut self, version: Version) {
        let Some(updater) = self.updater else {
            return;
        };
        self.state = State::Installing(version);
        self.task = Some(Box::pin(async move {
            Finished::Installed {
                version,
                result: updater.install(version).await,
            }
        }));
    }

    /// Takes the end of the task.
    pub fn finish(&mut self, finished: Finished) -> Outcome {
        self.task = None;
        let mut outcome = Outcome::default();
        match finished {
            Finished::Checked {
                latest: Ok(Some(version)),
                ..
            } => {
                if self.state != State::Available(version) {
                    info!(%version, "a new version of clipbroad is available");
                }
                self.state = State::Available(version);
            }
            Finished::Checked {
                asked,
                latest: Ok(None),
            } => {
                self.state = State::Idle;
                if asked {
                    outcome.note = Some(format!("clipbroad {VERSION} is up to date"));
                }
            }
            Finished::Checked {
                asked: true,
                latest: Err(error),
            } => {
                warn!("cannot check for updates: {error:#}");
                self.state = State::Idle;
                outcome.note = Some(format!("Cannot check for updates: {error}"));
            }
            // A machine without internet is no reason to bother anyone.
            Finished::Checked {
                asked: false,
                latest: Err(error),
            } => debug!("cannot check for updates: {error:#}"),
            Finished::Installed {
                version,
                result: Ok(()),
            } => {
                info!(%version, "update installed, restarting");
                outcome.restart = true;
            }
            Finished::Installed {
                version,
                result: Err(error),
            } => {
                warn!(%version, "update failed: {error:#}");
                self.state = State::Available(version);
                outcome.note = Some(format!("Update failed: {error}"));
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests;
