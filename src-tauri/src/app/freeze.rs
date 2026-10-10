//! Tray "Freeze": suspend the webview(s) without closing them.
//!
//! WebView2's `TrySuspend` pauses all page activity (JS, timers, network) and
//! lets the browser process trim the page's memory, but keeps the page, its DOM
//! and its session alive in the renderer. `Resume` continues exactly where the
//! page left off, so there is no reload and no re-login. This is the same
//! mechanism Edge uses for sleeping tabs.
//!
//! WebView2 refuses to suspend a visible webview, so freezing hides every
//! window first and also marks the controller invisible. Thawing restores both.
//! While frozen the page is offline: websocket-based sites (chat apps) see a
//! dropped connection and reconnect after `Resume`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{menu::CheckMenuItem, AppHandle, Manager, Wry};

/// Menu id of the tray checkbox.
pub const FREEZE_MENU_ID: &str = "freeze_app";

#[derive(Default)]
pub struct FreezeState {
    frozen: AtomicBool,
    item: Mutex<Option<CheckMenuItem<Wry>>>,
}

/// Counts suspend completions across all windows so a partial failure can be
/// rolled back only once every `TrySuspend` has reported back.
struct FreezeJob {
    pending: AtomicUsize,
    failed: AtomicBool,
}

impl FreezeJob {
    /// Records one window's result. Returns `Some(any_failed)` for the last one.
    fn finish(&self, ok: bool) -> Option<bool> {
        if !ok {
            self.failed.store(true, Ordering::Release);
        }
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            Some(self.failed.load(Ordering::Acquire))
        } else {
            None
        }
    }
}

/// Keep a handle to the tray checkbox so freeze/thaw can keep it in sync.
pub fn register_item(app: &AppHandle, item: CheckMenuItem<Wry>) {
    if let Some(state) = app.try_state::<FreezeState>() {
        if let Ok(mut slot) = state.item.lock() {
            *slot = Some(item);
        }
    }
}

pub fn is_frozen(app: &AppHandle) -> bool {
    app.try_state::<FreezeState>()
        .is_some_and(|state| state.frozen.load(Ordering::Acquire))
}

/// Tray checkbox clicked: the native toggle already flipped it, so follow it.
pub fn on_tray_toggle(app: &AppHandle) {
    let want_frozen = app
        .try_state::<FreezeState>()
        .and_then(|state| {
            state
                .item
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().and_then(|item| item.is_checked().ok()))
        })
        .unwrap_or(false);

    if want_frozen {
        freeze(app);
    } else {
        thaw(app);
    }
}

/// Any "show the app" path calls this first so a frozen (blank) webview is
/// never put on screen.
pub fn thaw_if_frozen(app: &AppHandle) {
    if is_frozen(app) {
        thaw(app);
    }
}

fn freeze(app: &AppHandle) {
    let Some(state) = app.try_state::<FreezeState>() else {
        return;
    };
    if state.frozen.swap(true, Ordering::AcqRel) {
        return;
    }

    let windows: Vec<_> = app.webview_windows().into_values().collect();
    if windows.is_empty() {
        state.frozen.store(false, Ordering::Release);
        sync_checkbox(&state, false);
        return;
    }

    let job = Arc::new(FreezeJob {
        pending: AtomicUsize::new(windows.len()),
        failed: AtomicBool::new(false),
    });

    for window in windows {
        let _ = window.hide();

        let app_handle = app.clone();
        let job = job.clone();
        let on_done: Arc<dyn Fn(bool) + Send + Sync> = Arc::new(move |ok| {
            if let Some(any_failed) = job.finish(ok) {
                settle(&app_handle, any_failed);
            }
        });

        let fallback = on_done.clone();
        if let Err(error) = window.with_webview(move |webview| {
            com::suspend(&webview.controller(), on_done);
        }) {
            eprintln!("[Pake] Freeze: webview unavailable: {error}");
            fallback(false);
        }
    }
}

/// Runs once after every window reported its suspend result.
fn settle(app: &AppHandle, any_failed: bool) {
    let Some(state) = app.try_state::<FreezeState>() else {
        return;
    };
    if any_failed {
        eprintln!("[Pake] Freeze refused (media or a call may be active); rolling back.");
        state.frozen.store(false, Ordering::Release);
        sync_checkbox(&state, false);
        resume_all(app);
    } else if !state.frozen.load(Ordering::Acquire) {
        // Thawed by the user while the suspend was still completing: the
        // earlier resume may have run before the page actually suspended.
        resume_all(app);
    }
}

fn thaw(app: &AppHandle) {
    let Some(state) = app.try_state::<FreezeState>() else {
        return;
    };
    if !state.frozen.swap(false, Ordering::AcqRel) {
        return;
    }
    resume_all(app);
    sync_checkbox(&state, false);
}

fn resume_all(app: &AppHandle) {
    for window in app.webview_windows().into_values() {
        let _ = window.with_webview(|webview| com::resume(&webview.controller()));
    }
}

fn sync_checkbox(state: &FreezeState, checked: bool) {
    if let Ok(slot) = state.item.lock() {
        if let Some(item) = slot.as_ref() {
            let _ = item.set_checked(checked);
        }
    }
}

// ---8<--- com (WebView2 calls; kept free of tauri types)
mod com {
    use std::sync::Arc;
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Controller, ICoreWebView2_3};
    use webview2_com::TrySuspendCompletedHandler;
    use windows::core::Interface;

    /// Hide the controller and start `TrySuspend`. `on_done(true)` fires once the
    /// page is actually suspended; `on_done(false)` on any refusal or error.
    pub fn suspend(controller: &ICoreWebView2Controller, on_done: Arc<dyn Fn(bool) + Send + Sync>) {
        let started = unsafe {
            (|| -> windows::core::Result<()> {
                // TrySuspend fails while the webview is visible.
                controller.SetIsVisible(false)?;
                let core3: ICoreWebView2_3 = controller.CoreWebView2()?.cast()?;
                let callback = on_done.clone();
                let handler = TrySuspendCompletedHandler::create(Box::new(move |result, ok| {
                    callback(result.is_ok() && ok);
                    Ok(())
                }));
                core3.TrySuspend(&handler)
            })()
        };
        if let Err(error) = started {
            eprintln!("[Pake] Freeze: TrySuspend could not start: {error}");
            on_done(false);
        }
    }

    /// Resume the page and make the controller visible again, which is its
    /// normal state (window visibility is handled by the hidden/shown HWND).
    pub fn resume(controller: &ICoreWebView2Controller) {
        unsafe {
            if let Ok(core) = controller.CoreWebView2() {
                if let Ok(core3) = core.cast::<ICoreWebView2_3>() {
                    let _ = core3.Resume();
                }
            }
            let _ = controller.SetIsVisible(true);
        }
    }
}
// ---8<--- end com
