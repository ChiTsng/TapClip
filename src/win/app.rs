//! The hidden app window. It owns the engine, the clipboard listener and the
//! tray icon; every clipboard operation happens on its thread.
//!
//! App state sits in a thread-local `RefCell`. Modal UI (menus, message boxes)
//! pumps messages re-entrantly, so it always runs outside of any borrow.

use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, KillTimer, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WM_APP, WM_CLIPBOARDUPDATE, WM_CLOSE, WM_DESTROY, WM_DESTROYCLIPBOARD, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};

use super::autostart;
use super::clipboard::{self, CF_UNICODE, Formats, Snapshot};
use super::tray::Tray;
use super::ui::{message_box, wide};
use crate::bridge::{Job, TapError};
use crate::config::Config;
use crate::engine::{Effect, Engine, LocalCopy};
use crate::{log, pairing};

pub const WM_APP_JOB: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_PROMOTE: u32 = WM_APP + 3;
const WM_APP_PAIRING: u32 = WM_APP + 4;

const TIMER_EXPIRE: usize = 1;
const TIMER_PROMOTE: usize = 2;
const PROMOTE_RETRY_MS: u32 = 50;
const PROMOTE_ATTEMPTS: u32 = 40;

const WINDOW_CLASS: &str = "TapClip.Window";

const MENU_PAIRING: usize = 1;
const MENU_AUTOSTART: usize = 2;
const MENU_QUIT: usize = 3;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

struct App {
    hwnd: HWND,
    engine: Engine,
    formats: Formats,
    jobs: mpsc::Receiver<Job>,
    config: Config,
    config_path: PathBuf,
    custom_config: bool,
    tray: Tray,
    /// Phone text currently promised on the PC clipboard.
    offered: Option<String>,
    /// The user's clipboard from before the offer, put back if it expires.
    snapshot: Option<Snapshot>,
    /// A pasted offer waiting to become a normal item, with attempts so far.
    promote: Option<(String, u32)>,
}

pub fn create(
    config: Config,
    config_path: PathBuf,
    custom_config: bool,
    jobs: mpsc::Receiver<Job>,
) -> Result<HWND, String> {
    let class = wide(WINDOW_CLASS);
    let title = wide("TapClip");
    let hwnd = unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err("无法注册窗口类".into());
        }
        // A hidden top-level window (not message-only) so it receives the
        // TaskbarCreated broadcast and can own the tray icon.
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("无法创建窗口".into());
    }
    TASKBAR_CREATED.store(unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }, Ordering::Relaxed);

    let app = App {
        hwnd,
        engine: Engine::new(Duration::from_secs(config.ttl_seconds)),
        formats: Formats::register(),
        jobs,
        config,
        config_path,
        custom_config,
        tray: Tray::add(hwnd),
        offered: None,
        snapshot: None,
        promote: None,
    };
    APP.with(|cell| *cell.borrow_mut() = Some(app));

    if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
        destroy(hwnd);
        return Err("无法监听剪贴板".into());
    }
    Ok(hwnd)
}

pub fn run_loop() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn destroy(hwnd: HWND) {
    unsafe { DestroyWindow(hwnd) };
}

