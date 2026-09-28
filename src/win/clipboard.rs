//! Win32 clipboard primitives: classifying local copies, lazy (delayed
//! rendering) offers, promotion to a normal item, snapshot and restore.
//! Everything here runs on the UI thread that owns `hwnd`.

use std::ptr;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW, GetClipboardOwner,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT, CF_ENHMETAFILE, CF_METAFILEPICT, CF_OEMTEXT,
    CF_OWNERDISPLAY, CF_PALETTE, CF_TEXT, CF_UNICODETEXT,
};

use super::ui::wide;
use crate::engine::{LocalCopy, MAX_TEXT_BYTES, normalize_newlines};

pub const CF_UNICODE: u32 = CF_UNICODETEXT as u32;
const SNAPSHOT_LIMIT: usize = 64 * 1024 * 1024;
/// Formats whose handles are GDI objects rather than HGLOBAL memory.
const GDI_FORMATS: [u16; 8] = [
    CF_BITMAP,
    CF_METAFILEPICT,
    CF_PALETTE,
    CF_ENHMETAFILE,
    CF_OWNERDISPLAY,
    CF_DSPBITMAP,
    CF_DSPMETAFILEPICT,
    CF_DSPENHMETAFILE,
];

/// Registered formats through which apps say "do not record or sync this".
pub struct Formats {
    exclude_monitor: u32,
    viewer_ignore: u32,
    history: u32,
    cloud: u32,
}

impl Formats {
    pub fn register() -> Self {
        let register = |name: &str| unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) };
        Self {
            exclude_monitor: register("ExcludeClipboardContentFromMonitorProcessing"),
            viewer_ignore: register("Clipboard Viewer Ignore"),
            history: register("CanIncludeInClipboardHistory"),
            cloud: register("CanUploadToCloudClipboard"),
        }
    }

    fn private_markers(&self) -> [u32; 4] {
        [self.exclude_monitor, self.viewer_ignore, self.history, self.cloud]
    }
}

/// The clipboard, opened for the lifetime of this value.
struct Opened;

impl Opened {
    fn new(hwnd: HWND) -> Option<Self> {
        for attempt in 0..10 {
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Some(Opened);
            }
            if attempt < 9 {
                thread::sleep(Duration::from_millis(15));
            }
        }
        None
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

pub fn owned_by(hwnd: HWND) -> bool {
    unsafe { GetClipboardOwner() == hwnd }
}

/// Classifies what another app just put on the clipboard.
/// `None` when the clipboard could not be opened.
pub fn read_local_copy(hwnd: HWND, formats: &Formats) -> Option<LocalCopy> {
    let _open = Opened::new(hwnd)?;
    if is_sensitive(formats) {
        return Some(LocalCopy::Sensitive);
    }
    let copy = read_unicode_text()
        .filter(|text| !text.is_empty() && text.len() <= MAX_TEXT_BYTES)
        .map_or(LocalCopy::Other, |text| LocalCopy::Text(normalize_newlines(&text)));
    Some(copy)
}

fn is_sensitive(f: &Formats) -> bool {
    available(f.exclude_monitor) || available(f.viewer_ignore) || dword_is_zero(f.history) || dword_is_zero(f.cloud)
}

fn available(format: u32) -> bool {
    format != 0 && unsafe { IsClipboardFormatAvailable(format) } != 0
}

fn dword_is_zero(format: u32) -> bool {
    available(format)
        && with_locked(unsafe { GetClipboardData(format) }, |bytes| bytes.len() >= 4 && bytes[..4] == [0; 4])
            .unwrap_or(false)
}

fn read_unicode_text() -> Option<String> {
    if !available(CF_UNICODE) {
        return None;
    }
    with_locked(unsafe { GetClipboardData(CF_UNICODE) }, |bytes| {
        // Skip decoding text that could never fit the share limit.
        (bytes.len() <= MAX_TEXT_BYTES * 4).then(|| {
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .take_while(|&unit| unit != 0)
                .collect();
            String::from_utf16_lossy(&units)
        })
    })
    .flatten()
}

/// Promises text on the clipboard without handing it over (delayed
/// rendering), hidden from clipboard history, cloud sync and other monitors.
/// The text itself is supplied later by `render`.
pub fn offer(hwnd: HWND, formats: &Formats) -> bool {
    let Some(_open) = Opened::new(hwnd) else { return false };
    if unsafe { EmptyClipboard() } == 0 {
        return false;
    }
    unsafe { SetClipboardData(CF_UNICODE, ptr::null_mut()) };
    for marker in formats.private_markers() {
        put(marker, &0u32.to_le_bytes());
    }
    true
}

/// Answers WM_RENDERFORMAT: the requesting app already holds the clipboard open.
pub fn render(text: &str) -> bool {
    put(CF_UNICODE, &text_bytes(text))
}

/// Answers WM_RENDERALLFORMATS, sent when our window goes away while owning a promise.
pub fn render_all(hwnd: HWND, text: &str) {
    let Some(_open) = Opened::new(hwnd) else { return };
    if owned_by(hwnd) {
        render(text);
    }
}

/// Re-publishes text as an ordinary clipboard item, which history then records.
pub fn publish(hwnd: HWND, text: &str) -> bool {
    let Some(_open) = Opened::new(hwnd) else { return false };
    (unsafe { EmptyClipboard() } != 0) && put(CF_UNICODE, &text_bytes(text))
}

/// A copy of every restorable clipboard format.
pub struct Snapshot {
    items: Vec<(u32, Vec<u8>)>,
}

pub fn snapshot(hwnd: HWND) -> Snapshot {
    let mut items = Vec::new();
    let Some(_open) = Opened::new(hwnd) else { return Snapshot { items } };
    let has_unicode = available(CF_UNICODE);
    let mut total = 0;
    let mut format = 0;
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if !restorable(format, has_unicode) {
            continue;
        }
        let copied = with_locked(unsafe { GetClipboardData(format) }, |bytes| {
            (total + bytes.len() <= SNAPSHOT_LIMIT).then(|| bytes.to_vec())
        });
        if let Some(Some(bytes)) = copied {
            total += bytes.len();
            items.push((format, bytes));
        }
    }
    Snapshot { items }
}

