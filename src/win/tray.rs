//! Notification-area icon. Clicks arrive as `WM_APP_TRAY` on the app window.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

use super::app::WM_APP_TRAY;
use super::icon;

const TIP: &str = "TapClip · 碰一下，传剪贴板";

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
}

impl Tray {
    pub fn add(hwnd: HWND) -> Self {
        let tray = Self { hwnd, icon: icon::create() };
        tray.show();
        tray
    }

    /// (Re-)adds the icon, e.g. after Explorer restarts.
    pub fn show(&self) {
        let data = self.data();
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
    }

    pub fn remove(&self) {
        let data = self.data();
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &data);
            DestroyIcon(self.icon);
        }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.icon;
        for (slot, unit) in data.szTip.iter_mut().zip(TIP.encode_utf16().take(127)) {
            *slot = unit;
        }
        data
    }
}
