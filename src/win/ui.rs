//! Small Win32 UI helpers.

use std::ptr;

use windows_sys::Win32::UI::WindowsAndMessaging::{MB_SETFOREGROUND, MESSAGEBOX_STYLE, MessageBoxW};

/// NUL-terminated UTF-16 for Win32 string parameters.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Modal message box with the TapClip caption and its own taskbar button.
pub fn message_box(text: &str, style: MESSAGEBOX_STYLE) {
    let (text, caption) = (wide(text), wide("TapClip"));
    unsafe { MessageBoxW(ptr::null_mut(), text.as_ptr(), caption.as_ptr(), style | MB_SETFOREGROUND) };
}
