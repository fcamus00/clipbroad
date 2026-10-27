//! `clipbroad watch`: print every change of the local clipboard.

use std::fs;
use std::future::Future;
use std::io::{self, Write};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clipbroad_clipboard::fake::{self, FakeClipboard, TEXT_FORMAT};
use clipbroad_clipboard::{Clipboard, ClipboardError};

use crate::backend;
use crate::run::{Copied, read_content};

/// Text previews are cut to this many characters.
const PREVIEW_CHARS: usize = 80;

/// What `--fake` copies, as `(text, formats)`: one entry per kind of output line. A PNG image
/// follows them.
const FAKE_COPIES: [(Option<&str>, &[&str]); 6] = [
    (Some("hello from clipbroad"), &[TEXT_FORMAT]),
    (Some("two lines\nand a\ttab"), &[TEXT_FORMAT]),
    (
        Some(
            "A long text with multibyte characters (ñandú, 🦀) shows that the preview is cut on \
             character boundaries, not bytes.",
        ),
        &[TEXT_FORMAT],
    ),
    (Some("hunter2"), &[TEXT_FORMAT, "x-kde-passwordManagerHint"]),
    // An image type that is not PNG.
    (None, &["image/jpeg"]),
    (None, &[]),
];

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Use an in-memory clipboard that simulates a few copies, then exit. For tests while the
    /// real backends do not exist.
    #[arg(long, hide = true)]
    fake: bool,
}

pub async fn run(args: Args) -> Result<()> {
    if args.fake {
        let fake = FakeClipboard::new();
        let simulation = simulate_copies(fake.clone());
        let stop = async {
            simulation.await;
            Ok(())
        };
        watch(Arc::new(fake), stop, &mut io::stdout()).await
    } else {
        watch(
            backend::system_clipboard()?,
            tokio::signal::ctrl_c(),
            &mut io::stdout(),
        )
        .await
    }
}

/// Prints one line per clipboard change until `stop` completes (Ctrl+C, or the end of the
/// `--fake` simulation).
async fn watch(
    clipboard: Arc<dyn Clipboard>,
    stop: impl Future<Output = io::Result<()>>,
    out: &mut impl Write,
) -> Result<()> {
    let mut changes = clipboard.watch().context("cannot watch the clipboard")?;
    tracing::info!("watching the clipboard, press Ctrl+C to stop");
    let mut stop = pin!(stop);
    loop {
        let change = tokio::select! {
            // Check changes before `stop`. `stop` only runs while this loop waits here, so the
            // `--fake` simulation cannot copy again before the previous change is printed.
            biased;
            change = changes.recv() => change,
            result = &mut stop => return result.context("cannot listen for Ctrl+C"),
        };
        let change = change
            .context("the clipboard watcher stopped")?
            .context("the clipboard watcher failed")?;
        tracing::debug!(?change, "clipboard changed");

        let content = if change.formats.is_empty() {
            "(clipboard empty)".to_owned()
        } else {
            let clipboard = Arc::clone(&clipboard);
            // Reads may block (up to 2 s each by contract), and so may looking up file sizes:
            // keep them off the async thread.
            tokio::task::spawn_blocking(move || describe(read_content(&*clipboard))).await?
        };
        writeln!(
            out,
            "{}  [{}]  {content}",
            utc_time(SystemTime::now()),
            change.formats.join(", ")
        )?;
    }
}

/// What a change holds, for its output line.
fn describe(content: Result<Option<Copied>, ClipboardError>) -> String {
    match content {
        Ok(Some(Copied::Files(paths))) => describe_files(&paths),
        Ok(Some(Copied::Text(text))) => preview(&text),
        Ok(Some(Copied::Image(png))) => format!("(image/png, {} bytes)", png.as_bytes().len()),
        Ok(None) => "(no files, text or PNG image)".to_owned(),
        Err(ClipboardError::Sensitive { marker }) => {
            format!("(sensitive: {marker}, not read or synced)")
        }
        Err(error) => format!("(cannot read: {error})"),
    }
}

