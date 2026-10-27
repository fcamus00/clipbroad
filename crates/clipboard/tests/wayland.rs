//! Smoke test of the Wayland backend against the real clipboard. Needs a Wayland session with a
//! data-control protocol, so it is ignored by default (CI has none). Run it locally with:
//!
//! ```sh
//! cargo nextest run -p clipbroad-clipboard --test wayland --run-ignored only
//! ```
//!
//! It overwrites the clipboard. One test on purpose: the clipboard is global, so parallel tests
//! would see each other's changes.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use clipbroad_clipboard::wayland::WaylandClipboard;
use clipbroad_clipboard::{Clipboard, ClipboardChange, ClipboardWatch};

const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

async fn next_change(watch: &mut ClipboardWatch) -> ClipboardChange {
    tokio::time::timeout(EVENT_TIMEOUT, watch.recv())
        .await
        .expect("no change event in time")
        .expect("watch ended")
        .expect("watch failed")
}

/// Names of this process's threads that belong to a clipboard watch.
fn watch_threads() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .expect("list threads")
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .map(|name| name.trim_end().to_owned())
        .filter(|name| name.starts_with("wl-watch"))
        .collect()
}

#[tokio::test]
#[ignore = "needs a Wayland session; overwrites the clipboard"]
async fn real_clipboard_round_trip_and_watch() {
    let clipboard = WaylandClipboard::new().expect("Wayland session");

    clipboard
        .write_text("before watching")
        .expect("write before watching");
    let mut watch = clipboard.watch().expect("start watching");
    assert_eq!(watch_threads().len(), 2, "{:?}", watch_threads());

    let early = tokio::time::timeout(Duration::from_millis(300), watch.recv()).await;
    assert!(
        early.is_err(),
        "content present before watching was reported: {early:?}"
    );

    let text = "línea 1\r\nlínea 2 \u{1F600}\n";
    clipboard.write_text(text).expect("write");
    let change = next_change(&mut watch).await;
    assert!(change.has_format("text/plain;charset=utf-8"), "{change:?}");
    assert_eq!(clipboard.read_text().expect("read").as_deref(), Some(text));

    clipboard.write_text("").expect("write empty");
    next_change(&mut watch).await;
    assert_eq!(clipboard.read_text().expect("read empty"), None);

    drop(watch);
    let deadline = Instant::now() + Duration::from_secs(1);
    while !watch_threads().is_empty() {
        assert!(
            Instant::now() < deadline,
            "watch threads still running: {:?}",
            watch_threads()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
