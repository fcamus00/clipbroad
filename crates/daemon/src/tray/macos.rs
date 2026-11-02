//! macOS: the icon is an `NSStatusItem`, which needs an `NSApplication` whose events the main
//! thread handles.
//!
//! The binary has no app bundle, so the activation policy is set by hand: `Accessory` gives no
//! Dock icon and no menu bar of its own, as `LSUIElement` would in a bundle. Instead of
//! `NSApplication::run`, which can only be stopped from the main thread, [`EventLoop::run`] pumps
//! the events itself, so it gets to run its update at least every [`POLL_INTERVAL`].

use anyhow::{Context, Result};
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_foundation::{MainThreadMarker, NSDate, ns_string};

use super::POLL_INTERVAL;

pub struct EventLoop {
    app: Retained<NSApplication>,
}

impl EventLoop {
    pub fn new() -> Result<Self> {
        let main =
            MainThreadMarker::new().context("the tray icon must start on the main thread")?;
        let app = NSApplication::sharedApplication(main);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        // What `run` would do first. `tray-icon` asks for it before adding the icon.
        app.finishLaunching();
        Ok(Self { app })
    }

    /// Handles the app's events (clicks on the icon, its menu) and calls `update` after each one,
    /// and at least every [`POLL_INTERVAL`], until it returns false.
    pub fn run(&self, mut update: impl FnMut() -> bool) {
        // The value of `NSDefaultRunLoopMode`. Run loop modes are matched by name, and reading
        // the extern static would need `unsafe`.
        let mode = ns_string!("kCFRunLoopDefaultMode");
        while update() {
            autoreleasepool(|_| {
                let until = NSDate::dateWithTimeIntervalSinceNow(POLL_INTERVAL.as_secs_f64());
                if let Some(event) = self.app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&until),
                    mode,
                    true,
                ) {
                    self.app.sendEvent(&event);
                }
            });
        }
    }
}

/// The menu bar is always there.
pub fn check_host() {}
