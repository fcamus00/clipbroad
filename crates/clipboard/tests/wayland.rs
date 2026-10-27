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
use clipbroad_clipboard::{Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};
use clipbroad_core::Png;
use wl_clipboard_rs::copy;

const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

/// PNG signature and `IHDR` chunk header: what `Png::try_from` checks.
const PNG_START: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";

/// Copies `text` as `text_mime` and `png` as `image/png`, as one copy.
fn copy_both(text_mime: &str, text: &str, png: &Png) {
    let source = |bytes: &[u8], mime: &str| copy::MimeSource {
        source: copy::Source::Bytes(bytes.into()),
        mime_type: copy::MimeType::Specific(mime.to_owned()),
    };
    let mut options = copy::Options::new();
    // Offer exactly these two types, like a browser does.
    options.omit_additional_text_mime_types(true);
    options
        .copy_multi(vec![
            source(text.as_bytes(), text_mime),
            source(png.as_bytes(), "image/png"),
        ])
        .expect("copy text and image");
}

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
    assert_eq!(
        clipboard.read_text().expect("read empty").as_deref(),
        Some("")
    );

    // What `wl-copy --sensitive` and KeePassXC offer: the text plus `x-kde-passwordManagerHint`.
    let mut sensitive = copy::Options::new();
    sensitive.sensitive(true);
    sensitive
        .clone()
        .copy(
            copy::Source::Bytes(b"hunter2"[..].into()),
            copy::MimeType::Text,
        )
        .expect("copy sensitive");
    let change = next_change(&mut watch).await;
    assert!(change.has_format("x-kde-passwordManagerHint"), "{change:?}");
    assert!(
        matches!(
            clipboard.read_text(),
            Err(ClipboardError::Sensitive {
                marker: "x-kde-passwordManagerHint"
            })
        ),
        "sensitive text was read"
    );

    // Bigger than a pipe buffer, so it takes many reads. Only the start has to look like a PNG.
    let mut bytes = PNG_START.to_vec();
    bytes.extend((0..300_000_u32).map(|i| i.to_le_bytes()[0]));
    let png = Png::try_from(bytes).expect("starts like a PNG");
    clipboard.write_image(&png).expect("write image");
    let change = next_change(&mut watch).await;
    assert_eq!(change.formats, ["image/png"]);
    // Byte for byte, or anti-echo would see our own write as a new copy.
    assert_eq!(
        clipboard.read_image().expect("read image"),
        Some(png.clone())
    );
    assert_eq!(clipboard.read_text().expect("read text"), None);

    // A browser's "Copy image": the image plus `<img>` markup. The image wins.
    copy_both("text/html", "<img src=\"x.png\">", &png);
    next_change(&mut watch).await;
    assert_eq!(clipboard.read_text().expect("read text"), None);
    assert!(clipboard.read_image().expect("read image").is_some());

    // Plain text next to an image (a spreadsheet's cells) is still text.
    copy_both("text/plain;charset=utf-8", "a\tb", &png);
    next_change(&mut watch).await;
    assert_eq!(
        clipboard.read_text().expect("read text").as_deref(),
        Some("a\tb")
    );

    // A sensitive marker hides the image too.
    sensitive
        .copy(
            copy::Source::Bytes(png.as_bytes().into()),
            copy::MimeType::Specific("image/png".to_owned()),
        )
        .expect("copy sensitive image");
    next_change(&mut watch).await;
    assert!(
        matches!(
            clipboard.read_image(),
            Err(ClipboardError::Sensitive { .. })
        ),
        "sensitive image was read"
    );

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