/// Asks a running instance to exit cleanly (restoring a pending offer) and
/// waits up to three seconds for its window to go. False if none was running.
pub fn quit_running() -> bool {
    let class = wide(WINDOW_CLASS);
    let find = || unsafe { FindWindowW(class.as_ptr(), ptr::null()) };
    let hwnd = find();
    if hwnd.is_null() {
        return false;
    }
    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    for _ in 0..60 {
        if find().is_null() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Shows the pairing details once the message loop runs.
pub fn show_pairing_later(hwnd: HWND) {
    unsafe { PostMessageW(hwnd, WM_APP_PAIRING, 0, 0) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut app) => app.as_mut().map(f),
        Err(_) => {
            log!("re-entrant window message dropped");
            None
        }
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            with_app(App::on_clipboard_update);
        }
        WM_RENDERFORMAT => {
            with_app(|app| app.on_render(wparam as u32));
        }
        WM_RENDERALLFORMATS => {
            with_app(App::on_render_all);
        }
        // Our promise was replaced (or we emptied the clipboard ourselves);
        // the WM_CLIPBOARDUPDATE that follows tells us by what.
        WM_DESTROYCLIPBOARD => {}
        WM_TIMER => {
            with_app(|app| app.on_timer(wparam));
        }
        WM_APP_JOB => {
            with_app(App::on_jobs);
        }
        WM_APP_PROMOTE => {
            with_app(App::try_promote);
        }
        WM_APP_TRAY => {
            if matches!(lparam as u32 & 0xFFFF, WM_LBUTTONUP | WM_RBUTTONUP) {
                show_menu(hwnd);
            }
        }
        WM_APP_PAIRING => show_pairing(),
        WM_DESTROY => {
            with_app(App::shutdown);
            unsafe { PostQuitMessage(0) };
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            with_app(|app| app.tray.show());
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    0
}

impl App {
    fn on_clipboard_update(&mut self) {
        if clipboard::owned_by(self.hwnd) {
            return; // our own write
        }
        let Some(copy) = clipboard::read_local_copy(self.hwnd, &self.formats) else {
            log!("local copy ignored: clipboard busy");
            return;
        };
        log!("local copy: {}", describe(&copy));
        self.promote = None;
        let effect = self.engine.on_local_copy(copy, Instant::now());
        self.apply(effect);
    }

    fn on_jobs(&mut self) {
        while let Ok(job) = self.jobs.try_recv() {
            let (outcome, effect) = self.engine.on_tap(job.phone, job.choice, Instant::now());
            let result = if self.apply(effect) { Ok(outcome) } else { Err(TapError::Busy) };
            let _ = job.reply.send(result);
        }
    }

    fn on_render(&mut self, format: u32) {
        if format != CF_UNICODE {
            return;
        }
        let Some(text) = self.offered.as_deref() else { return };
        clipboard::render(text);
        log!("offer pasted");
        match self.engine.on_offer_used(Instant::now()) {
            Effect::None => self.clear_offer(),
            effect => {
                self.apply(effect);
            }
        }
    }

    fn on_render_all(&mut self) {
        if let Some(text) = &self.offered {
            clipboard::render_all(self.hwnd, text);
        }
    }

    fn on_timer(&mut self, id: usize) {
        unsafe { KillTimer(self.hwnd, id) };
        match id {
            TIMER_EXPIRE => {
                log!("offer expired unused");
                self.engine.on_offer_expired();
                // Restore even if the engine already moved on after a failed
                // re-offer: our promise may still sit on the clipboard.
                if self.offered.is_some() {
                    self.apply(Effect::Restore);
                }
            }
            TIMER_PROMOTE => self.try_promote(),
            _ => {}
        }
    }

    fn try_promote(&mut self) {
        let Some((text, attempts)) = self.promote.take() else { return };
        if !clipboard::owned_by(self.hwnd) {
            return; // someone copied something else meanwhile
        }
        if clipboard::publish(self.hwnd, &text) {
            log!("pasted offer promoted to a normal item");
        } else if attempts + 1 < PROMOTE_ATTEMPTS {
            self.promote = Some((text, attempts + 1));
            unsafe { SetTimer(self.hwnd, TIMER_PROMOTE, PROMOTE_RETRY_MS, None) };
        } else {
            log!("promote gave up: clipboard stayed busy");
        }
    }

    /// Applies an engine effect to the PC clipboard. False if it was busy.
    fn apply(&mut self, effect: Effect) -> bool {
        match effect {
            Effect::None => true,
            Effect::Offer(text) => {
                if self.offered.is_none() {
                    // Only the user's own clipboard is worth keeping: when
                    // re-offering, the clipboard holds our previous promise.
                    self.snapshot = Some(clipboard::snapshot(self.hwnd));
                }
                if !clipboard::offer(self.hwnd, &self.formats) {
                    log!("offer failed: clipboard busy");
                    self.engine.on_offer_expired();
                    if self.offered.is_none() {
                        self.snapshot = None;
                    }
                    return false;
                }
                log!("offering phone text (len={})", text.len());
                self.offered = Some(text);
                self.promote = None;
                let ttl_ms = self.engine.ttl().as_millis().min(u32::MAX as u128) as u32;
                unsafe { SetTimer(self.hwnd, TIMER_EXPIRE, ttl_ms, None) };
                true
            }
            Effect::Abandon => {
                log!("offer superseded by a local copy");
                self.clear_offer();
                true
            }
            Effect::Promote(text) => {
                self.clear_offer();
                self.promote = Some((text, 0));
                unsafe { PostMessageW(self.hwnd, WM_APP_PROMOTE, 0, 0) };
                true
            }
            Effect::Restore => {
                let snapshot = self.snapshot.take();
                self.clear_offer();
                let restored = snapshot.is_some_and(|s| clipboard::restore(self.hwnd, &self.formats, &s));
                log!("previous clipboard restored: {restored}");
                true
            }
        }
    }

    fn clear_offer(&mut self) {
        unsafe { KillTimer(self.hwnd, TIMER_EXPIRE) };
        self.offered = None;
        self.snapshot = None;
    }

    fn shutdown(&mut self) {
        if self.offered.is_some() {
            self.engine.on_offer_expired();
            self.apply(Effect::Restore);
        }
        unsafe { RemoveClipboardFormatListener(self.hwnd) };
        self.tray.remove();
    }
}

fn describe(copy: &LocalCopy) -> String {
    match copy {
        LocalCopy::Text(text) => format!("text (len={})", text.len()),
        LocalCopy::Sensitive => "sensitive, skipped".into(),
        LocalCopy::Other => "non-text".into(),
    }
}

fn show_menu(hwnd: HWND) {
    let autostart_on = autostart::is_enabled();
    let (pairing, autostart_label, quit) = (wide("配对信息…"), wide("开机自动运行"), wide("退出 TapClip"));
    let command = unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, MENU_PAIRING, pairing.as_ptr());
        AppendMenuW(
            menu,
            MF_STRING | if autostart_on { MF_CHECKED } else { 0 },
            MENU_AUTOSTART,
            autostart_label.as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());

        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        // Required for the menu to close when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        command as usize
    };

    match command {
        MENU_PAIRING => show_pairing(),
        MENU_AUTOSTART => {
            let custom = with_app(|app| app.custom_config.then(|| app.config_path.clone())).flatten();
            if let Err(e) = autostart::set(!autostart_on, custom.as_deref()) {
                message_box(&e, MB_OK | MB_ICONERROR);
            }
        }
        MENU_QUIT => destroy(hwnd),
        _ => {}
    }
}

fn show_pairing() {
    if let Some(text) = with_app(|app| pairing::info_text(&app.config, &app.config_path)) {
        message_box(&text, MB_OK | MB_ICONINFORMATION);
    }
}
