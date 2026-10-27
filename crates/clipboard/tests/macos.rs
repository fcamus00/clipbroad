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

use std::io::Cursor;
use std::time::Duration;

use clipbroad_clipboard::macos::{AccessBehavior, MacosClipboard};
use clipbroad_clipboard::{Clipboard, ClipboardChange, ClipboardError, ClipboardWatch};
use clipbroad_core::Png;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSBitmapImageRep, NSPasteboard};
use objc2_foundation::{NSArray, NSData, NSString};

/// A few polling ticks.
const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

async fn next_change(watch: &mut ClipboardWatch) -> ClipboardChange {
    tokio::time::timeout(EVENT_TIMEOUT, watch.recv())
        .await
        .expect("no change event in time")
        .expect("watch ended")
        .expect("watch failed")
}

const CONCEALED: &str = "org.nspasteboard.ConcealedType";

/// Copies these types and their data as one copy, the way nspasteboard.org asks password managers
/// to: declare every type first, then write the data.
#[allow(unsafe_code)] // `declareTypes:owner:` is unsafe only because of the owner's type.
fn copy_types(types: &[(&str, &[u8])]) {
    autoreleasepool(|_| {
        let pasteboard = NSPasteboard::generalPasteboard();
        let names: Vec<_> = types
            .iter()
            .map(|(name, _)| NSString::from_str(name))
            .collect();
        let names: Vec<&NSString> = names.iter().map(AsRef::as_ref).collect();
        // SAFETY: no owner, so there is no owner type to get wrong.
        unsafe { pasteboard.declareTypes_owner(&NSArray::from_slice(&names), None) };
        for (name, (_, data)) in names.iter().zip(types) {
            assert!(pasteboard.setData_forType(Some(&NSData::with_bytes(data)), name));
        }
    });
}

/// A 2x1 RGB PNG: a red and a green pixel.
fn small_png() -> Png {
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, 2, 1);
    encoder.set_color(png::ColorType::Rgb);
    let mut writer = encoder.write_header().expect("PNG header");
    writer
        .write_image_data(&[255, 0, 0, 0, 255, 0])
        .expect("PNG data");
    writer.finish().expect("PNG end");
    Png::try_from(png).expect("a PNG")
}

/// The width and height of a PNG.
fn png_size(png: &Png) -> (u32, u32) {
    let reader = png::Decoder::new(Cursor::new(png.as_bytes()))
        .read_info()
        .expect("a valid PNG");
    let info = reader.info();
    (info.width, info.height)
}

/// `png` as TIFF, the way `AppKit` encodes it.
fn tiff_of(png: &Png) -> Vec<u8> {
    autoreleasepool(|_| {
        let bitmap = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(png.as_bytes()))
            .expect("AppKit reads the PNG");
        bitmap.TIFFRepresentation().expect("TIFF").to_vec()
    })
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

    copy_types(&[("public.utf8-plain-text", b"hunter2"), (CONCEALED, b"")]);
    let change = next_change(&mut watch).await;
    assert!(change.has_format(CONCEALED), "{change:?}");
    assert!(
        matches!(
            clipboard.read_text(),
            Err(ClipboardError::Sensitive { marker: CONCEALED })
        ),
        "concealed text was read"
    );

    check_images(&clipboard, &mut watch).await;
}

/// The image part of the smoke test.
async fn check_images(clipboard: &MacosClipboard, watch: &mut ClipboardWatch) {
    // An image we write reads back byte for byte.
    let png = small_png();
    clipboard.write_image(&png).expect("write image");
    let change = next_change(watch).await;
    assert!(change.has_format("public.png"), "{change:?}");
    assert_eq!(
        clipboard.read_image().expect("read image"),
        Some(png.clone())
    );
    assert_eq!(clipboard.read_text().expect("read text"), None);

    // A browser's "Copy image": the image plus `<img>` markup. The image wins.
    copy_types(&[
        ("public.html", b"<img src=\"x.png\">"),
        ("public.png", png.as_bytes()),
    ]);
    next_change(watch).await;
    assert_eq!(clipboard.read_text().expect("read text"), None);
    assert_eq!(
        clipboard.read_image().expect("read image"),
        Some(png.clone())
    );

    // Plain text next to an image (a spreadsheet's cells) is still text.
    copy_types(&[
        ("public.utf8-plain-text", b"a\tb"),
        ("public.png", png.as_bytes()),
    ]);
    next_change(watch).await;
    assert_eq!(
        clipboard.read_text().expect("read text").as_deref(),
        Some("a\tb")
    );

    // TIFF only (some apps, e.g. Preview): converted to PNG.
    copy_types(&[("public.tiff", &tiff_of(&png))]);
    next_change(watch).await;
    let converted = clipboard
        .read_image()
        .expect("read TIFF")
        .expect("TIFF read as an image");
    assert_eq!(png_size(&converted), (2, 1));

    // A sensitive marker hides the image too.
    copy_types(&[("public.png", png.as_bytes()), (CONCEALED, b"")]);
    next_change(watch).await;
    assert!(
        matches!(
            clipboard.read_image(),
            Err(ClipboardError::Sensitive { marker: CONCEALED })
        ),
        "concealed image was read"
    );
}
