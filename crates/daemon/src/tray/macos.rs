//! macOS: the icon is an `NSStatusItem`, which needs an `NSApplication` whose events the main
//! thread handles.
//!
//! The binary has no app bundle, so the activation policy is set by hand: `Accessory` gives no
//! Dock icon and no menu bar of its own, as `LSUIElement` would in a bundle. Instead of
//! `NSApplication::run`, which can only be stopped from the main thread, [`EventLoop::run`] pumps
//! the events itself and checks every [`POLL_INTERVAL`] whether the daemon has ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_foundation::{MainThreadMarker, NSDate, ns_string};

/// How long a Ctrl+C or a "Quit" waits at most for the main thread to notice.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

pub struct EventLoop {
    app: Retained<NSApplication>,
    done: Arc<AtomicBool>,
}

impl EventLoop {
    pub fn new() -> Result<Self> {
        let main =
            MainThreadMarker::new().context("the tray icon must start on the main thread")?;
        let app = NSApplication::sharedApplication(main);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        // What `run` would do first. `tray-icon` asks for it before adding the icon.
        app.finishLaunching();
        Ok(Self {
            app,
            done: Arc::default(),
        })
    }

    pub fn waker(&self) -> Waker {
        Waker {
            done: Arc::clone(&self.done),
        }
    }

    /// Handles the app's events (clicks on the icon, its menu) until [`Waker::wake`].
    pub fn run(&self) {
        // The value of `NSDefaultRunLoopMode`. Run loop modes are matched by name, and reading
        // the extern static would need `unsafe`.
        let mode = ns_string!("kCFRunLoopDefaultMode");
        while !self.done.load(Ordering::Acquire) {
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

pub struct Waker {
    done: Arc<AtomicBool>,
}

impl Waker {
    /// Ends [`EventLoop::run`] from any thread, within [`POLL_INTERVAL`].
    pub fn wake(&self) {
        self.done.store(true, Ordering::Release);
    }
}

/// The menu bar is always there.
pub fn check_host() {}
