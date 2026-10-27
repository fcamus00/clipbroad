//! Manual check of the Wayland backend against the real clipboard.
//!
//! ```sh
//! cargo run -p clipbroad-clipboard --example wayland -- watch         # print every change
//! cargo run -p clipbroad-clipboard --example wayland -- read          # print the current text
//! cargo run -p clipbroad-clipboard --example wayland -- write "hola"  # copy, serve until replaced
//! ```

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use clipbroad_clipboard::Clipboard;
    use clipbroad_clipboard::wayland::WaylandClipboard;

    let clipboard = WaylandClipboard::new()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["watch"] => {
            let mut watch = clipboard.watch()?;
            println!("watching, copy something (Ctrl-C to stop)");
            while let Some(change) = watch.recv().await {
                let change = change?;
                println!("change: formats={:?}", change.formats);
                println!("  text: {:?}", clipboard.read_text()?);
            }
        }
        ["read"] => println!("{:?}", clipboard.read_text()?),
        ["write", text] => {
            // On Wayland the content is served by this process, so stay alive until another copy
            // replaces it: the first change is our own write, the second is someone else's.
            let mut watch = clipboard.watch()?;
            clipboard.write_text(text)?;
            for label in ["own write", "replaced by another copy"] {
                let change = watch.recv().await.ok_or("watch ended")??;
                println!("{label}: formats={:?}", change.formats);
            }
        }
        _ => return Err("usage: wayland watch | read | write <text>".into()),
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the Wayland backend only exists on Linux");
}
