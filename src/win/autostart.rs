//! Start with Windows through the per-user `Run` registry key.

use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

use super::ui::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "TapClip";

pub fn is_enabled() -> bool {
    let (key, name) = (wide(RUN_KEY), wide(VALUE_NAME));
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut size,
        )
    };
    status == ERROR_SUCCESS
}

/// `config` is only passed for a non-default config file.
pub fn set(enabled: bool, config: Option<&Path>) -> Result<(), String> {
    let (key, name) = (wide(RUN_KEY), wide(VALUE_NAME));
    let status = if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("找不到程序路径：{e}"))?;
        let mut command = format!("\"{}\"", exe.display());
        if let Some(path) = config {
            command.push_str(&format!(" --config \"{}\"", path.display()));
        }
        let data = wide(&command);
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        }
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            other => other,
        }
    };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(format!("写入注册表失败（错误码 {status}）")) }
}
