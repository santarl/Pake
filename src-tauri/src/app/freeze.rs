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
//!
//! While frozen the tray shows a grey icon with a pause badge and a tooltip.
//! Freeze can be toggled from the tray menu checkbox or by middle-clicking the
//! tray icon.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{image::Image, menu::CheckMenuItem, tray::TrayIcon, AppHandle, Manager, Wry};

/// Menu id of the tray checkbox.
pub const FREEZE_MENU_ID: &str = "freeze_app";

#[derive(Default)]
pub struct FreezeState {
    frozen: AtomicBool,
    item: Mutex<Option<CheckMenuItem<Wry>>>,
    indicator: Mutex<Option<Indicator>>,
}

/// The tray icon plus its normal and frozen looks.
struct Indicator {
    tray: TrayIcon<Wry>,
    normal: Image<'static>,
    frozen: Image<'static>,
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
        let _ = item.set_checked(state.frozen.load(Ordering::Acquire));
        if let Ok(mut slot) = state.item.lock() {
            *slot = Some(item);
        }
    }
}

/// Keep the tray icon so it can switch to a frozen look (grey, pause badge).
pub fn register_tray(app: &AppHandle, tray: &TrayIcon<Wry>, base: &Image<'_>) {
    let Some(state) = app.try_state::<FreezeState>() else {
        return;
    };
    let (width, height) = (base.width(), base.height());
    let indicator = Indicator {
        tray: tray.clone(),
        normal: Image::new_owned(base.rgba().to_vec(), width, height),
        frozen: Image::new_owned(frozen_rgba(base.rgba(), width, height), width, height),
    };
    if let Ok(mut slot) = state.indicator.lock() {
        *slot = Some(indicator);
    }
    apply_indicator(&state, state.frozen.load(Ordering::Acquire));
}

/// Middle-click on the tray icon.
pub fn toggle(app: &AppHandle) {
    if is_frozen(app) {
        thaw(app);
    } else {
        freeze(app);
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
    // Middle-click does not go through the checkbox, so reflect the state here.
    sync_checkbox(&state, true);
    apply_indicator(&state, true);

    let windows: Vec<_> = app.webview_windows().into_values().collect();
    if windows.is_empty() {
        state.frozen.store(false, Ordering::Release);
        sync_checkbox(&state, false);
        apply_indicator(&state, false);
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
        apply_indicator(&state, false);
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
    apply_indicator(&state, false);
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

fn apply_indicator(state: &FreezeState, frozen: bool) {
    if let Ok(slot) = state.indicator.lock() {
        if let Some(indicator) = slot.as_ref() {
            let icon = if frozen {
                &indicator.frozen
            } else {
                &indicator.normal
            };
            let _ = indicator.tray.set_icon(Some(icon.clone()));
            let tooltip = frozen.then_some("Frozen (middle-click to resume)");
            let _ = indicator.tray.set_tooltip(tooltip);
        }
    }
}

// ---8<--- icon (pure pixel code, no tauri types)
/// Grey, dimmed copy of an RGBA icon with a small blue "pause" badge in the
/// bottom-right corner. Used as the tray icon while frozen.
fn frozen_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = rgba.to_vec();
    if out.len() != w * h * 4 {
        return out;
    }

    for px in out.chunks_exact_mut(4) {
        let luma = 0.299 * f32::from(px[0]) + 0.587 * f32::from(px[1]) + 0.114 * f32::from(px[2]);
        let grey = (luma * 0.6) as u8;
        px[..3].fill(grey);
    }

    let side = w.min(h) as f32;
    let radius = side * 0.3;
    let margin = side * 0.03;
    let (cx, cy) = (w as f32 - radius - margin, h as f32 - radius - margin);
    let (bar_half_gap, bar_width, bar_half_height) = (radius * 0.11, radius * 0.3, radius * 0.52);

    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let coverage = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let dx = (fx - cx).abs();
            let in_bar = (fy - cy).abs() <= bar_half_height
                && dx >= bar_half_gap
                && dx <= bar_half_gap + bar_width;
            let colour: [f32; 3] = if in_bar {
                [255.0, 255.0, 255.0]
            } else {
                [37.0, 99.0, 235.0]
            };

            let px = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
            let dst_alpha = f32::from(px[3]) / 255.0;
            let out_alpha = coverage + dst_alpha * (1.0 - coverage);
            for (channel, src) in px[..3].iter_mut().zip(colour) {
                let dst = f32::from(*channel);
                *channel =
                    ((src * coverage + dst * dst_alpha * (1.0 - coverage)) / out_alpha) as u8;
            }
            px[3] = (out_alpha * 255.0) as u8;
        }
    }
    out
}
// ---8<--- end icon

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

#[cfg(test)]
mod tests {
    use super::frozen_rgba;

    fn solid(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
        pixel.repeat((width * height) as usize)
    }

    fn pixel(buf: &[u8], width: usize, x: usize, y: usize) -> &[u8] {
        &buf[(y * width + x) * 4..(y * width + x) * 4 + 4]
    }

    #[test]
    fn keeps_size() {
        let out = frozen_rgba(&solid(32, 32, [200, 40, 40, 255]), 32, 32);
        assert_eq!(out.len(), 32 * 32 * 4);
    }

    #[test]
    fn greys_and_dims_the_icon_away_from_the_badge() {
        let out = frozen_rgba(&solid(64, 64, [200, 40, 40, 255]), 64, 64);
        let px = pixel(&out, 64, 0, 0);
        assert!(px[0] == px[1] && px[1] == px[2]);
        assert!(px[0] < 200);
        assert_eq!(px[3], 255);
    }

    #[test]
    fn draws_a_blue_badge_in_the_bottom_right() {
        let out = frozen_rgba(&solid(64, 64, [200, 40, 40, 255]), 64, 64);
        // Inside the badge circle, outside the white pause bars.
        let px = pixel(&out, 64, 27, 42);
        assert!(px[2] > px[0] + 100, "expected blue, got {px:?}");
    }

    #[test]
    fn leaves_transparent_pixels_transparent_away_from_the_badge() {
        let out = frozen_rgba(&solid(64, 64, [0, 0, 0, 0]), 64, 64);
        assert_eq!(pixel(&out, 64, 0, 0)[3], 0);
        // ...but the badge itself is opaque.
        assert_eq!(pixel(&out, 64, 27, 42)[3], 255);
    }

    #[test]
    fn ignores_malformed_buffers() {
        assert_eq!(frozen_rgba(&[1, 2, 3], 4, 4), vec![1, 2, 3]);
    }
}
