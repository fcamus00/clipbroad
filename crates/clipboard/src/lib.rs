//! OS clipboard access: one `Clipboard` trait with a backend per platform
//! (Wayland, X11, macOS, Windows) plus an in-memory fake for tests.