/// Puts a snapshot back, marked so history and monitors do not record it twice.
pub fn restore(hwnd: HWND, formats: &Formats, snapshot: &Snapshot) -> bool {
    let Some(_open) = Opened::new(hwnd) else { return false };
    if unsafe { EmptyClipboard() } == 0 {
        return false;
    }
    for (format, bytes) in &snapshot.items {
        put(*format, bytes);
    }
    for marker in formats.private_markers() {
        if !snapshot.items.iter().any(|(format, _)| *format == marker) {
            put(marker, &0u32.to_le_bytes());
        }
    }
    true
}

fn restorable(format: u32, has_unicode: bool) -> bool {
    match format {
        f if GDI_FORMATS.iter().any(|&g| u32::from(g) == f) => false,
        // Private and GDI-object ranges only mean something to their owner.
        0x0200..=0x03FF => false,
        // Windows re-synthesizes these from CF_UNICODETEXT.
        f if has_unicode && (f == u32::from(CF_TEXT) || f == u32::from(CF_OEMTEXT)) => false,
        // OLE bookkeeping that points back into the original owner process.
        f if f >= 0xC000 => !matches!(format_name(f).as_deref(), Some("DataObject" | "Ole Private Data")),
        _ => true,
    }
}

fn format_name(format: u32) -> Option<String> {
    let mut buf = [0u16; 128];
    let len = unsafe { GetClipboardFormatNameW(format, buf.as_mut_ptr(), buf.len() as i32) };
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// Windows apps expect CRLF and a terminating NUL.
fn text_bytes(text: &str) -> Vec<u8> {
    text.replace('\n', "\r\n").encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

/// Runs `f` over the bytes behind an HGLOBAL; `None` if it is not one.
fn with_locked<R>(handle: HANDLE, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    if handle.is_null() {
        return None;
    }
    unsafe {
        let size = GlobalSize(handle);
        let data = GlobalLock(handle) as *const u8;
        if data.is_null() {
            return None;
        }
        let result = f(std::slice::from_raw_parts(data, size));
        GlobalUnlock(handle);
        Some(result)
    }
}

/// Places a copy of `bytes` on the open clipboard. The system takes ownership
/// of the memory on success; on failure we free it.
fn put(format: u32, bytes: &[u8]) -> bool {
    if format == 0 {
        return false;
    }
    let Some(handle) = alloc(bytes) else { return false };
    if unsafe { SetClipboardData(format, handle) }.is_null() {
        unsafe { GlobalFree(handle) };
        return false;
    }
    true
}

fn alloc(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if handle.is_null() {
            return None;
        }
        let data = GlobalLock(handle) as *mut u8;
        if data.is_null() {
            GlobalFree(handle);
            return None;
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        GlobalUnlock(handle);
        Some(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_bytes_are_crlf_utf16_with_nul() {
        let bytes = text_bytes("a\nb");
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&p| u16::from_le_bytes(p)).collect();
        assert_eq!(units, [b'a' as u16, b'\r' as u16, b'\n' as u16, b'b' as u16, 0]);
    }

    #[test]
    fn gdi_private_and_synthesized_formats_are_not_restored() {
        assert!(!restorable(u32::from(CF_BITMAP), false));
        assert!(!restorable(0x0250, false));
        assert!(!restorable(u32::from(CF_TEXT), true));
        assert!(restorable(u32::from(CF_TEXT), false));
        assert!(restorable(CF_UNICODE, true));
        assert!(restorable(15 /* CF_HDROP */, true));
    }
}