/// `(3 files, 41.9 MB)`, with sizes from the file system. Folders and paths that cannot be read
/// are counted apart: `run` does not sync them.
fn describe_files(paths: &[PathBuf]) -> String {
    let sizes: Vec<u64> = paths
        .iter()
        .filter_map(|path| fs::metadata(path).ok().filter(fs::Metadata::is_file))
        .map(|meta| meta.len())
        .collect();
    let skipped = match paths.len() - sizes.len() {
        0 => String::new(),
        n => format!(", {n} not synced (folders or unreadable)"),
    };
    format!(
        "({}{skipped})",
        files_summary(sizes.len(), sizes.iter().sum())
    )
}

/// `3 files, 41.9 MB`.
pub fn files_summary(count: usize, bytes: u64) -> String {
    let noun = if count == 1 { "file" } else { "files" };
    format!("{count} {noun}, {}", human_size(bytes))
}

/// `512 bytes`, `41.9 MB`: decimal units, one decimal digit, rounded.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    let mut scale = 1_u128;
    let mut unit = None;
    for name in UNITS {
        if u128::from(bytes) < scale * 1000 {
            break;
        }
        scale *= 1000;
        unit = Some(name);
    }
    let Some(unit) = unit else {
        return format!("{bytes} bytes");
    };
    let tenths = (u128::from(bytes) * 10 + scale / 2) / scale;
    format!("{}.{} {unit}", tenths / 10, tenths % 10)
}

/// Copies each of [`FAKE_COPIES`] and then an image, handing control back to [`watch`] after
/// each one so it gets printed before the next.
async fn simulate_copies(fake: FakeClipboard) {
    for (text, formats) in FAKE_COPIES {
        fake.simulate_copy(
            text.map(str::to_owned),
            formats.iter().map(|&format| format.to_owned()).collect(),
        );
        tokio::task::yield_now().await;
    }
    fake.simulate_copy_image(fake::tiny_png());
    tokio::task::yield_now().await;
}

/// The text quoted and escaped (so one change stays on one line), cut to [`PREVIEW_CHARS`]
/// characters, followed by its full length.
fn preview(text: &str) -> String {
    let (head, ellipsis) = match text.char_indices().nth(PREVIEW_CHARS) {
        Some((end, _)) => (&text[..end], "…"),
        None => (text, ""),
    };
    format!("{head:?}{ellipsis} ({} chars)", text.chars().count())
}

/// `HH:MM:SS UTC`. Local time would need a time zone database, a new dependency just for this.
pub fn utc_time(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    format!(
        "{:02}:{:02}:{:02} UTC",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn preview_cuts_on_char_boundaries() {
        let text = "ñ".repeat(PREVIEW_CHARS + 1);
        let expected = format!("\"{}\"… (81 chars)", "ñ".repeat(PREVIEW_CHARS));
        assert_eq!(preview(&text), expected);
    }

    #[test]
    fn preview_keeps_text_at_the_limit_whole() {
        let text = "a".repeat(PREVIEW_CHARS);
        assert_eq!(preview(&text), format!("\"{text}\" (80 chars)"));
    }

    #[test]
    fn preview_escapes_control_characters() {
        assert_eq!(preview("a\r\n\u{1b}[31m"), r#""a\r\n\u{1b}[31m" (8 chars)"#);
    }

    #[test]
    fn human_size_uses_decimal_units() {
        let cases = [
            (0, "0 bytes"),
            (999, "999 bytes"),
            (1000, "1.0 kB"),
            (41_949_999, "41.9 MB"),
            (41_950_000, "42.0 MB"),
            (2 * 1024 * 1024 * 1024, "2.1 GB"),
            (u64::MAX, "18446.7 PB"),
        ];
        for (bytes, shown) in cases {
            assert_eq!(human_size(bytes), shown);
        }
    }

    #[test]
    fn files_show_their_count_and_size_and_what_is_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("a b.txt");
        fs::write(&file, vec![0; 1500]).expect("write");
        assert_eq!(
            describe_files(std::slice::from_ref(&file)),
            "(1 file, 1.5 kB)"
        );
        let paths = [
            file.clone(),
            file,
            dir.path().to_owned(),
            dir.path().join("gone"),
        ];
        assert_eq!(
            describe_files(&paths),
            "(2 files, 3.0 kB, 2 not synced (folders or unreadable))"
        );
    }

    #[test]
    fn utc_time_is_hours_minutes_seconds() {
        let time = UNIX_EPOCH + Duration::from_secs(3 * 86_400 + 13 * 3600 + 5 * 60 + 9);
        assert_eq!(utc_time(time), "13:05:09 UTC");
    }
}
