//! `clipbroad update` from end to end: the real binary, in a temporary folder, against a fake
//! release on localhost, with the daemon running.
//!
//! The binary needs the `update-smoke` feature, so that the environment can point it there:
//! `cargo nextest run -p clipbroad --features update-smoke --test update_smoke`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{Cursor, Read, Write as _};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const OLD_VERSION: &str = "0.0.1";
const TIMEOUT: Duration = Duration::from_secs(30);
const NEW_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Added to the new binary so it differs from the old one and still runs.
const NEW_MARK: &[u8] = b"clipbroad update smoke test";

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A web server with the two things clipbroad asks GitHub for.
struct FakeReleases {
    url: String,
    files: Files,
}

impl FakeReleases {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let url = format!(
            "http://{}/releases",
            listener.local_addr().expect("an address")
        );
        let files = Files::default();
        let (shared, latest) = (Arc::clone(&files), format!("{url}/tag/v{NEW_VERSION}"));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                answer(stream, &shared, &latest);
            }
        });
        Self { url, files }
    }

    fn publish(&self, name: &str, bytes: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .expect("not poisoned")
            .insert(name.to_owned(), bytes.into());
    }

    /// Publishes `SHA256SUMS` for an archive and returns the key that signs it. The archive itself
    /// is up to the test.
    fn publish_sums(&self, archive_bytes: &[u8]) -> String {
        let mut sum = String::new();
        for byte in Sha256::digest(archive_bytes) {
            write!(sum, "{byte:02x}").expect("writing to a String never fails");
        }
        let sums = format!("{sum}  {}.{}\n", folder(), extension());
        self.publish("SHA256SUMS", sums.clone());
        let keys = minisign::KeyPair::generate_unencrypted_keypair().expect("a key pair");
        let signature =
            minisign::sign(None, &keys.sk, Cursor::new(sums.as_bytes()), None, None).expect("sign");
        self.publish("SHA256SUMS.minisig", signature.to_string());
        keys.pk.to_base64()
    }
}

