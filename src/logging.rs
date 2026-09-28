//! Tiny append-only log in `%LOCALAPPDATA%\TapClip`. Never records clipboard
//! contents, only sizes and decisions.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 1024 * 1024;

static FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn init(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.old"));
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = file;
}

pub fn write(args: std::fmt::Arguments) {
    let mut guard = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = guard.as_mut() {
        let mut t = SYSTEMTIME::default();
        // SAFETY: `t` is a valid out-pointer.
        unsafe { GetLocalTime(&mut t) };
        let _ = writeln!(
            file,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {args}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        );
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::logging::write(format_args!($($arg)*)) };
}
