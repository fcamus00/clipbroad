//! Windows: `tray-icon` adds the icon through a hidden window, whose messages (clicks, the menu,
//! Explorer restarting) arrive through the message loop of the thread that created it.

use std::ptr;

use anyhow::Result;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_QUIT,
};

pub struct EventLoop {
    thread: u32,
}

impl EventLoop {
    // Win32 FFI. It never fails here, but keeps the signature every platform shares (it can fail
    // on macOS).
    #[allow(unsafe_code, clippy::unnecessary_wraps)]
    pub fn new() -> Result<Self> {
        // SAFETY: takes nothing and cannot fail.
        let thread = unsafe { GetCurrentThreadId() };
        Ok(Self { thread })
    }

    pub fn waker(&self) -> Waker {
        Waker {
            thread: self.thread,
        }
    }

    /// Dispatches this thread's messages until [`Waker::wake`] posts `WM_QUIT`.
    // Win32 FFI. It takes `self` like on the other platforms, though the loop is just this
    // thread's.
    #[allow(unsafe_code, clippy::unused_self)]
    pub fn run(&self) {
        let mut message = MSG::default();
        loop {
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

pub struct Waker {
    thread: u32,
}

impl Waker {
    /// Ends [`EventLoop::run`] from any thread.
    #[allow(unsafe_code)] // Win32 FFI.
    pub fn wake(&self) {
        // SAFETY: plain values. It fails only if the thread is gone or has no message queue, and
        // then there is no loop to end.
        unsafe { PostThreadMessageW(self.thread, WM_QUIT, 0, 0) };
    }
}

/// Explorer hosts the icons. If it is not up yet at login, `tray-icon` adds the icon once it is.
pub fn check_host() {}
