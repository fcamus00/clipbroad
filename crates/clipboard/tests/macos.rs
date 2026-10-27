//! Smoke test of the macOS backend against the real general pasteboard. GitHub's macOS runners
//! have a logged-in session where it works, so CI runs it. Run it on a Mac with:
//!
//! ```sh
//! cargo nextest run -p clipbroad-clipboard --test macos
//! ```
//!
//! It overwrites the clipboard. One test on purpose: the clipboard is global, so parallel tests
//! would see each other's changes.

#![cfg(target_os = "macos")]

use std::time::Duration;

use clipbroad_clipboard::macos::{AccessBehavior, MacosClipboard};
use clipbroad_clipboard::{Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSArray, NSString, ns_string};

/// A few polling ticks.
const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

async fn next_change(watch: &mut ClipboardWatch) -> ClipboardChange {
    tokio::time::timeout(EVENT_TIMEOUT, watch.recv())
        .await
        .expect("no change event in time")
        .expect("watch ended")
        .expect("watch failed")
}

/// Copies `text` marked as concealed, the way nspasteboard.org asks password managers to: declare
/// every type first, then write the data.
#[allow(unsafe_code)] // `declareTypes:owner:` is unsafe only because of the owner's type.
fn copy_concealed(text: &str) {
    autoreleasepool(|_| {
        let pasteboard = NSPasteboard::generalPasteboard();
        let text_type = ns_string!("public.utf8-plain-text");
        let concealed = ns_string!("org.nspasteboard.ConcealedType");
        // SAFETY: no owner, so there is no owner type to get wrong.
        unsafe {
            pasteboard.declareTypes_owner(&NSArray::from_slice(&[text_type, concealed]), None)
        };
        let text = NSString::from_str(text);
        assert!(pasteboard.setString_forType(&text, text_type));
        assert!(pasteboard.setString_forType(ns_string!(""), concealed));
    });
}

#[tokio::test]
async fn real_clipboard_round_trip_and_watch() {
    let clipboard = MacosClipboard;
    assert_ne!(
        clipboard.access_behavior(),
        Some(AccessBehavior::AlwaysDeny),
        "pasteboard access is denied for this process"
    );

    clipboard
        .write_text("before watching")
        .expect("write before watching");
    let mut watch = clipboard.watch().expect("start watching");

    // Two polling ticks.
    let early = tokio::time::timeout(Duration::from_millis(700), watch.recv()).await;
    assert!(
        early.is_err(),
        "content present before watching was reported: {early:?}"
    );

    let text = "línea 1\r\nlínea 2 \u{1F600}\n";
    clipboard.write_text(text).expect("write");
    let change = next_change(&mut watch).await;
    assert!(change.has_format("public.utf8-plain-text"), "{change:?}");
    assert_eq!(clipboard.read_text().expect("read").as_deref(), Some(text));

    clipboard.write_text("").expect("write empty");
    next_change(&mut watch).await;
    assert_eq!(
        clipboard.read_text().expect("read empty").as_deref(),
        Some("")
    );

    copy_concealed("hunter2");
    let change = next_change(&mut watch).await;
    assert!(
        change.has_format("org.nspasteboard.ConcealedType"),
        "{change:?}"
    );
    assert!(
        matches!(
            clipboard.read_text(),
            Err(ClipboardError::Sensitive {
                marker: "org.nspasteboard.ConcealedType"
            })
        ),
        "concealed text was read"
    );
}
