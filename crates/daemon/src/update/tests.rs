use std::io::Cursor;

use anyhow::anyhow;

use super::*;

fn version(text: &str) -> Version {
    text.parse().expect("a version")
}

#[test]
fn versions_compare_as_numbers() {
    assert!(version("0.10.0") > version("0.9.9"));
    assert!(version("1.0.0") > version("0.99.99"));
    assert_eq!(version("0.3.1").to_string(), "0.3.1");
}

#[test]
fn only_three_numbers_are_a_version() {
    for text in ["", "1", "1.2", "1.2.3.4", "1.2.x", "0.3.0-rc.1", "v0.3.0"] {
        assert!(text.parse::<Version>().is_err(), "{text}");
    }
}

#[test]
fn reads_the_version_from_the_redirect() {
    let url = "https://github.com/fcamus00/clipbroad/releases/tag/v0.3.1";
    assert_eq!(version_in(url).expect("a version"), version("0.3.1"));
    // GitHub sends this when a repo has no release.
    assert!(version_in("https://github.com/fcamus00/clipbroad/releases").is_err());
    assert!(version_in(".../releases/tag/v0.3.0-rc.1").is_err());
}

#[test]
fn a_binary_in_the_cellar_belongs_to_homebrew() {
    assert_eq!(
        Channel::of(Path::new(
            "/opt/homebrew/Cellar/clipbroad/0.2.0/bin/clipbroad"
        )),
        Channel::Homebrew {
            prefix: "/opt/homebrew".into()
        }
    );
}

#[test]
fn any_other_binary_is_direct() {
    for exe in [
        "/home/me/.local/bin/clipbroad",
        "/opt/homebrew/bin/clipbroad",
        "/home/me/Cellar/other/1.0.0/bin/clipbroad",
        r"C:\Users\me\AppData\Local\Programs\clipbroad\clipbroad.exe",
    ] {
        assert_eq!(Channel::of(Path::new(exe)), Channel::Direct, "{exe}");
    }
}

#[test]
fn the_embedded_key_parses() {
    PublicKey::decode(PUBLIC_KEY).expect("packaging/minisign.pub is a public key");
}

#[test]
fn checksum_needs_the_whole_name() {
    let sums = "aaaa  clipbroad-v0.3.1-linux-x86_64.tar.gz\n\
                bbbb  clipbroad-v0.3.1-windows-x86_64.zip\n";
    assert_eq!(
        checksum(sums, "clipbroad-v0.3.1-windows-x86_64.zip"),
        Some("bbbb")
    );
    assert_eq!(checksum(sums, "linux-x86_64.tar.gz"), None);
    assert_eq!(checksum(sums, "clipbroad-v0.3.2-linux-x86_64.tar.gz"), None);
}

#[test]
fn sha256_is_lowercase_hex() {
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// A new key's public half, and a minisign signature file for `data`.
fn signed(data: &[u8]) -> (PublicKey, String) {
    let keys = minisign::KeyPair::generate_unencrypted_keypair().expect("a key pair");
    let public = PublicKey::from_base64(&keys.pk.to_base64()).expect("a public key");
    let signature = minisign::sign(None, &keys.sk, Cursor::new(data), None, None)
        .expect("a signature")
        .to_string();
    (public, signature)
}

#[test]
fn accepts_what_the_key_signed() {
    let (key, signature) = signed(b"sums");
    verify(&key, b"sums", signature.as_bytes()).expect("a valid signature");
}

#[test]
fn rejects_changed_sums() {
    let (key, signature) = signed(b"sums");
    assert!(verify(&key, b"other sums", signature.as_bytes()).is_err());
}

#[test]
fn rejects_another_key() {
    let (_, signature) = signed(b"sums");
    let (other, _) = signed(b"sums");
    assert!(verify(&other, b"sums", signature.as_bytes()).is_err());
}

#[test]
fn rejects_what_is_not_a_signature() {
    let (key, _) = signed(b"sums");
    assert!(verify(&key, b"sums", b"not a signature").is_err());
}

#[test]
fn swap_replaces_the_file_and_leaves_no_copy_on_unix() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (new, exe) = (dir.path().join("new"), dir.path().join("clipbroad.exe"));
    fs::write(&new, "new").expect("write");
    fs::write(&exe, "old").expect("write");
    swap(&new, &exe).expect("swap");
    assert_eq!(fs::read_to_string(&exe).expect("read"), "new");
    assert!(!new.exists());
    assert_eq!(replaced(&exe).exists(), cfg!(windows));
}

