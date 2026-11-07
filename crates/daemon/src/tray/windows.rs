//! Windows: `tray-icon` adds the icon through a hidden window, whose messages (clicks, the menu,
//! Explorer restarting) arrive through the message loop of the thread that created it. A timer
//! wakes that loop, so it gets to draw new views at least every [`POLL_INTERVAL`]. A message from
//! another thread would not do: an open menu runs its own loop, which throws those away.

use std::ptr;
use std::sync::mpsc::Receiver;

use anyhow::Result;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, SetTimer, TranslateMessage,
};

use super::{POLL_INTERVAL, draw_arrived};
use crate::menu::View;

pub struct EventLoop;

impl EventLoop {
    // It cannot fail here, but can on macOS.
    #[allow(clippy::unnecessary_wraps)]
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    /// Dispatches this thread's messages and draws the views that arrived after each one, and at
    /// least every [`POLL_INTERVAL`], until the daemon has ended.
    // Win32 FFI. It keeps `self` like on the other platforms, though the loop is just this
    // thread's.
    #[allow(unsafe_code, clippy::unused_self)]
    pub fn run(&self, views: &Receiver<View>, mut draw: impl FnMut(&View)) {
        let interval = u32::try_from(POLL_INTERVAL.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: plain values. No window and no callback mean it posts WM_TIMER to this thread,
        // and the process ends soon after this loop, so the timer needs no cleanup.
        unsafe { SetTimer(ptr::null_mut(), 0, interval, None) };
        let mut message = MSG::default();
        while draw_arrived(views, &mut draw) {
            // SAFETY: `message` is a valid MSG to write to. A null window means every message of
            // this thread. Returns 0 for WM_QUIT and -1 on an error (invalid arguments, none here).
            let got = unsafe { GetMessageW(&raw mut message, ptr::null_mut(), 0, 0) };
            if got == 0 || got == -1 {
                break;
            }
            // SAFETY: `message` was just filled in by GetMessageW.
            unsafe {
                TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }
}

/// Explorer hosts the icons. If it is not up yet at login, `tray-icon` adds the icon once it is.
pub fn check_host() {}
