//! OS notifications through one hidden window on its own thread: lid, power source, screen,
//! sleep/resume (power setting notifications) and session lock/unlock (WTS). The same
//! window receives the end-of-session messages, which stop the daemon cleanly on logoff
//! or shutdown. Created on first use; lives as long as the process.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use daemon::sys::{Guard, SystemEvent};
use proto::ApiError;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::{POWERBROADCAST_SETTING, RegisterPowerSettingNotification};
use windows_sys::Win32::System::RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification};
use windows_sys::Win32::System::SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_CONSOLE_DISPLAY_STATE, GUID_LIDSWITCH_STATE_CHANGE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DispatchMessageW, GetMessageW, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, RegisterClassW, TranslateMessage, WM_ENDSESSION,
    WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_WTSSESSION_CHANGE, WNDCLASSW, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use super::ffi::to_wide;
use super::power::guid_u128;

type Sink = Arc<dyn Fn(SystemEvent) + Send + Sync>;

/// How long the session-end handler waits for the daemon to stop. Windows shows its
/// "apps are preventing shutdown" screen after about five seconds.
const END_SESSION_WAIT: Duration = Duration::from_millis(4500);

// Statics rather than one lazily built struct: Windows sends the current lid and screen
// state right after registering, before a lazily built struct would exist.
static SINKS: Mutex<Vec<(u64, Sink)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(1);
/// Last lid and screen states, replayed to late subscribers as the initial ones.
static LID_CLOSED: Mutex<Option<bool>> = Mutex::new(None);
static DISPLAY_ON: Mutex<Option<bool>> = Mutex::new(None);
static LID_SEEN: AtomicBool = AtomicBool::new(false);
static ON_END: Mutex<Vec<Box<dyn Fn() + Send + Sync>>> = Mutex::new(Vec::new());
static STOPPED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static STARTED: OnceLock<Result<(), String>> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Starts the window thread on first use.
fn started() -> Result<(), String> {
    STARTED
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            match std::thread::Builder::new().name("os-events".into()).spawn(move || window_thread(tx)) {
                Ok(_) => rx.recv().unwrap_or_else(|_| Err("event thread ended".into())),
                Err(e) => Err(e.to_string()),
            }
        })
        .clone()
}

fn deliver(e: SystemEvent) {
    let sinks: Vec<Sink> = lock(&SINKS).iter().map(|(_, s)| s.clone()).collect();
    for s in sinks {
        s(e);
    }
}

struct Subscription(u64);

impl Guard for Subscription {}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&SINKS).retain(|(id, _)| *id != self.0);
    }
}

pub fn watch(sink: Sink) -> Result<Box<dyn Guard>, ApiError> {
    started().map_err(|e| ApiError::internal(format!("OS event window: {e}")))?;
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    // Held while replaying, so a change arriving meanwhile is delivered after the replay.
    let mut sinks = lock(&SINKS);
    if let Some(closed) = *lock(&LID_CLOSED) {
        sink(SystemEvent::Lid { closed, initial: true });
    }
    if let Some(on) = *lock(&DISPLAY_ON) {
        sink(SystemEvent::Display { on });
    }
    sinks.push((id, sink));
    Ok(Box::new(Subscription(id)))
}

/// Runs `f` when the session ends (logoff, shutdown, restart). The handler then waits,
/// briefly, for [`session_end_done`].
pub fn on_session_end(f: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
    started()?;
    lock(&ON_END).push(Box::new(f));
    Ok(())
}

/// The daemon has stopped; a waiting session-end handler may return.
pub fn session_end_done() {
    *lock(&STOPPED.0) = true;
    STOPPED.1.notify_all();
}