/// A Homebrew folder in `dir` whose `brew` logs its arguments, and whose installed binary reports
/// `installed`. It returns the installation and the log.
#[cfg(all(unix, not(feature = "update-smoke")))]
fn fake_homebrew(dir: &Path, installed: &str) -> (Installation, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let script = |path: PathBuf, body: String| {
        fs::create_dir_all(path.parent().expect("a folder")).expect("folder");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
        path
    };
    let log = dir.join("brew.log");
    script(
        dir.join("bin/brew"),
        format!("echo \"$@\" >> '{}'", log.display()),
    );
    let exe = script(
        dir.join("opt/clipbroad/bin/clipbroad"),
        format!("echo 'clipbroad {installed}'"),
    );
    let channel = Channel::Homebrew {
        prefix: dir.to_owned(),
    };
    (Installation { exe, channel }, log)
}

#[cfg(all(unix, not(feature = "update-smoke")))]
#[tokio::test]
async fn homebrew_refreshes_its_taps_before_upgrading() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (installation, log) = fake_homebrew(dir.path(), "9.9.9");
    Updater::new(installation)
        .expect("an updater")
        .install(version("9.9.9"))
        .await
        .expect("the update works");
    assert_eq!(
        fs::read_to_string(log).expect("brew ran"),
        format!("update\nupgrade {HOMEBREW_FORMULA}\n")
    );
}

#[cfg(all(unix, not(feature = "update-smoke")))]
#[tokio::test]
async fn says_when_homebrew_does_not_have_the_version_yet() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (installation, _) = fake_homebrew(dir.path(), "9.9.8");
    let error = Updater::new(installation)
        .expect("an updater")
        .install(version("9.9.9"))
        .await
        .expect_err("the old version is still there");
    assert_eq!(error.to_string(), "Homebrew does not have 9.9.9 yet");
}

fn updates() -> Updates<'static> {
    Updates::new(None)
}

#[test]
fn a_found_version_is_offered() {
    let mut updates = updates();
    let outcome = updates.finish(Finished::Checked {
        asked: false,
        latest: Ok(Some(version("9.9.9"))),
    });
    assert_eq!(outcome, Outcome::default());
    assert_eq!(updates.state(), State::Available(version("9.9.9")));
    assert_eq!(updates.available(), Some(version("9.9.9")));
}

#[test]
fn only_a_person_hears_there_is_nothing_new() {
    let mut updates = updates();
    let scheduled = updates.finish(Finished::Checked {
        asked: false,
        latest: Ok(None),
    });
    assert_eq!(scheduled.note, None);
    let asked = updates.finish(Finished::Checked {
        asked: true,
        latest: Ok(None),
    });
    assert_eq!(
        asked.note,
        Some(format!("clipbroad {VERSION} is up to date"))
    );
    assert_eq!(updates.state(), State::Idle);
}

#[test]
fn only_a_person_hears_a_check_failed() {
    let mut updates = updates();
    let scheduled = updates.finish(Finished::Checked {
        asked: false,
        latest: Err(anyhow!("no network")),
    });
    assert_eq!(scheduled.note, None);
    let asked = updates.finish(Finished::Checked {
        asked: true,
        latest: Err(anyhow!("no network")),
    });
    assert_eq!(
        asked.note.as_deref(),
        Some("Cannot check for updates: no network")
    );
    assert_eq!(updates.state(), State::Idle);
}

#[test]
fn a_failed_scheduled_check_keeps_what_was_found() {
    let mut updates = updates();
    updates.state = State::Available(version("9.9.9"));
    updates.finish(Finished::Checked {
        asked: false,
        latest: Err(anyhow!("no network")),
    });
    assert_eq!(updates.state(), State::Available(version("9.9.9")));
}

#[test]
fn a_finished_install_restarts() {
    let outcome = updates().finish(Finished::Installed {
        version: version("9.9.9"),
        result: Ok(()),
    });
    assert!(outcome.restart);
}

#[test]
fn a_failed_install_says_why_and_can_be_tried_again() {
    let mut updates = updates();
    updates.state = State::Installing(version("9.9.9"));
    let outcome = updates.finish(Finished::Installed {
        version: version("9.9.9"),
        result: Err(anyhow!("9.9.9 is not signed yet")),
    });
    assert!(!outcome.restart);
    assert_eq!(
        outcome.note.as_deref(),
        Some("Update failed: 9.9.9 is not signed yet")
    );
    assert_eq!(updates.state(), State::Available(version("9.9.9")));
}

#[test]
fn without_an_updater_a_click_only_says_so() {
    let mut updates = updates();
    assert!(updates.choose(false).is_some());
    updates.check_on_schedule();
    assert!(updates.task.is_none());
    assert_eq!(updates.state(), State::Idle);
}
