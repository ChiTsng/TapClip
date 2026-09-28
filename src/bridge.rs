//! Hand-off from the HTTP thread to the UI thread, which owns the engine and
//! the clipboard.

use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::engine::{Choice, TapOutcome};
use crate::win::app::WM_APP_JOB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapError {
    /// Another app held the clipboard; the phone should just tap again.
    Busy,
    /// The UI thread did not answer in time.
    Unavailable,
}

pub type TapResult = Result<TapOutcome, TapError>;

pub struct Job {
    pub phone: Option<String>,
    pub choice: Choice,
    pub reply: mpsc::Sender<TapResult>,
}

/// Anything that can serve a tap; the HTTP layer only sees this.
pub trait Tapper {
    fn tap(&self, phone: Option<String>, choice: Choice) -> TapResult;
}

pub struct Bridge {
    /// HWND as an integer: raw window handles are not `Send`.
    hwnd: usize,
    jobs: mpsc::Sender<Job>,
}

impl Bridge {
    pub fn new(hwnd: HWND, jobs: mpsc::Sender<Job>) -> Self {
        Self { hwnd: hwnd as usize, jobs }
    }
}

impl Tapper for Bridge {
    fn tap(&self, phone: Option<String>, choice: Choice) -> TapResult {
        let (reply, answer) = mpsc::channel();
        self.jobs.send(Job { phone, choice, reply }).map_err(|_| TapError::Unavailable)?;
        // SAFETY: posting to a window owned by this process; a stale handle only fails.
        if unsafe { PostMessageW(self.hwnd as HWND, WM_APP_JOB, 0, 0) } == 0 {
            return Err(TapError::Unavailable);
        }
        answer.recv_timeout(Duration::from_secs(3)).unwrap_or(Err(TapError::Unavailable))
    }
}
