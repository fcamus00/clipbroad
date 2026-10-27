//! `clipbroad watch`: print every change of the local clipboard.

use std::future::Future;
use std::io::{self, Write};
use std::pin::pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clipbroad_clipboard::Clipboard;
use clipbroad_clipboard::fake::{FakeClipboard, TEXT_FORMAT};

use crate::backend;

/// Text previews are cut to this many characters.
const PREVIEW_CHARS: usize = 80;

/// What `--fake` copies, as `(text, formats)`: one entry per kind of output line.
const FAKE_COPIES: [(Option<&str>, &[&str]); 5] = [
    (Some("hello from clipbroad"), &[TEXT_FORMAT]),
    (Some("two lines\nand a\ttab"), &[TEXT_FORMAT]),
    (
        Some(
            "A long text with multibyte characters (ñandú, 🦀) shows that the preview is cut on \
             character boundaries, not bytes.",
        ),
        &[TEXT_FORMAT],
    ),
    (None, &["image/png"]),
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
            // `read_text` may block (up to 2 s by contract), so keep it off the async thread.
            match tokio::task::spawn_blocking(move || clipboard.read_text()).await? {
                Ok(Some(text)) => preview(&text),
                Ok(None) => "(no text)".to_owned(),
                Err(error) => format!("(cannot read text: {error})"),
            }
        };
        writeln!(
            out,
            "{}  [{}]  {content}",
            utc_time(SystemTime::now()),
            change.formats.join(", ")
        )?;
    }
}

/// Copies each of [`FAKE_COPIES`], handing control back to [`watch`] after each one so it gets
/// printed before the next.
async fn simulate_copies(fake: FakeClipboard) {
    for (text, formats) in FAKE_COPIES {
        fake.simulate_copy(
            text.map(str::to_owned),
            formats.iter().map(|&format| format.to_owned()).collect(),
        );
        tokio::task::yield_now().await;
    }
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
fn utc_time(now: SystemTime) -> String {
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
    fn utc_time_is_hours_minutes_seconds() {
        let time = UNIX_EPOCH + Duration::from_secs(3 * 86_400 + 13 * 3600 + 5 * 60 + 9);
        assert_eq!(utc_time(time), "13:05:09 UTC");
    }
}
