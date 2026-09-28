#![cfg_attr(not(test), windows_subsystem = "windows")]

//! TapClip: tap an NFC tag with the iPhone to share one clipboard item with
//! this PC. See docs/SPEC.md for the behaviour.

mod bridge;
mod config;
mod engine;
mod logging;
mod pairing;
mod server;
mod win;

use std::path::PathBuf;
use std::sync::mpsc;

use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK};

use bridge::Bridge;
use win::ui::message_box;

fn main() {
    if std::env::args_os().any(|arg| arg == "--quit") {
        win::app::quit_running();
        return;
    }
    let custom_config = config_arg();
    let Some(_instance) = win::instance::acquire() else {
        message_box("TapClip 已经在运行了，图标在任务栏右下角。", MB_OK | MB_ICONINFORMATION);
        return;
    };
    logging::init(&config::log_path());

    let config_path = custom_config.clone().unwrap_or_else(config::default_path);
    let (config, first_run) = match config::load_or_create(&config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            message_box(&format!("TapClip 无法启动：{e}"), MB_OK | MB_ICONERROR);
            return;
        }
    };

    let (jobs, job_queue) = mpsc::channel();
    let hwnd = match win::app::create(config.clone(), config_path, custom_config.is_some(), job_queue) {
        Ok(hwnd) => hwnd,
        Err(e) => {
            message_box(&format!("TapClip 无法启动：{e}"), MB_OK | MB_ICONERROR);
            return;
        }
    };
    if let Err(e) = server::spawn(&config, Bridge::new(hwnd, jobs)) {
        message_box(
            &format!("TapClip 无法监听端口 {}：{e}\n\n可能被别的程序占用了，可以在配置文件里换一个端口。", config.port),
            MB_OK | MB_ICONERROR,
        );
        win::app::destroy(hwnd);
        return;
    }

    log!("TapClip {} started, port {}", env!("CARGO_PKG_VERSION"), config.port);
    if first_run {
        win::app::show_pairing_later(hwnd);
    }
    win::app::run_loop();
    log!("TapClip exited");
}

/// `--config <path>` selects a non-default config file (used by the tests).
fn config_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}