fn answer(mut stream: TcpStream, files: &Files, latest: &str) {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|end| end == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
    let request = String::from_utf8_lossy(&request);
    let path = request.split(' ').nth(1).unwrap_or_default();
    let file = path.rsplit('/').next().unwrap_or_default();
    let file = files.lock().expect("not poisoned").get(file).cloned();
    let (head, body) = if path.ends_with("/latest") {
        (format!("302 Found\r\nLocation: {latest}"), Vec::new())
    } else if let Some(file) = file {
        ("200 OK".to_owned(), file)
    } else {
        ("404 Not Found".to_owned(), Vec::new())
    };
    let head = format!(
        "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn folder() -> String {
    format!(
        "clipbroad-v{NEW_VERSION}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn extension() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

/// A folder holding a copy of the binary and its own config, on a free port.
struct Machine {
    dir: tempfile::TempDir,
    exe: PathBuf,
    config: PathBuf,
    releases: String,
    key: String,
    current: &'static str,
}

impl Machine {
    fn new(releases: &FakeReleases, key: String) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = Path::new(env!("CARGO_BIN_EXE_clipbroad"));
        let exe = dir.path().join(source.file_name().expect("a file name"));
        fs::copy(source, &exe).expect("copy the binary");
        let config = dir.path().join("config.toml");
        let port = UdpSocket::bind("127.0.0.1:0")
            .and_then(|socket| socket.local_addr())
            .expect("a free port")
            .port();
        fs::write(&config, format!("listen_port = {port}\nmdns = false\n")).expect("config");
        Self {
            dir,
            exe,
            config,
            releases: releases.url.clone(),
            key,
            current: OLD_VERSION,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.exe);
        command
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .env("CLIPBROAD_SMOKE_RELEASES", &self.releases)
            .env("CLIPBROAD_SMOKE_KEY", &self.key)
            .env("CLIPBROAD_SMOKE_CURRENT", self.current);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("clipbroad should start")
    }

    fn update(&self) -> Output {
        self.run(&["update"])
    }

    fn start_daemon(&self) -> Daemon {
        let log = fs::File::create(self.dir.path().join("daemon.log")).expect("log file");
        let child = self
            .command(&["run", "--fake"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("the daemon should start");
        Daemon { child, pid: None }
    }

    /// The PID `clipbroad status` reports, once there is a daemon that passes `check`.
    fn wait_for_pid(&self, check: impl Fn(u32) -> bool) -> u32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let status = self.run(&["status"]);
            let status = String::from_utf8_lossy(&status.stdout);
            let pid = status
                .split("running (pid ")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .and_then(|pid| pid.parse().ok());
            if let Some(pid) = pid.filter(|pid| check(*pid)) {
                return pid;
            }
            assert!(Instant::now() < deadline, "no such daemon: {status}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.dir.path().join("daemon.log")).unwrap_or_default()
    }

    fn assert_not_replaced(&self) {
        let new = fs::read(&self.exe).expect("the binary");
        assert!(!new.ends_with(NEW_MARK), "the binary changed");
        let files: Vec<_> = fs::read_dir(self.dir.path())
            .expect("the folder")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("update") || name.contains(".old"))
            .collect();
        assert_eq!(files, Vec::<String>::new(), "files left behind");
    }
}

/// Stops the daemon at the end of a test, also one that restarted as another process.
struct Daemon {
    child: Child,
    pid: Option<u32>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(pid) = self.pid {
            let _ = kill(pid);
        }
    }
}

fn kill(pid: u32) -> std::io::Result<Output> {
    if cfg!(windows) {
        Command::new("taskkill")
            .args(["/f", "/pid", &pid.to_string()])
            .output()
    } else {
        Command::new("kill").arg(pid.to_string()).output()
    }
}

/// Packs `binary` the way the release workflow does, with the `tar` of the OS.
fn archive(binary: &[u8], file_name: &std::ffi::OsStr) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("temp dir");
    let folder = folder();
    fs::create_dir(dir.path().join(&folder)).expect("folder");
    let exe = dir.path().join(&folder).join(file_name);
    fs::write(&exe, binary).expect("binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("permissions");
    }
    let name = format!("{folder}.{}", extension());
    let flags = if cfg!(windows) { "-acf" } else { "-czf" };
    let tar = if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        format!(r"{root}\System32\tar.exe")
    } else {
        "tar".to_owned()
    };
    let status = Command::new(tar)
        .args([flags, &name, &folder])
        .current_dir(dir.path())
        .status()
        .expect("tar should run");
    assert!(status.success());
    fs::read(dir.path().join(name)).expect("the archive")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn replaces_the_binary_and_restarts_the_daemon() {
    let source = Path::new(env!("CARGO_BIN_EXE_clipbroad"));
    let mut new_binary = fs::read(source).expect("the binary");
    new_binary.extend_from_slice(NEW_MARK);
    let archive = archive(&new_binary, source.file_name().expect("a file name"));

    let releases = FakeReleases::start();
    let key = releases.publish_sums(&archive);
    releases.publish(&format!("{}.{}", folder(), extension()), archive);
    let machine = Machine::new(&releases, key);
    let mut daemon = machine.start_daemon();
    let first_pid = machine.wait_for_pid(|_| true);

    let output = machine.update();
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("Updated to {NEW_VERSION}")),
        "{text}"
    );
    assert!(text.contains("restarting"), "{text}");
    assert_eq!(fs::read(&machine.exe).expect("the binary"), new_binary);

    if cfg!(target_os = "linux") {
        // The same process, which systemd keeps tracking, running the new file.
        machine.wait_for_pid(|pid| pid == first_pid);
        let deadline = Instant::now() + TIMEOUT;
        while machine.log().matches("node started").count() < 2 {
            assert!(Instant::now() < deadline, "no restart: {}", machine.log());
            thread::sleep(Duration::from_millis(100));
        }
        machine.wait_for_pid(|pid| pid == first_pid);
        assert!(daemon.child.try_wait().expect("try_wait").is_none());
    } else {
        // A new process, which the menu bar accepts an icon from on macOS. On Windows, the old
        // executable stays aside until the old process is gone.
        daemon.pid = Some(machine.wait_for_pid(|pid| pid != first_pid));
    }
}

#[test]
fn says_when_there_is_nothing_new() {
    let releases = FakeReleases::start();
    let key = releases.publish_sums(b"anything");
    let mut machine = Machine::new(&releases, key);
    machine.current = NEW_VERSION;
    let output = machine.update();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("is up to date"));
    machine.assert_not_replaced();
}

#[test]
fn rejects_an_archive_that_is_not_the_signed_one() {
    let releases = FakeReleases::start();
    let key = releases.publish_sums(b"the signed archive");
    releases.publish(&format!("{}.{}", folder(), extension()), "another archive");
    let machine = Machine::new(&releases, key);
    let output = machine.update();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("wrong checksum"),
        "{}",
        stderr(&output)
    );
    machine.assert_not_replaced();
}

#[test]
fn rejects_a_release_signed_by_another_key() {
    let releases = FakeReleases::start();
    let archive = b"an archive";
    releases.publish_sums(archive);
    releases.publish(&format!("{}.{}", folder(), extension()), archive);
    let other_key = minisign::KeyPair::generate_unencrypted_keypair().expect("a key pair");
    let machine = Machine::new(&releases, other_key.pk.to_base64());
    let output = machine.update();
    assert!(!output.status.success());
    assert!(stderr(&output).contains("signature"), "{}", stderr(&output));
    machine.assert_not_replaced();
}

#[test]
fn does_not_install_a_release_without_a_signature() {
    let releases = FakeReleases::start();
    let archive = b"an archive";
    let key = releases.publish_sums(archive);
    releases.publish(&format!("{}.{}", folder(), extension()), archive);
    releases
        .files
        .lock()
        .expect("not poisoned")
        .remove("SHA256SUMS.minisig");
    let machine = Machine::new(&releases, key);
    let output = machine.update();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not signed yet"),
        "{}",
        stderr(&output)
    );
    machine.assert_not_replaced();
}
