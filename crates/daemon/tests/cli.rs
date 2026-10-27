//! Runs the real `clipbroad` binary. Cargo builds it and passes its path in
//! `CARGO_BIN_EXE_clipbroad`, so no extra test crate is needed.

use std::process::{Command, Output};

fn clipbroad(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clipbroad"))
        .args(args)
        .output()
        .expect("the clipbroad binary should start")
}

fn stdout(output: Output) -> String {
    assert!(output.status.success(), "clipbroad failed: {output:?}");
    String::from_utf8(output.stdout).expect("stdout should be UTF-8")
}

#[test]
fn watch_fake_prints_one_line_per_change() {
    let stdout = stdout(clipbroad(&["watch", "--fake"]));

    // Drop the time column, e.g. "17:33:07 UTC  ".
    let lines: Vec<&str> = stdout
        .lines()
        .map(|line| {
            line.split_once(" UTC  ")
                .expect("line starts with the time")
                .1
        })
        .collect();
    assert_eq!(
        lines,
        [
            r#"[text/plain;charset=utf-8]  "hello from clipbroad" (20 chars)"#,
            r#"[text/plain;charset=utf-8]  "two lines\nand a\ttab" (19 chars)"#,
            r#"[text/plain;charset=utf-8]  "A long text with multibyte characters (ñandú, 🦀) shows that the preview is cut o"… (114 chars)"#,
            "[text/plain;charset=utf-8, x-kde-passwordManagerHint]  (sensitive: x-kde-passwordManagerHint, not read or synced)",
            "[image/png]  (no text)",
            "[]  (clipboard empty)",
        ]
    );
}

#[test]
fn help_describes_commands_and_hides_fake() {
    let help = stdout(clipbroad(&["--help"]));
    assert!(
        help.contains("watch  Print every change of the local clipboard"),
        "{help}"
    );

    assert!(help.contains("run    Sync the clipboard"), "{help}");

    let run_help = stdout(clipbroad(&["run", "--help"]));
    assert!(run_help.contains("--config <FILE>"), "{run_help}");
    assert!(run_help.contains("password manager"), "{run_help}");
    assert!(!run_help.contains("--fake"), "{run_help}");

    let watch_help = stdout(clipbroad(&["watch", "--help"]));
    assert!(watch_help.contains("preview of its text"), "{watch_help}");
    assert!(watch_help.contains("sensitive"), "{watch_help}");
    assert!(!watch_help.contains("--fake"), "{watch_help}");
}
