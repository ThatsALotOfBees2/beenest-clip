//! Global save hotkey. `run` must be called on the main thread and blocks.

use std::str::FromStr;

use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

pub fn run(spec: &str, on_press: impl Fn() + Send + 'static) -> Result<()> {
    let hotkey = HotKey::from_str(spec).with_context(|| format!("invalid hotkey `{spec}`"))?;
    let manager = GlobalHotKeyManager::new().context("global hotkeys are unavailable")?;
    manager
        .register(hotkey)
        .with_context(|| format!("could not register hotkey `{spec}` (already in use?)"))?;

    std::thread::spawn(move || {
        for event in GlobalHotKeyEvent::receiver() {
            if event.state == HotKeyState::Pressed {
                on_press();
            }
        }
    });

    pump();
    drop(manager);
    Ok(())
}

/// Windows delivers hotkeys as window messages to the registering thread.
#[cfg(windows)]
fn pump() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// macOS delivers hotkeys through the main thread's run loop.
#[cfg(target_os = "macos")]
fn pump() {
    core_foundation::runloop::CFRunLoop::run_current();
}

/// On X11 the hotkey library listens on its own thread.
#[cfg(not(any(windows, target_os = "macos")))]
fn pump() {
    loop {
        std::thread::park();
    }
}