fn window_thread(ready: std::sync::mpsc::Sender<Result<(), String>>) {
    let class = to_wide("away-guard-events");
    // SAFETY: Win32 window setup with valid, live arguments; failures are reported.
    unsafe {
        let instance = GetModuleHandleW(null_mut());
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance, lpszClassName: class.as_ptr(), ..Default::default() };
        if RegisterClassW(&wc) == 0 {
            let _ = ready.send(Err(format!("RegisterClass: {}", std::io::Error::last_os_error())));
            return;
        }
        // A hidden top-level window (never shown): message-only windows miss broadcasts.
        let title = to_wide("away-guard");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), instance, null_mut());
        if hwnd.is_null() {
            let _ = ready.send(Err(format!("CreateWindow: {}", std::io::Error::last_os_error())));
            return;
        }
        for g in [GUID_LIDSWITCH_STATE_CHANGE, GUID_ACDC_POWER_SOURCE, GUID_CONSOLE_DISPLAY_STATE] {
            RegisterPowerSettingNotification(hwnd as _, &g, DEVICE_NOTIFY_WINDOW_HANDLE);
        }
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        let _ = ready.send(Ok(()));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn power_setting_event(s: &POWERBROADCAST_SETTING) -> Option<SystemEvent> {
    if s.DataLength < 4 {
        return None;
    }
    // SAFETY: DataLength >= 4 bytes follow the header.
    let value = unsafe { std::ptr::read_unaligned(s.Data.as_ptr().cast::<u32>()) };
    let id = guid_u128(&s.PowerSetting);
    if id == guid_u128(&GUID_LIDSWITCH_STATE_CHANGE) {
        let closed = value == 0;
        *lock(&LID_CLOSED) = Some(closed);
        // The first one comes right after registering and is the current state.
        let initial = !LID_SEEN.swap(true, Ordering::SeqCst);
        Some(SystemEvent::Lid { closed, initial })
    } else if id == guid_u128(&GUID_ACDC_POWER_SOURCE) {
        Some(SystemEvent::PowerSource { on_ac: value == 0 })
    } else if id == guid_u128(&GUID_CONSOLE_DISPLAY_STATE) {
        // 0 off, 1 on, 2 dimmed.
        *lock(&DISPLAY_ON) = Some(value != 0);
        Some(SystemEvent::Display { on: value != 0 })
    } else {
        None
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            let event = match wparam as u32 {
                PBT_POWERSETTINGCHANGE if lparam != 0 => {
                    // SAFETY: for this message lparam points to a POWERBROADCAST_SETTING.
                    power_setting_event(unsafe { &*(lparam as *const POWERBROADCAST_SETTING) })
                }
                PBT_APMSUSPEND => Some(SystemEvent::Suspending),
                PBT_APMRESUMEAUTOMATIC => Some(SystemEvent::Resumed),
                _ => None,
            };
            if let Some(e) = event {
                deliver(e);
            }
            1
        }
        WM_WTSSESSION_CHANGE => {
            match wparam as u32 {
                WTS_SESSION_LOCK => deliver(SystemEvent::Session { locked: true }),
                WTS_SESSION_UNLOCK => deliver(SystemEvent::Session { locked: false }),
                _ => {}
            }
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                for f in lock(&ON_END).iter() {
                    f();
                }
                // The process may be ended as soon as this returns: wait for the stop.
                let guard = lock(&STOPPED.0);
                let _ = STOPPED.1.wait_timeout_while(guard, END_SESSION_WAIT, |stopped| !*stopped);
            }
            0
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only: the window comes up, Windows tells it the current screen state, and a
    /// subscription can be made and dropped.
    #[test]
    fn subscribes_gets_the_current_state_and_unsubscribes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let guard = watch(Arc::new(move |e| sink.lock().unwrap().push(e))).unwrap();
        let count = || SINKS.lock().unwrap().len();
        let with = count();
        deliver(SystemEvent::Session { locked: true });
        assert!(seen.lock().unwrap().contains(&SystemEvent::Session { locked: true }));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while DISPLAY_ON.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "no initial screen state from Windows");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(guard);
        assert_eq!(count(), with - 1);
        // A late subscriber gets it replayed.
        let late = Arc::new(Mutex::new(Vec::new()));
        let sink = late.clone();
        drop(watch(Arc::new(move |e| sink.lock().unwrap().push(e))).unwrap());
        assert!(late.lock().unwrap().iter().any(|e| matches!(e, SystemEvent::Display { .. })), "{:?}", late.lock().unwrap());
    }
}
