//! Single-instance guard via a session-local named mutex.

use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

use super::ui::wide;

/// The guard lives as long as the returned handle; `None` if TapClip already runs.
pub fn acquire() -> Option<HANDLE> {
    let name = wide("Local\\TapClip.SingleInstance");
    unsafe {
        let handle = CreateMutexW(ptr::null(), 0, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return None;
        }
        Some(handle)
    }
}
