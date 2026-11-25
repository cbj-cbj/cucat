#![recursion_limit = "256"]

mod cats;
mod config;
mod game_detect;
mod i18n;
mod input_hook;
#[cfg(target_os = "macos")]
mod macos_window;
mod mcp;
mod net;
mod obs;
mod privacy;
mod tray;
mod updater;
#[cfg(windows)]
mod win7_layered;
#[cfg(windows)]
mod windows_display;

#[cfg(windows)]
const WIN7_OFFSCREEN_PARK_POS: i32 = -16000;

use cats::{all_cats, cat_exists, cats_path, sanitize_cat_id, CatSkinConfig};
use config::{
    config_path, load_settings, normalize_keyboard_layout, normalize_keyboard_orientation,
    normalize_tail_key_ids, normalize_tail_key_mode, save_settings, set_windows_autostart,
    try_save_settings, AppSettings,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter, Listener, LogicalSize, Manager, PhysicalPosition, Size, State};

pub const BASE_WIDTH: f64 = 420.0;
pub const BASE_HEIGHT: f64 = 440.0;
pub const BASE_WIDTH_108: f64 = 540.0;
pub const BASE_HEIGHT_108: f64 = 456.0;
pub const MIN_WINDOW_WIDTH: f64 = 100.0;
pub const MIN_WINDOW_HEIGHT: f64 = 100.0;
#[cfg(windows)]
pub const WIN7_MIN_WINDOW_WIDTH: f64 = 260.0;
#[cfg(windows)]
pub const WIN7_MIN_WINDOW_HEIGHT: f64 = 410.0;

/// Appends one line to `%APPDATA%/kuro-cat-pet/runtime.log`.
///
/// The release profile uses `panic = "abort"`, so a panic otherwise leaves no
/// trace at all (the process just disappears with 0xc0000409).
pub fn kuro_log(line: &str) {
    use std::io::Write as _;
    let path = config::app_data_dir().join("runtime.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "[{}] {}", utc_timestamp(), line);
    }
}

/// Installs a panic hook that records the message before the abort.
pub fn install_crash_logger() {
    std::panic::set_hook(Box::new(|info| {
        let message = info.to_string().replace('\n', " | ");
        kuro_log(&format!("PANIC {message}"));
    }));
    kuro_log(&format!(
        "start v{} pid {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
}

fn utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-since-epoch -> (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub fn base_dimensions(layout: &str) -> (f64, f64) {
    if layout == "108" {
        (BASE_WIDTH_108, BASE_HEIGHT_108)
    } else {
        (BASE_WIDTH, BASE_HEIGHT)
    }
}

pub fn window_dimensions(layout: &str, scale: f64) -> (f64, f64) {
    let (bw, bh) = base_dimensions(layout);
    let s = scale.clamp(0.20, 1.50);
    #[cfg(windows)]
    let (min_w, min_h) = if win_needs_win7_ulw() {
        (WIN7_MIN_WINDOW_WIDTH, WIN7_MIN_WINDOW_HEIGHT)
    } else {
        (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)
    };
    #[cfg(not(windows))]
    let (min_w, min_h) = (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT);
    (
        (bw * s).round().max(min_w),
        (bh * s).round().max(min_h),
    )
}

/// Clamps a dragged pet window's top-left physical position against the virtual desktop bounds
/// while explicitly allowing the transparent headspace above the cat's ears (~17% of window height)
/// to slide above the screen top edge (`y < v_top`), so the cat itself can reach the very top bezel
/// without ever getting lost off-screen.
/// True when the window rectangle overlaps a visible monitor by a sensible margin.
/// Used to recover from a saved position left over from an unplugged monitor.
fn position_hits_any_monitor(app: &AppHandle, x: i32, y: i32, w: i32, h: i32) -> bool {
    let Ok(monitors) = app.available_monitors() else {
        return true;
    };
    if monitors.is_empty() {
        return true;
    }
    let w = w.max(1);
    let h = h.max(1);
    monitors.iter().any(|monitor| {
        let pos = monitor.position();
        let size = monitor.size();
        let overlap_w = (x + w).min(pos.x + size.width as i32) - x.max(pos.x);
        let overlap_h = (y + h).min(pos.y + size.height as i32) - y.max(pos.y);
        overlap_w > w / 3 && overlap_h > h / 4
    })
}

fn clamp_pet_drag_position(
    x: i32,
    y: i32,
    win_w: i32,
    win_h: i32,
    v_left: i32,
    v_top: i32,
    v_width: i32,
    v_height: i32,
) -> (i32, i32) {
    let v_right = v_left + v_width.max(320);
    let v_bottom = v_top + v_height.max(240);

    // Transparent visual padding around the cat/keyboard inside the window:
    // ~17% empty air above the ears/headset, ~10% side margins, ~8% below keyboard.
    let top_headroom = ((win_h as f64) * 0.17).round() as i32;
    let side_margin = ((win_w as f64) * 0.10).round() as i32;
    let min_visible_h = ((win_h as f64) * 0.68).round() as i32;

    let min_x = v_left - side_margin;
    let max_x = (v_right - win_w + side_margin).max(min_x);
    let min_y = v_top - top_headroom;
    let max_y = (v_bottom - min_visible_h).max(min_y);

    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

fn resize_window_anchored_bottom_right(
    win: &tauri::WebviewWindow,
    layout: &str,
    scale: f64,
) -> Option<(i32, i32)> {
    let (new_w, new_h) = window_dimensions(layout, scale);
    #[cfg(target_os = "macos")]
    if win
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .display_id
        .is_some()
    {
        macos_window::resize_pinned(win, new_w, new_h);
        return None;
    }
    #[cfg(windows)]
    {
        let preferred = win
            .state::<AppState>()
            .settings
            .lock()
            .unwrap()
            .windows_display_id
            .clone();
        if let Some(id) = preferred.as_deref() {
            return windows_display::resize_pinned(win, id, new_w, new_h);
        }
    }
    let sf = win.scale_factor().unwrap_or(1.0);

    #[cfg(windows)]
    if win_needs_win7_ulw() {
        let old_rect = win7_layered::get_rect();
        let _ = win.set_size(Size::Logical(LogicalSize::new(new_w, new_h)));
        let new_pw = (new_w * sf).round() as i32;
        let new_ph = (new_h * sf).round() as i32;
        if let Some((ox, oy, ow, oh)) = old_rect {
            let new_x = ox + ow - new_pw;
            let new_y = oy + oh - new_ph;
            if new_x != ox || new_y != oy {
                win7_layered::set_position(new_x, new_y);
            }
            if win7_layered::is_overlay_active() {
                let _ = win.set_position(PhysicalPosition::new(new_x, new_y));
            } else {
                let _ = win.set_position(PhysicalPosition::new(
                    WIN7_OFFSCREEN_PARK_POS,
                    WIN7_OFFSCREEN_PARK_POS,
                ));
            }
            return Some((new_x, new_y));
        }
        return None;
    }

    let old_pos = win.outer_position().ok();
    let old_size = win.outer_size().ok();
    let _ = win.set_size(Size::Logical(LogicalSize::new(new_w, new_h)));

    if let (Some(pos), Some(sz)) = (old_pos, old_size) {
        let new_pw = (new_w * sf).round() as i32;
        let new_ph = (new_h * sf).round() as i32;
        let (new_x, new_y) = (
            pos.x + sz.width as i32 - new_pw,
            pos.y + sz.height as i32 - new_ph,
        );
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let (new_x, new_y) = (new_x.max(0), new_y.max(0));
        if new_x != pos.x || new_y != pos.y {
            let _ = win.set_position(PhysicalPosition::new(new_x, new_y));
        }
        return Some((new_x, new_y));
    }
    None
}

pub fn is_pet_visible(app: &AppHandle) -> bool {
    #[cfg(windows)]
    if win_needs_win7_ulw() {
        return win7_layered::is_visible();
    }
    app.get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(true)
}

pub fn set_pet_visible(app: &AppHandle, visible: bool) {
    let on_top = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .always_on_top;

    #[cfg(windows)]
    if win_needs_win7_ulw() {
        win7_layered::set_visible(visible);
        if !visible {
            let _ = app.emit("kuro-win7-close-menu", ());
            win7_layered::set_overlay_active(false);
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_position(PhysicalPosition::new(
                    WIN7_OFFSCREEN_PARK_POS,
                    WIN7_OFFSCREEN_PARK_POS,
                ));
            }
        } else if on_top {
            win7_layered::reassert_topmost();
        }
        let _ = app.emit("kuro-win7-visibility", visible);
        return;
    }

    if let Some(win) = app.get_webview_window("main") {
        if visible {
            let _ = win.show();
            #[cfg(windows)]
            if on_top {
                win_gentle_reassert_topmost(&win);
            }
        } else {
            let _ = win.hide();
        }
    }
}

pub fn set_pet_always_on_top(app: &AppHandle, on_top: bool) {
    #[cfg(windows)]
    if win_needs_win7_ulw() {
        win7_layered::set_always_on_top(on_top);
    }
    if let Some(win) = app.get_webview_window("main") {
        #[cfg(target_os = "macos")]
        macos_window::configure_fullscreen(&win, on_top);
        #[cfg(not(target_os = "macos"))]
        let _ = win.set_always_on_top(on_top);
    }
}

pub const PEER_WINDOW_LABEL: &str = "peer";

fn peer_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(PEER_WINDOW_LABEL)
}

/// Creates the hidden window that mirrors the friend's cat. It is built once during
/// setup so no network thread ever has to construct a webview.
fn create_peer_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(existing) = peer_window(app) {
        return Some(existing);
    }
    #[cfg(windows)]
    if win_needs_win7_ulw() {
        return None;
    }
    let (w, h) = window_dimensions("65", 1.0);
    let built = tauri::WebviewWindowBuilder::new(
        app,
        PEER_WINDOW_LABEL,
        tauri::WebviewUrl::App("peer.html".into()),
    )
    .title("KuroCat Friend")
    .inner_size(w, h)
    .decorations(false)
    .resizable(false)
    .transparent(true)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    .visible(false)
    // MUST match the main window's additionalBrowserArgs, see open_context_menu.
    .additional_browser_args("--autoplay-policy=no-user-gesture-required")
    .build()
    .ok()?;
    // The friend's cat is purely decorative: never steal a click from the desktop.
    let _ = built.set_ignore_cursor_events(true);
    let close_target = built.clone();
    built.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = close_target.hide();
        }
    });
    Some(built)
}

/// Sizes the friend's window from their layout + scale and parks it beside our own
/// cat (left side first, flipping to the right when there is no room). A position the
/// user dragged into place wins over the automatic one.
fn apply_peer_window_geometry(app: &AppHandle, layout: &str, scale: f64) {
    let Some(win) = peer_window(app) else {
        return;
    };
    let (w, h) = window_dimensions(layout, scale);
    let _ = win.set_size(Size::Logical(LogicalSize::new(w, h)));

    let sf = win.scale_factor().unwrap_or(1.0);
    let pw = (w * sf).round() as i32;
    let ph = (h * sf).round() as i32;

    let manual = {
        let state = app.state::<AppState>();
        let s = state.settings.lock().unwrap();
        match (s.peer_x, s.peer_y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        }
    };
    if let Some((x, y)) = manual {
        if position_hits_any_monitor(app, x, y, pw, ph) {
            let _ = win.set_position(PhysicalPosition::new(x, y));
            return;
        }
    }

    let Some(main) = app.get_webview_window("main") else {
        return;
    };
    let Ok(main_pos) = main.outer_position() else {
        return;
    };
    let Ok(main_size) = main.outer_size() else {
        return;
    };
    let gap = (14.0 * sf).round() as i32;
    let mut x = main_pos.x - pw - gap;
    let mut y = main_pos.y + main_size.height as i32 - ph;

    if let Ok(Some(monitor)) = main.current_monitor() {
        let area = monitor.work_area();
        let left = area.position.x;
        let top = area.position.y;
        let right = left + area.size.width as i32;
        let bottom = top + area.size.height as i32;
        if x < left {
            x = main_pos.x + main_size.width as i32 + gap;
        }
        if x + pw > right {
            x = (right - pw).max(left);
        }
        y = y.clamp(top, (bottom - ph).max(top));
    }
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

/// Creates the friend window a moment after startup, on the main thread.
///
/// Building it inside `setup()` races with the first WebView2 initialisation of
/// the main window (same user-data folder), which can abort the process on the
/// very first launch after an update.
fn ensure_peer_window_async(app: &AppHandle) {
    if peer_window(app).is_some() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        let main_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            let _ = create_peer_window(&main_app);
        });
    });
}

pub fn set_peer_window_visible(app: &AppHandle, visible: bool) {
    if visible {
        // Never build a webview from this (event-callback) thread: creation is
        // deferred to the main thread, see `ensure_peer_window_async`.
        let Some(win) = peer_window(app) else {
            ensure_peer_window_async(app);
            return;
        };
        let _ = win.set_always_on_top(true);
        let _ = win.show();
        #[cfg(windows)]
        win_gentle_reassert_topmost(&win);
    } else if let Some(win) = peer_window(app) {
        let _ = win.hide();
    }
    // Lets the page start/stop its render loops so a hidden window costs no CPU.
    let _ = app.emit("kuro-peer-visibility", visible);
}

/// Everything the friend's window needs to look exactly like the original cat.
pub fn net_state_payload(settings: &AppSettings) -> serde_json::Value {
    let cats = cats::all_cats();
    let active = cats.into_iter().find(|c| c.id == settings.cat_breed);
    serde_json::json!({
        "name": settings.net_name,
        "catId": settings.cat_breed,
        "skin": active,
        "renderMode": settings.render_mode,
        "keyboardLayout": settings.keyboard_layout,
        "keyboardOrientation": settings.keyboard_orientation,
        "scale": settings.scale,
        "showKeyBubbles": settings.show_key_bubbles,
        "tailKeyMode": settings.tail_key_mode,
        "tailKeyIds": settings.tail_key_ids,
        "collarColor": settings.collar_color,
        "headphoneMode": settings.headphone_mode,
    })
}

pub fn broadcast_net_state(app: &AppHandle) {
    if !net::is_connected() {
        return;
    }
    // `try_lock` keeps this safe when called from a settings listener that runs
    // while another thread already holds the settings lock; the retry thread
    // makes sure the last change is still delivered.
    let snapshot = match app.state::<AppState>().settings.try_lock() {
        Ok(guard) => Some(guard.clone()),
        Err(_) => None,
    };
    match snapshot {
        Some(snapshot) => net::broadcast_state(&net_state_payload(&snapshot)),
        None => {
            let retry_app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                if !net::is_connected() {
                    return;
                }
                let snapshot = retry_app.state::<AppState>().settings.lock().unwrap().clone();
                net::broadcast_state(&net_state_payload(&snapshot));
            });
        }
    }
}

pub fn shutdown_pet_windows() {
    kuro_log("clean exit requested");
    net::stop();
    obs::shutdown_obs_server();
    #[cfg(windows)]
    if win_needs_win7_ulw() {
        win7_layered::shutdown();
    }
}

pub fn run_mcp_stdio() {
    mcp::run_stdio_mcp();
}

pub struct AppState {
    pub settings: Arc<Mutex<AppSettings>>,
    pub menu_open: Arc<AtomicBool>,
    tail_picker_bounds: Arc<Mutex<Option<TailPickerBounds>>>,
    pub menu_creating: Arc<AtomicBool>,
    pub menu_close_gen: Arc<AtomicU64>,
    pub menu_opened_at: Arc<Mutex<Instant>>,
    pub dragging: Arc<AtomicBool>,
    pub mcp_enabled: Arc<AtomicBool>,
    pub update_banner_open: Arc<AtomicBool>,
    pub paused_guard: Arc<AtomicBool>,
}

/// Shared Chromium / WebView2 lean runtime flags.
/// IMPORTANT: Must match `src-tauri/tauri.conf.json`'s `additionalBrowserArgs` byte-for-byte;
/// otherwise Windows WebView2 rejects creating the 2nd ("menu") window with HRESULT 0x8007139F.
const WEBVIEW_BROWSER_ARGS: &str = "--autoplay-policy=no-user-gesture-required --disable-background-networking --disable-sync --disable-extensions --disable-features=Translate,BackForwardCache,MediaRouter,OptimizationHints --js-flags=--max-old-space-size=96";

const SIDE_MENU_IDLE_DESTROY_SECS: u64 = 25;

fn schedule_side_menu_idle_destroy(app: AppHandle) {
    let gen = app
        .state::<AppState>()
        .menu_close_gen
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(SIDE_MENU_IDLE_DESTROY_SECS));
        let state = app.state::<AppState>();
        if state.menu_open.load(Ordering::Acquire)
            || state.menu_creating.load(Ordering::Acquire)
            || state.menu_close_gen.load(Ordering::Acquire) != gen
        {
            return;
        }
        if let Some(menu_win) = app.get_webview_window("menu") {
            // Fully destroy the hidden 2nd WebView2 renderer process after idle grace period
            // to reclaim ~35-55 MB RAM while keeping rapid reopen instant inside the window.
            let _ = menu_win.destroy();
        }
    });
}

#[tauri::command]
fn get_resolved_locale(state: State<'_, AppState>) -> String {
    let lang = state.settings.lock().unwrap().language.clone();
    i18n::effective_locale(&lang).to_string()
}

#[tauri::command]
fn trim_idle_memory() {
    #[cfg(windows)]
    {
        static LAST_TRIM_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_TRIM_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 45_000 {
            return;
        }
        LAST_TRIM_MS.store(now_ms, Ordering::Relaxed);

        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn SetProcessWorkingSetSize(
                h_process: isize,
                dw_minimum_working_set_size: usize,
                dw_maximum_working_set_size: usize,
            ) -> i32;
        }
        unsafe {
            let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowCursorInfo {
    rel_x: f64,
    rel_y: f64,
    inside_window: bool,
    over_cat: bool,
    alt_override: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TailPickerBounds {
    toolbar: [f64; 4],
    keyboard: [f64; 4],
    close_button: [f64; 4],
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> AppSettings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn get_cats() -> Vec<CatSkinConfig> {
    all_cats()
}

#[tauri::command]
fn get_game_active() -> bool {
    game_detect::is_game_active()
}

#[tauri::command]
fn get_mcp_config(state: State<'_, AppState>) -> String {
    let port = state.settings.lock().unwrap().mcp_port;
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "KuroCat.exe".to_string());
    serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            "kurocat": {
                "command": exe,
                "args": ["--mcp"],
                "url": format!("http://127.0.0.1:{}/mcp", port)
            }
        }
    }))
    .unwrap_or_default()
}

/// Start a room on this machine (LAN / same Wi-Fi, or a port-forwarded host).
/// Returns the `ip:port` the friend should paste into "join room".
#[tauri::command]
fn net_host(
    app: AppHandle,
    state: State<'_, AppState>,
    port: u16,
    room: String,
    name: String,
) -> Result<String, String> {
    let room = config::normalize_net_room(&room);
    let name = config::normalize_net_name(&name);
    let port = if port == 0 { net::DEFAULT_PORT } else { port };
    {
        let mut s = state.settings.lock().unwrap();
        s.net_room = room.clone();
        s.net_name = name.clone();
        s.net_port = port;
        save_settings(&s);
    }
    let bound = net::start_host(app.clone(), port, &room, &name)?;
    broadcast_net_state(&app);
    let ip = net::local_ip_string().unwrap_or_else(|| "本机IP".to_string());
    Ok(format!("{ip}:{bound}"))
}

/// Join a friend's room through the relay server (or a LAN host address).
#[tauri::command]
fn net_connect(
    app: AppHandle,
    state: State<'_, AppState>,
    server: String,
    room: String,
    name: String,
) -> Result<(), String> {
    let server = config::normalize_net_server(&server);
    let room = config::normalize_net_room(&room);
    let name = config::normalize_net_name(&name);
    if server.is_empty() {
        return Err("请填写中继服务器地址，或改用「局域网建房」".into());
    }
    {
        let mut s = state.settings.lock().unwrap();
        s.net_server = server.clone();
        s.net_room = room.clone();
        s.net_name = name.clone();
        save_settings(&s);
    }
    net::connect(app.clone(), &server, &room, &name)?;
    broadcast_net_state(&app);
    Ok(())
}

#[tauri::command]
fn net_disconnect(app: AppHandle) {
    net::stop();
    set_peer_window_visible(&app, false);
}

/// Remember where the user dragged the friend's cat window.
#[tauri::command]
fn save_peer_window_pos(app: AppHandle, state: State<'_, AppState>) {
    let Some(win) = peer_window(&app) else {
        return;
    };
    let Ok(pos) = win.outer_position() else {
        return;
    };
    let mut s = state.settings.lock().unwrap();
    s.peer_x = Some(pos.x);
    s.peer_y = Some(pos.y);
    save_settings(&s);
}

/// Forget the manual position: the friend's cat goes back next to our own cat.
#[tauri::command]
fn reset_peer_window_pos(app: AppHandle, state: State<'_, AppState>) {
    let (layout, scale) = {
        let mut s = state.settings.lock().unwrap();
        s.peer_x = None;
        s.peer_y = None;
        save_settings(&s);
        (s.keyboard_layout.clone(), s.scale)
    };
    apply_peer_window_geometry(&app, &layout, scale);
}

#[tauri::command]
fn net_status() -> serde_json::Value {
    serde_json::json!({
        "connected": net::is_connected(),
        "hosting": net::relay_running(),
    })
}

#[tauri::command]
fn update_settings(app: AppHandle, state: State<'_, AppState>, mut patch: AppSettings) -> AppSettings {
    patch.scale = patch.scale.clamp(0.20, 1.50);
    let clean_breed = sanitize_cat_id(&patch.cat_breed);
    patch.cat_breed = if cat_exists(&clean_breed) {
        clean_breed
    } else {
        "black".to_string()
    };
    if patch.mcp_port == 0 {
        patch.mcp_port = 27420;
    }
    if patch.obs_port == 0 {
        patch.obs_port = 27421;
    }
    if !patch.obs_enabled {
        patch.obs_hide_pet = false;
    }
    patch.keyboard_layout = normalize_keyboard_layout(&patch.keyboard_layout);
    patch.keyboard_orientation = normalize_keyboard_orientation(&patch.keyboard_orientation);
    patch.tail_key_mode = normalize_tail_key_mode(&patch.tail_key_mode);
    patch.tail_key_ids = normalize_tail_key_ids(&patch.tail_key_ids);
    patch.render_mode = config::normalize_render_mode(&patch.render_mode);
    patch.collar_color = config::normalize_collar_color(&patch.collar_color);
    patch.headphone_mode = config::normalize_headphone_mode(&patch.headphone_mode);
    patch.net_server = config::normalize_net_server(&patch.net_server);
    patch.net_room = config::normalize_net_room(&patch.net_room);
    patch.net_name = config::normalize_net_name(&patch.net_name);
    if patch.net_port == 0 {
        patch.net_port = net::DEFAULT_PORT;
    }
    patch.fps_cap = config::normalize_fps_cap(patch.fps_cap);
    patch.language = config::normalize_language(&patch.language);

    let (prev_autostart, prev_scale, prev_layout, prev_obs_hide, prev_language) = {
        let s = state.settings.lock().unwrap();
        (
            s.autostart,
            s.scale,
            s.keyboard_layout.clone(),
            s.obs_enabled && s.obs_hide_pet,
            s.language.clone(),
        )
    };
    if patch.autostart != prev_autostart {
        let _ = set_windows_autostart(patch.autostart);
    }

    state.mcp_enabled.store(patch.mcp_enabled, Ordering::Relaxed);
    obs::sync_obs_server(&app, patch.obs_enabled, patch.obs_port);

    let next_obs_hide = patch.obs_enabled && patch.obs_hide_pet;
    if next_obs_hide != prev_obs_hide && !state.paused_guard.load(Ordering::Relaxed) {
        set_pet_visible(&app, !next_obs_hide);
    }

    let size_changed = (patch.scale - prev_scale).abs() > 0.005 || patch.keyboard_layout != prev_layout;
    set_pet_always_on_top(&app, patch.always_on_top);
    if let Some(win) = app.get_webview_window("main") {
        if size_changed {
            if let Some((nx, ny)) =
                resize_window_anchored_bottom_right(&win, &patch.keyboard_layout, patch.scale)
            {
                patch.x = Some(nx);
                patch.y = Some(ny);
            }
        }
    }

    {
        let mut s = state.settings.lock().unwrap();
        #[cfg(target_os = "macos")]
        {
            // Display placement is changed by the native menu and window drag, not by web settings.
            patch.display_id = s.display_id.clone();
            if patch.display_id.is_some() || !size_changed {
                patch.x = s.x;
                patch.y = s.y;
            }
        }
        #[cfg(windows)]
        {
            patch.windows_display_id = s.windows_display_id.clone();
            if !size_changed {
                patch.x = s.x;
                patch.y = s.y;
            }
        }
        *s = patch.clone();
        save_settings(&s);
    }
    let _ = app.emit("kuro-settings", patch.clone());
    if patch.language != prev_language {
        tray::refresh_tray_language(&app);
    }
    if size_changed {
        reposition_context_menu_if_open(&app);
    }
    patch
}

const SIDE_MENU_LOGICAL_W: f64 = 276.0;

/// Computes a non-overlapping physical position `(mx, my)` for the floating menu popover
/// beside the cat window: prefers LEFT (since the cat defaults to bottom-right), then RIGHT,
/// then ABOVE, never covering the cat when screen space allows.
fn compute_side_menu_pos(
    pet_rect: (i32, i32, i32, i32),
    work_area: (i32, i32, i32, i32),
    menu_size: (i32, i32),
    gap: i32,
    pad: i32,
) -> (i32, i32) {
    let (px, py, pw, ph) = pet_rect;
    let (wx, wy, ww, wh) = work_area;
    let (mw, mh) = menu_size;

    let min_x = wx + pad;
    let max_x = (wx + ww - mw - pad).max(min_x);
    let min_y = wy + pad;
    let max_y = (wy + wh - mh - pad).max(min_y);

    let space_left = (px - gap) - min_x;
    let space_right = (wx + ww - pad) - (px + pw + gap);

    if space_left >= mw {
        let mx = px - mw - gap;
        let my = (py + ph - mh).clamp(min_y, max_y);
        return (mx.clamp(min_x, max_x), my);
    }
    if space_right >= mw {
        let mx = px + pw + gap;
        let my = (py + ph - mh).clamp(min_y, max_y);
        return (mx.clamp(min_x, max_x), my);
    }

    // Tight horizontal screen fallback: try placing directly above the cat
    let space_above = (py - gap) - min_y;
    if space_above >= mh {
        let mx = (px + (pw - mw) / 2).clamp(min_x, max_x);
        let my = (py - mh - gap).clamp(min_y, max_y);
        return (mx, my);
    }

    // Last resort on very small viewports: pick side with more space, clamped on-screen
    let mx = if space_left >= space_right { min_x } else { max_x };
    let my = (py + ph - mh).clamp(min_y, max_y);
    (mx, my)
}

fn current_pet_rect_and_monitor(app: &AppHandle) -> Option<((i32, i32, i32, i32), (i32, i32, i32, i32), f64)> {
    let main_win = app.get_webview_window("main")?;
    let sf = main_win.scale_factor().unwrap_or(1.0);

    #[cfg(windows)]
    let pet_rect = if win_needs_win7_ulw() {
        win7_layered::get_rect().map(|(x, y, w, h)| (x, y, w as i32, h as i32))
    } else {
        match (main_win.outer_position().ok(), main_win.outer_size().ok()) {
            (Some(p), Some(sz)) => Some((p.x, p.y, sz.width as i32, sz.height as i32)),
            _ => None,
        }
    };
    #[cfg(not(windows))]
    let pet_rect = match (main_win.outer_position().ok(), main_win.outer_size().ok()) {
        (Some(p), Some(sz)) => Some((p.x, p.y, sz.width as i32, sz.height as i32)),
        _ => None,
    };

    let pet_rect = pet_rect?;
    let monitor = main_win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| main_win.primary_monitor().ok().flatten());

    let work_area = if let Some(mon) = monitor {
        let mp = mon.position();
        let ms = mon.size();
        let taskbar_margin = (46.0 * sf).round() as i32;
        (mp.x, mp.y, ms.width as i32, (ms.height as i32 - taskbar_margin).max(320))
    } else {
        (0, 0, (1920.0 * sf).round() as i32, (1030.0 * sf).round() as i32)
    };

    Some((pet_rect, work_area, sf))
}

fn side_menu_geometry(app: &AppHandle) -> Option<(i32, i32, f64, f64)> {
    let (pet_rect, work_area, sf) = current_pet_rect_and_monitor(app)?;
    let mon_logical_h = work_area.3 as f64 / sf.max(0.5);
    let menu_logical_h = (mon_logical_h - 24.0).clamp(360.0, 686.0);
    let mw = (SIDE_MENU_LOGICAL_W * sf).round() as i32;
    let mh = (menu_logical_h * sf).round() as i32;
    let gap = (10.0 * sf).round() as i32;
    let pad = (10.0 * sf).round() as i32;
    let (mx, my) = compute_side_menu_pos(pet_rect, work_area, (mw, mh), gap, pad);
    Some((mx, my, SIDE_MENU_LOGICAL_W, menu_logical_h))
}

fn reposition_context_menu_if_open(app: &AppHandle) {
    let Some(menu_win) = app.get_webview_window("menu") else {
        return;
    };
    if !menu_win.is_visible().unwrap_or(false) {
        return;
    }
    if let Some((mx, my, lw, lh)) = side_menu_geometry(app) {
        let _ = menu_win.set_size(Size::Logical(LogicalSize::new(lw, lh)));
        let _ = menu_win.set_position(PhysicalPosition::new(mx, my));
    }
}

#[cfg(windows)]
fn win_is_menu_still_focused(menu_win: &tauri::WebviewWindow) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsChild};
    let Ok(raw_hwnd) = menu_win.hwnd() else {
        return false;
    };
    let menu_hwnd = raw_hwnd.0 as windows_sys::Win32::Foundation::HWND;
    if menu_hwnd.is_null() {
        return false;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_null() {
        return false;
    }
    fg == menu_hwnd || unsafe { IsChild(menu_hwnd, fg) != 0 }
}

/// IMPORTANT: Must be `async fn` so Tauri executes it off the Windows UI main thread!
/// Calling `WebviewWindowBuilder::build()` inside a synchronous `#[tauri::command]`
/// deadlocks the Windows WebView2 / tao event loop (freezing right-click, left-drag, and tray exit).
#[tauri::command]
async fn open_context_menu(app: AppHandle) -> Result<(), String> {
    let Some((mx, my, lw, lh)) = side_menu_geometry(&app) else {
        return Err("side_menu_geometry returned None".into());
    };

    struct ResetFlag(Arc<AtomicBool>);
    impl Drop for ResetFlag {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    // Cancel any pending idle destroy before reusing or building the side menu.
    app.state::<AppState>()
        .menu_close_gen
        .fetch_add(1, Ordering::AcqRel);

    let menu_win = if let Some(existing) = app.get_webview_window("menu") {
        existing
    } else {
        let creating_flag = app.state::<AppState>().menu_creating.clone();
        if creating_flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        let _reset_guard = ResetFlag(creating_flag);

        // Re-check after acquiring creation guard in case a concurrent call just finished.
        if let Some(existing) = app.get_webview_window("menu") {
            existing
        } else {
            #[cfg(windows)]
            let use_transparent = !win_needs_win7_ulw();
            #[cfg(not(windows))]
            let use_transparent = true;

            let created = match tauri::WebviewWindowBuilder::new(
                &app,
                "menu",
                tauri::WebviewUrl::App("menu.html".into()),
            )
            .title("KuroCat Menu")
            .inner_size(lw, lh)
            .decorations(false)
            .resizable(false)
            .transparent(use_transparent)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible(false)
            // MUST match `tauri.conf.json` main window's `additionalBrowserArgs` identically;
            // otherwise Windows WebView2 rejects creating a 2nd window in the same userDataFolder
            // with HRESULT 0x8007139F (ERROR_INVALID_STATE)!
            .additional_browser_args(WEBVIEW_BROWSER_ARGS)
            .build()
            {
                Ok(w) => w,
                Err(err) => {
                    eprintln!("[kurocat] open_context_menu: failed to build menu window: {err}");
                    return Err(err.to_string());
                }
            };

            let app_ev = app.clone();
            created.on_window_event(move |event| match event {
                tauri::WindowEvent::Focused(false) => {
                    let state = app_ev.state::<AppState>();
                    let elapsed = state.menu_opened_at.lock().unwrap().elapsed();
                    if elapsed >= Duration::from_millis(360) {
                        if let Some(w) = app_ev.get_webview_window("menu") {
                            #[cfg(windows)]
                            if win_is_menu_still_focused(&w) {
                                return;
                            }
                            let _ = w.hide();
                        }
                        state.menu_open.store(false, Ordering::Relaxed);
                        schedule_side_menu_idle_destroy(app_ev.clone());
                    }
                }
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    if let Some(w) = app_ev.get_webview_window("menu") {
                        let _ = w.hide();
                    }
                    app_ev
                        .state::<AppState>()
                        .menu_open
                        .store(false, Ordering::Relaxed);
                    schedule_side_menu_idle_destroy(app_ev.clone());
                }
                _ => {}
            });
            created
        }
    };

    let state = app.state::<AppState>();
    *state.menu_opened_at.lock().unwrap() = Instant::now();
    state.menu_open.store(true, Ordering::Relaxed);

    let _ = menu_win.set_size(Size::Logical(LogicalSize::new(lw, lh)));
    let _ = menu_win.set_position(PhysicalPosition::new(mx, my));
    let _ = menu_win.set_always_on_top(true);
    let _ = menu_win.show();
    let _ = menu_win.set_focus();
    let _ = app.emit("kuro-menu-opened", ());
    Ok(())
}

#[tauri::command]
fn close_context_menu(app: AppHandle) {
    if let Some(menu_win) = app.get_webview_window("menu") {
        let _ = menu_win.hide();
    }
    app.state::<AppState>()
        .menu_open
        .store(false, Ordering::Relaxed);
    schedule_side_menu_idle_destroy(app);
}

#[tauri::command]
fn open_tail_picker_from_menu(app: AppHandle) {
    close_context_menu(app.clone());
    set_pet_visible(&app, true);
    let _ = app.emit("kuro-open-tail-picker", ());
}

#[tauri::command]
fn preview_scale(app: AppHandle, scale: f64) {
    let clamped = scale.clamp(0.20, 1.50);
    let _ = app.emit("kuro-scale-preview", clamped);
}

#[tauri::command]
fn get_obs_url(state: State<'_, AppState>) -> String {
    let port = state.settings.lock().unwrap().obs_port;
    obs::obs_url(port)
}

#[tauri::command]
fn set_menu_open(state: State<'_, AppState>, open: bool) {
    state.menu_open.store(open, Ordering::Relaxed);
}

#[tauri::command]
fn set_tail_picker_bounds(state: State<'_, AppState>, bounds: Option<TailPickerBounds>) {
    *state.tail_picker_bounds.lock().unwrap() = bounds;
}

#[tauri::command]
fn save_tail_picker_selection(
    app: AppHandle,
    state: State<'_, AppState>,
    key_ids: Vec<String>,
) -> Result<AppSettings, String> {
    let updated = {
        let mut settings = state.settings.lock().unwrap();
        let mut updated = settings.clone();
        updated.tail_key_mode = "custom".to_string();
        updated.tail_key_ids = normalize_tail_key_ids(&key_ids);
        try_save_settings(&updated).map_err(|error| format!("保存自定义尾巴按键失败: {error}"))?;
        *settings = updated.clone();
        updated
    };
    let _ = app.emit("kuro-settings", updated.clone());
    Ok(updated)
}

#[tauri::command]
fn set_dragging(state: State<'_, AppState>, dragging: bool) {
    state.dragging.store(dragging, Ordering::Relaxed);
}

/// Native 120Hz unclamped pointer drag on Windows:
/// Bypasses Windows' `WM_NCLBUTTONDOWN(HTCAPTION)` titlebar drag loop so Aero Snap never fights
/// the user and the transparent headspace above the cat's ears can smoothly slide above `y = 0`,
/// allowing the cat itself to reach the very top edge of any monitor cleanly.
/// Returns `true` when handled natively (Windows); `false` on other platforms so the frontend
/// falls back to Tauri's `startDragging()`.
#[tauri::command]
async fn start_pet_drag(app: AppHandle) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetCursorPos, GetSystemMetrics, SetWindowPos, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
        };

        let Some(win) = app.get_webview_window("main") else {
            return false;
        };
        let is_win7 = win_needs_win7_ulw();
        let hwnd_raw: isize = match win.hwnd() {
            Ok(h) => h.0 as isize,
            Err(_) => return false,
        };
        let (start_wx, start_wy, win_w, win_h) = if is_win7 {
            match win7_layered::get_rect() {
                Some((x, y, w, h)) => (x, y, w, h),
                None => return false,
            }
        } else {
            let Ok(pos) = win.outer_position() else {
                return false;
            };
            let Ok(sz) = win.outer_size() else {
                return false;
            };
            (pos.x, pos.y, sz.width as i32, sz.height as i32)
        };

        let mut start_pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut start_pt) } == 0 {
            return false;
        }
        let offset_x = start_pt.x - start_wx;
        let offset_y = start_pt.y - start_wy;

        let state = app.state::<AppState>();
        state.dragging.store(true, Ordering::Relaxed);
        let dragging_flag = state.dragging.clone();

        let _ = tauri::async_runtime::spawn_blocking(move || {
            let mut last_x = start_wx;
            let mut last_y = start_wy;
            let hwnd = hwnd_raw as windows_sys::Win32::Foundation::HWND;

            loop {
                let lbtn_down = unsafe { (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0 };
                if !lbtn_down {
                    break;
                }
                let mut pt = POINT { x: 0, y: 0 };
                if unsafe { GetCursorPos(&mut pt) } != 0 {
                    let (v_left, v_top, v_w, v_h) = unsafe {
                        (
                            GetSystemMetrics(SM_XVIRTUALSCREEN),
                            GetSystemMetrics(SM_YVIRTUALSCREEN),
                            GetSystemMetrics(SM_CXVIRTUALSCREEN),
                            GetSystemMetrics(SM_CYVIRTUALSCREEN),
                        )
                    };
                    let (nx, ny) = clamp_pet_drag_position(
                        pt.x - offset_x,
                        pt.y - offset_y,
                        win_w,
                        win_h,
                        v_left,
                        v_top,
                        v_w,
                        v_h,
                    );
                    if nx != last_x || ny != last_y {
                        last_x = nx;
                        last_y = ny;
                        if is_win7 {
                            win7_layered::set_position(nx, ny);
                            if win7_layered::is_overlay_active() {
                                unsafe {
                                    SetWindowPos(
                                        hwnd,
                                        std::ptr::null_mut(),
                                        nx,
                                        ny,
                                        0,
                                        0,
                                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                                    );
                                }
                            }
                        } else {
                            unsafe {
                                SetWindowPos(
                                    hwnd,
                                    std::ptr::null_mut(),
                                    nx,
                                    ny,
                                    0,
                                    0,
                                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                                );
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(8));
            }
            dragging_flag.store(false, Ordering::Relaxed);
        })
        .await;

        save_window_pos(app);
        true
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        false
    }
}

#[tauri::command]
fn set_update_banner_open(state: State<'_, AppState>, open: bool) {
    state.update_banner_open.store(open, Ordering::Relaxed);
}

#[tauri::command]
fn get_app_version() -> String {
    updater::CURRENT_VERSION.to_string()
}

#[tauri::command]
fn get_cached_update_info() -> Option<updater::UpdateInfo> {
    updater::get_cached_update_info()
}

#[tauri::command]
async fn check_for_update() -> Result<updater::UpdateInfo, String> {
    tauri::async_runtime::spawn_blocking(updater::check_latest_update)
        .await
        .map_err(|e| format!("检查更新任务异常: {}", e))?
}

#[tauri::command]
async fn install_update(app: AppHandle, info: updater::UpdateInfo) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || updater::perform_update_install(&app, info))
        .await
        .map_err(|e| format!("安装更新任务异常: {}", e))?
}

#[tauri::command]
fn open_release_page(url: String) {
    updater::open_url_in_browser(&url);
}

#[tauri::command]
#[cfg(windows)]
fn save_window_pos(app: AppHandle) {
    if win_needs_win7_ulw() {
        if let Some((x, y, _, _)) = win7_layered::get_rect() {
            windows_display::remember_dragged_position(&app, PhysicalPosition::new(x, y));
        }
    } else if let Some(position) = app
        .get_webview_window("main")
        .and_then(|win| win.outer_position().ok())
    {
        windows_display::remember_dragged_position(&app, position);
    }
}

#[tauri::command]
#[cfg(target_os = "macos")]
fn save_window_pos(app: AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = handle.get_webview_window("main") else {
            return;
        };
        let Ok(position) = win.outer_position() else {
            return;
        };
        let state = handle.state::<AppState>();
        let unpinned = {
            let mut settings = state.settings.lock().unwrap();
            let unpinned = settings
                .display_id
                .as_deref()
                .is_some_and(|id| !macos_window::is_at_display_corner(&win, id));
            if unpinned {
                settings.display_id = None;
            }
            settings.x = Some(position.x);
            settings.y = Some(position.y);
            save_settings(&settings);
            unpinned.then(|| settings.clone())
        };
        if let Some(settings) = unpinned {
            let _ = macos_window::refresh_menu(&handle);
            let _ = handle.emit("kuro-settings", settings);
        }
    });
}

#[tauri::command]
#[cfg(all(not(windows), not(target_os = "macos")))]
fn save_window_pos(app: AppHandle, state: State<'_, AppState>) {
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(pos) = win.outer_position() {
            let mut s = state.settings.lock().unwrap();
            s.x = Some(pos.x);
            s.y = Some(pos.y);
            save_settings(&s);
        }
    }
}

#[tauri::command]
fn reset_position(app: AppHandle) {
    reset_window_position(&app);
}

#[tauri::command]
fn create_desktop_shortcut() -> Result<String, String> {
    config::create_desktop_shortcut()
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    shutdown_pet_windows();
    app.exit(0);
}

pub fn apply_breed_and_save(app: &AppHandle, breed: &str) {
    let clean = sanitize_cat_id(breed);
    let normalized = if cat_exists(&clean) {
        clean
    } else {
        "black".to_string()
    };
    let state = app.state::<AppState>();
    let updated = {
        let mut s = state.settings.lock().unwrap();
        s.cat_breed = normalized;
        save_settings(&s);
        s.clone()
    };
    let _ = app.emit("kuro-settings", updated);
}

pub fn reload_and_emit_all(app: &AppHandle) {
    let cats = all_cats();
    let state = app.state::<AppState>();

    let (disk_settings, size_changed, top_changed, display_changed) = {
        let mut s = state.settings.lock().unwrap();
        let disk_settings = load_settings();
        let sc = (s.scale - disk_settings.scale).abs() > 0.01
            || s.keyboard_layout != disk_settings.keyboard_layout;
        let tc = s.always_on_top != disk_settings.always_on_top;
        #[cfg(target_os = "macos")]
        let display_changed = s.display_id != disk_settings.display_id;
        #[cfg(windows)]
        let display_changed = s.windows_display_id != disk_settings.windows_display_id;
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let display_changed = false;
        *s = disk_settings.clone();
        (disk_settings, sc, tc, display_changed)
    };
    state
        .mcp_enabled
        .store(disk_settings.mcp_enabled, Ordering::Relaxed);

    if top_changed {
        set_pet_always_on_top(app, disk_settings.always_on_top);
    }
    #[cfg(target_os = "macos")]
    if display_changed {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            macos_window::restore_pinned_position(&handle);
            let _ = macos_window::refresh_menu(&handle);
        });
    }
    #[cfg(windows)]
    if display_changed {
        windows_display::refresh_placement(app);
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let _ = display_changed;
    if let Some(win) = app.get_webview_window("main") {
        if size_changed {
            let _ = resize_window_anchored_bottom_right(
                &win,
                &disk_settings.keyboard_layout,
                disk_settings.scale,
            );
        }
    }

    let _ = app.emit("kuro-cats", cats);
    let _ = app.emit("kuro-settings", disk_settings);
}

fn file_mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn start_storage_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let c_path = cats_path();
        let s_path = config_path();
        let mut last_cats_m = file_mtime(&c_path);
        let mut last_settings_m = file_mtime(&s_path);

        loop {
            std::thread::sleep(Duration::from_millis(400));
            let cur_c = file_mtime(&c_path);
            let cur_s = file_mtime(&s_path);
            if cur_c != last_cats_m || cur_s != last_settings_m {
                last_cats_m = cur_c;
                last_settings_m = cur_s;
                reload_and_emit_all(&app);
            }
        }
    });
}

pub fn apply_scale_and_save(app: &AppHandle, scale: f64) {
    let clamped = scale.clamp(0.20, 1.50);
    let state = app.state::<AppState>();
    let layout = { state.settings.lock().unwrap().keyboard_layout.clone() };
    let new_pos = app
        .get_webview_window("main")
        .and_then(|win| resize_window_anchored_bottom_right(&win, &layout, clamped));
    let updated = {
        let mut s = state.settings.lock().unwrap();
        s.scale = clamped;
        if let Some((nx, ny)) = new_pos {
            s.x = Some(nx);
            s.y = Some(ny);
        }
        save_settings(&s);
        s.clone()
    };
    let _ = app.emit("kuro-settings", updated);
}

pub fn apply_keyboard_layout_and_save(app: &AppHandle, layout: &str) {
    let normalized = normalize_keyboard_layout(layout);
    let state = app.state::<AppState>();
    let cur_scale = { state.settings.lock().unwrap().scale };
    let new_pos = app
        .get_webview_window("main")
        .and_then(|win| resize_window_anchored_bottom_right(&win, &normalized, cur_scale));
    let updated = {
        let mut s = state.settings.lock().unwrap();
        s.keyboard_layout = normalized;
        if let Some((nx, ny)) = new_pos {
            s.x = Some(nx);
            s.y = Some(ny);
        }
        save_settings(&s);
        s.clone()
    };
    let _ = app.emit("kuro-settings", updated);
}

pub fn apply_keyboard_orientation_and_save(app: &AppHandle, orientation: &str) {
    let normalized = normalize_keyboard_orientation(orientation);
    let state = app.state::<AppState>();
    let updated = {
        let mut s = state.settings.lock().unwrap();
        s.keyboard_orientation = normalized;
        save_settings(&s);
        s.clone()
    };
    let _ = app.emit("kuro-settings", updated);
}

pub fn apply_tail_key_mode_and_save(app: &AppHandle, mode: &str) {
    let normalized = normalize_tail_key_mode(mode);
    let state = app.state::<AppState>();
    let updated = {
        let mut s = state.settings.lock().unwrap();
        s.tail_key_mode = normalized;
        save_settings(&s);
        s.clone()
    };
    let _ = app.emit("kuro-settings", updated);
}

#[cfg(target_os = "macos")]
pub fn reset_window_position(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = handle.get_webview_window("main") else {
            return;
        };
        let state = handle.state::<AppState>();
        let mut settings = state.settings.lock().unwrap();
        if macos_window::place(&win, settings.display_id.as_deref()) {
            if let Ok(position) = win.outer_position() {
                settings.x = Some(position.x);
                settings.y = Some(position.y);
                save_settings(&settings);
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
pub fn reset_window_position(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        #[cfg(windows)]
        if windows_display::reset_position(app, &win) {
            return;
        }
        let size = win.outer_size().unwrap_or(tauri::PhysicalSize::new(420, 440));
        let sf = win.scale_factor().unwrap_or(1.0);
        #[cfg(windows)]
        let (w_px, h_px) = if win_needs_win7_ulw() {
            win7_layered::get_rect()
                .map(|(_, _, w, h)| (w, h))
                .unwrap_or((size.width as i32, size.height as i32))
        } else {
            (size.width as i32, size.height as i32)
        };
        #[cfg(not(windows))]
        let (w_px, h_px) = (size.width as i32, size.height as i32);

        let (target_x, target_y) = default_bottom_right_position(w_px, h_px, sf);
        #[cfg(windows)]
        if win_needs_win7_ulw() {
            win7_layered::set_position(target_x, target_y);
            if win7_layered::is_overlay_active() {
                let _ = win.set_position(PhysicalPosition::new(target_x, target_y));
            }
        } else {
            let _ = win.set_position(PhysicalPosition::new(target_x, target_y));
        }
        #[cfg(not(windows))]
        {
            let _ = win.set_position(PhysicalPosition::new(target_x, target_y));
        }

        let state = app.state::<AppState>();
        let mut s = state.settings.lock().unwrap();
        s.x = Some(target_x);
        s.y = Some(target_y);
        save_settings(&s);
    }
}

#[cfg(windows)]
fn default_bottom_right_position(win_w: i32, win_h: i32, _scale_factor: f64) -> (i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        ((sw - win_w - 28).max(20), (sh - win_h - 68).max(20))
    }
}

#[cfg(target_os = "linux")]
fn default_bottom_right_position(win_w: i32, win_h: i32, _scale_factor: f64) -> (i32, i32) {
    let (sw, sh) = input_hook::linux_hook::primary_screen_size();
    ((sw - win_w - 28).max(20), (sh - win_h - 68).max(20))
}

#[cfg(all(not(windows), not(target_os = "macos"), not(target_os = "linux")))]
fn default_bottom_right_position(_win_w: i32, _win_h: i32, _scale_factor: f64) -> (i32, i32) {
    (900, 500)
}

#[cfg(windows)]
fn get_cursor_pos(_scale_factor: f64) -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) != 0 {
            Some((pt.x, pt.y))
        } else {
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn get_cursor_pos(scale_factor: f64) -> Option<(i32, i32)> {
    input_hook::macos_hook::cursor_pos_points().map(|(x, y)| {
        (
            (x * scale_factor).round() as i32,
            (y * scale_factor).round() as i32,
        )
    })
}

#[cfg(target_os = "linux")]
fn get_cursor_pos(_scale_factor: f64) -> Option<(i32, i32)> {
    input_hook::linux_hook::cursor_pos()
}

#[cfg(all(not(windows), not(target_os = "macos"), not(target_os = "linux")))]
fn get_cursor_pos(_scale_factor: f64) -> Option<(i32, i32)> {
    None
}

#[cfg(windows)]
fn win_is_click_through(win: &tauri::WebviewWindow) -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TRANSPARENT,
    };
    let hwnd = win.hwnd().ok()?.0 as windows_sys::Win32::Foundation::HWND;
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    Some((ex & (WS_EX_TRANSPARENT as isize)) != 0)
}

#[cfg(windows)]
fn win_force_click_through(win: &tauri::WebviewWindow, ignore: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_LAYERED, WS_EX_TRANSPARENT,
    };
    let Ok(raw_hwnd) = win.hwnd() else {
        return;
    };
    let hwnd = raw_hwnd.0 as windows_sys::Win32::Foundation::HWND;
    let is_win7 = win_needs_win7_ulw();
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_ex = if ignore {
            ex | (WS_EX_TRANSPARENT as isize) | (WS_EX_LAYERED as isize)
        } else if is_win7 {
            // Keep the offscreen/overlay WebView2 HWND free of WS_EX_LAYERED so popup overlays
            // (context menu / toast) never hit Win7's layered-alpha=0 invisibility quirk.
            ex & !(WS_EX_TRANSPARENT as isize) & !(WS_EX_LAYERED as isize)
        } else {
            ex & !(WS_EX_TRANSPARENT as isize)
        };
        if new_ex != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
            let _ = SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

#[cfg(windows)]
fn win_foreground_hwnd() -> windows_sys::Win32::Foundation::HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow() }
}

#[cfg(windows)]
fn win_is_topmost(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TOPMOST,
    };
    if hwnd.is_null() {
        return false;
    }
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    (ex & (WS_EX_TOPMOST as isize)) != 0
}

#[cfg(windows)]
fn win_is_iconic(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
    if hwnd.is_null() {
        return false;
    }
    unsafe { IsIconic(hwnd) != 0 }
}

#[cfg(windows)]
fn win_is_ignored_shell_window(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return false;
    }
    let class_name = String::from_utf16_lossy(&buf[..len as usize]);
    matches!(
        class_name.as_str(),
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Progman"
            | "WorkerW"
            | "Windows.UI.Core.CoreWindow"
            | "XamlExplorerHostIslandWindow"
            | "MultitaskingViewFrame"
            | "TaskListThumbnailWnd"
    )
}

#[cfg(windows)]
fn win_rects_overlap(
    a: windows_sys::Win32::Foundation::HWND,
    b: windows_sys::Win32::Foundation::HWND,
) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut ra = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut rb = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        if GetWindowRect(a, &mut ra) == 0 || GetWindowRect(b, &mut rb) == 0 {
            return true;
        }
    }
    ra.left < rb.right && ra.right > rb.left && ra.top < rb.bottom && ra.bottom > rb.top
}

/// Inspired by BongoCat ADR-0096: determine whether the foreground window is
/// currently stacked above our pet window in Windows Z-order using early-exit
/// synchronous EnumWindows (covering both WS_EX_TOPMOST fullscreen windows and
/// Shell-elevated borderless / FSO fullscreen windows on any monitor).
#[cfg(windows)]
fn win_window_is_above(
    candidate: windows_sys::Win32::Foundation::HWND,
    model: windows_sys::Win32::Foundation::HWND,
) -> bool {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;

    if candidate.is_null() || model.is_null() || candidate == model {
        return false;
    }
    if win_is_ignored_shell_window(candidate) || !win_rects_overlap(candidate, model) {
        return false;
    }

    #[repr(C)]
    struct OrderCtx {
        candidate: HWND,
        model: HWND,
        candidate_above: bool,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam as *mut OrderCtx);
        if hwnd == ctx.candidate {
            ctx.candidate_above = true;
            0
        } else if hwnd == ctx.model {
            0
        } else {
            1
        }
    }

    let mut ctx = OrderCtx {
        candidate,
        model,
        candidate_above: false,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), (&mut ctx as *mut OrderCtx) as LPARAM);
    }
    ctx.candidate_above
}

#[cfg(windows)]
pub fn win_gentle_reassert_topmost(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    let Ok(raw_hwnd) = win.hwnd() else {
        return;
    };
    let hwnd = raw_hwnd.0 as windows_sys::Win32::Foundation::HWND;
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

#[cfg(windows)]
pub fn win_is_elevated() -> bool {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{BOOL, CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(
            process_handle: HANDLE,
            desired_access: u32,
            token_handle: *mut HANDLE,
        ) -> BOOL;
        fn GetTokenInformation(
            token_handle: HANDLE,
            token_information_class: u32,
            token_information: *mut c_void,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> BOOL;
    }

    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION_CLASS: u32 = 20;

    #[repr(C)]
    struct TokenElevation {
        token_is_elevated: u32,
    }

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 || token.is_null() {
            return false;
        }
        let mut elevation = TokenElevation {
            token_is_elevated: 0,
        };
        let mut ret_len: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TOKEN_ELEVATION_CLASS,
            (&mut elevation as *mut TokenElevation).cast(),
            std::mem::size_of::<TokenElevation>() as u32,
            &mut ret_len,
        );
        let _ = CloseHandle(token);
        ok != 0 && elevation.token_is_elevated != 0
    }
}

#[cfg(windows)]
pub fn win_restart_as_admin(app: AppHandle) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: HWND,
            lp_operation: *const u16,
            lp_file: *const u16,
            lp_parameters: *const u16,
            lp_directory: *const u16,
            n_show_cmd: i32,
        ) -> isize;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let current_pid = std::process::id();

    std::thread::spawn(move || {
        let to_wide = |s: &std::ffi::OsStr| -> Vec<u16> {
            s.encode_wide().chain(std::iter::once(0)).collect()
        };
        let verb = to_wide(std::ffi::OsStr::new("runas"));
        let file = to_wide(exe.as_os_str());
        let params_str = format!("--wait-pid {}", current_pid);
        let params = to_wide(std::ffi::OsStr::new(&params_str));
        let dir = exe.parent().map(|p| to_wide(p.as_os_str()));
        let dir_ptr = dir.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());

        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                dir_ptr,
                SW_SHOWNORMAL,
            )
        };
        if result > 32 {
            crate::input_hook::set_hooks_paused(true);
            app.exit(0);
        }
    });
}

#[cfg(windows)]
fn win_wait_for_previous_pid_if_requested() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

    const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;
    let mut wait_pid: Option<u32> = None;
    let mut cleanup_old_exe: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--wait-pid" {
            if let Some(pid_str) = args.next() {
                wait_pid = pid_str.parse::<u32>().ok();
            }
        } else if arg == "--cleanup-old-exe" {
            cleanup_old_exe = args.next();
        }
    }

    if let Some(pid) = wait_pid {
        if pid != 0 && pid != std::process::id() {
            unsafe {
                let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
                if !handle.is_null() {
                    let _ = WaitForSingleObject(handle, 5000);
                    let _ = CloseHandle(handle);
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    if let Some(old_path) = cleanup_old_exe {
        let path = std::path::PathBuf::from(old_path);
        for _ in 0..10 {
            if !path.exists() || std::fs::remove_file(&path).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn is_over_interactive_silhouette(rx: f64, ry: f64, is_108: bool) -> (bool, bool) {
    // Cat body/head oval centered around (0.48, 0.49) for 65% or (0.51, 0.48) for 108-key
    let (cat_cx, cat_rx) = if is_108 { (0.51, 0.21) } else { (0.48, 0.25) };
    let dx_cat = (rx - cat_cx) / cat_rx;
    let dy_cat = (ry - 0.49) / 0.27;
    let over_cat = (dx_cat * dx_cat + dy_cat * dy_cat) <= 1.0;

    // Keyboard & mouse lower area
    let over_desk = if is_108 {
        rx >= 0.04 && rx <= 0.94 && ry >= 0.65 && ry <= 0.94
    } else {
        rx >= 0.06 && rx <= 0.92 && ry >= 0.65 && ry <= 0.91
    };

    (over_cat || over_desk, over_cat)
}

fn tail_picker_captures_cursor(
    bounds: Option<TailPickerBounds>,
    rx: f64,
    ry: f64,
    allow_keyboard: bool,
) -> bool {
    let Some(bounds) = bounds else {
        return false;
    };
    let contains = |[left, top, right, bottom]: [f64; 4]| {
        (left..=right).contains(&rx) && (top..=bottom).contains(&ry)
    };
    if contains(bounds.toolbar) || (allow_keyboard && contains(bounds.keyboard)) {
        return true;
    }

    let [left, top, right, bottom] = bounds.close_button;
    let radius_x = (right - left) * 0.5;
    let radius_y = (bottom - top) * 0.5;
    // The close button is circular in screen pixels, with separate radii in normalized space.
    radius_x > 0.0
        && radius_y > 0.0
        && ((rx - left - radius_x) / radius_x).powi(2)
            + ((ry - top - radius_y) / radius_y).powi(2)
            <= 1.0
}

#[cfg(test)]
mod tail_picker_hit_tests {
    use super::{tail_picker_captures_cursor, TailPickerBounds};

    #[test]
    fn captures_picker_controls_and_keyboard_only_while_open() {
        let bounds = Some(TailPickerBounds {
            toolbar: [0.05, 0.02, 0.95, 0.16],
            keyboard: [0.13, 0.65, 0.95, 0.91],
            close_button: [0.96, 0.08, 0.995, 0.16],
        });
        assert!(tail_picker_captures_cursor(bounds, 0.87, 0.05, false));
        assert!(tail_picker_captures_cursor(bounds, 0.5, 0.8, true));
        assert!(tail_picker_captures_cursor(bounds, 0.9775, 0.12, true));
        assert!(tail_picker_captures_cursor(bounds, 0.9775, 0.12, false));
        assert!(tail_picker_captures_cursor(bounds, 0.9775, 0.152, false));
        assert!(!tail_picker_captures_cursor(bounds, 0.961, 0.081, true));
        assert!(!tail_picker_captures_cursor(bounds, 0.955, 0.12, true));
        assert!(!tail_picker_captures_cursor(bounds, 0.5, 0.8, false));
        assert!(!tail_picker_captures_cursor(bounds, 0.99, 0.05, true));
        assert!(!tail_picker_captures_cursor(bounds, 0.10, 0.89, true));
        assert!(!tail_picker_captures_cursor(bounds, 0.5, 0.45, true));
        assert!(!tail_picker_captures_cursor(None, 0.87, 0.05, false));
        assert!(!tail_picker_captures_cursor(None, 0.5, 0.8, true));
        assert!(!tail_picker_captures_cursor(None, 0.9775, 0.12, false));
    }
}

/// Ensures the top update/unlock toast pill (up to ~54px tall including top margin)
/// remains clickable even when the pet window is scaled down to 20%-36%.
pub(crate) fn banner_hit_ry_limit(win_log_h: f64) -> f64 {
    (54.0 / win_log_h.max(1.0)).clamp(0.19, 0.55)
}

fn start_hit_test_loop(
    app: AppHandle,
    settings: Arc<Mutex<AppSettings>>,
    menu_open: Arc<AtomicBool>,
    dragging: Arc<AtomicBool>,
    update_banner_open: Arc<AtomicBool>,
    paused_guard: Arc<AtomicBool>,
) {
    let tail_picker_bounds = app.state::<AppState>().tail_picker_bounds.clone();
    std::thread::spawn(move || {
        let mut last_ignore: Option<bool> = None;
    // Click-through state of the friend window (interactive only while Alt is held).
    let mut peer_last_ignore: Option<bool> = None;
        let mut last_cursor: (i32, i32) = (i32::MIN, i32::MIN);
        let mut last_over_cat = false;
        let mut last_inside_pet = false;
        let mut last_alt_override = false;
        let mut last_effective_locked = false;
        let mut last_cursor_emit = Instant::now() - Duration::from_secs(1);
        let mut sleep_ms: u64 = 25;
        #[cfg(windows)]
        let mut drift_ticks: u8 = 0;
        #[cfg(windows)]
        let mut last_drift_check = Instant::now() - Duration::from_secs(1);
        #[cfg(windows)]
        let mut last_fg_hwnd: isize = 0;
        #[cfg(windows)]
        let mut last_topmost_check = Instant::now() - Duration::from_secs(1);
        #[cfg(windows)]
        let mut last_enum_check = Instant::now() - Duration::from_secs(2);
        #[cfg(windows)]
        let mut fg_just_changed = false;
        #[cfg(windows)]
        let mut fg_above_attempts: u8 = 0;
        #[cfg(windows)]
        let mut last_topmost_reassert = Instant::now() - Duration::from_secs(5);

        loop {
            std::thread::sleep(Duration::from_millis(sleep_ms));

            if paused_guard.load(Ordering::Relaxed) {
                sleep_ms = 120;
                continue;
            }
            let Some(win) = app.get_webview_window("main") else {
                sleep_ms = 120;
                continue;
            };
            #[cfg(windows)]
            let is_win7_ulw = win_needs_win7_ulw();
            #[cfg(not(windows))]
            let is_win7_ulw = false;

            if is_win7_ulw {
                #[cfg(windows)]
                if !win7_layered::is_visible() {
                    sleep_ms = 120;
                    continue;
                }
            } else if !win.is_visible().unwrap_or(false) {
                sleep_ms = 120;
                continue;
            }
            let sf = win.scale_factor().unwrap_or(1.0);
            let Some((cx, cy)) = get_cursor_pos(sf) else {
                continue;
            };

            let (win_x, win_y, w, h) = if is_win7_ulw {
                #[cfg(windows)]
                {
                    let Some((ux, uy, uw, uh)) = win7_layered::get_rect() else {
                        continue;
                    };
                    (ux, uy, uw.max(1) as f64, uh.max(1) as f64)
                }
                #[cfg(not(windows))]
                unreachable!()
            } else {
                let Ok(pos) = win.outer_position() else {
                    continue;
                };
                let Ok(size) = win.outer_size() else {
                    continue;
                };
                (pos.x, pos.y, size.width.max(1) as f64, size.height.max(1) as f64)
            };
            let rx = (cx - win_x) as f64 / w;
            let ry = (cy - win_y) as f64 / h;
            let inside = rx >= 0.0 && rx <= 1.0 && ry >= 0.0 && ry <= 1.0;
            let (locked, always_on_top, is_108, cur_scale) = {
                let s = settings.lock().unwrap();
                (
                    s.click_through_locked,
                    s.always_on_top,
                    s.keyboard_layout == "108",
                    s.scale.clamp(0.20, 1.50),
                )
            };
            let layout_str = if is_108 { "108" } else { "65" };

            let is_menu = menu_open.load(Ordering::Relaxed);
            let picker_bounds = *tail_picker_bounds.lock().unwrap();
            let is_drag = dragging.load(Ordering::Relaxed);
            let is_banner = update_banner_open.load(Ordering::Relaxed);

            // Continuous ADR-0096-style Windows Z-order guard for borderless & FSO fullscreen games:
            // - Checks O(1) topmost flag every ~200ms (or promptly ~50ms after foreground switch)
            // - Runs EnumWindows immediately on foreground switches and every ~900ms steady-state
            //   instead of every 150ms, saving CPU during active gameplay.
            #[cfg(windows)]
            if always_on_top && !is_menu && picker_bounds.is_none() && !is_drag {
                let fg_raw = win_foreground_hwnd();
                let fg_id = fg_raw as isize;
                if fg_id != last_fg_hwnd {
                    last_fg_hwnd = fg_id;
                    fg_above_attempts = 0;
                    fg_just_changed = true;
                }
                let need_check =
                    fg_just_changed || last_topmost_check.elapsed() >= Duration::from_millis(200);
                if need_check {
                    let run_enum = fg_just_changed
                        || fg_above_attempts > 0
                        || last_enum_check.elapsed() >= Duration::from_millis(900);
                    fg_just_changed = false;
                    last_topmost_check = Instant::now();
                    let our_hwnd_opt = if is_win7_ulw {
                        win7_layered::hwnd()
                    } else {
                        win.hwnd()
                            .ok()
                            .map(|h| h.0 as windows_sys::Win32::Foundation::HWND)
                    };
                    if let Some(our_hwnd) = our_hwnd_opt {
                        if !win_is_iconic(our_hwnd) {
                            let lost_topmost = !win_is_topmost(our_hwnd);
                            let fg_above = if !lost_topmost
                                && run_enum
                                && !fg_raw.is_null()
                                && fg_raw != our_hwnd
                            {
                                last_enum_check = Instant::now();
                                win_window_is_above(fg_raw, our_hwnd)
                            } else {
                                false
                            };

                            if !lost_topmost && !fg_above {
                                if run_enum {
                                    fg_above_attempts = 0;
                                }
                            } else if lost_topmost {
                                if last_topmost_reassert.elapsed() >= Duration::from_millis(250) {
                                    if is_win7_ulw {
                                        win7_layered::reassert_topmost();
                                    } else {
                                        let app_for_top = app.clone();
                                        let _ = app.run_on_main_thread(move || {
                                            if let Some(w) = app_for_top.get_webview_window("main") {
                                                if w.is_visible().unwrap_or(false) {
                                                    win_gentle_reassert_topmost(&w);
                                                }
                                            }
                                        });
                                    }
                                    last_topmost_reassert = Instant::now();
                                }
                            } else if fg_above_attempts < 3 {
                                let cooldown = match fg_above_attempts {
                                    0 => Duration::from_millis(0),
                                    1 => Duration::from_millis(350),
                                    _ => Duration::from_millis(900),
                                };
                                if last_topmost_reassert.elapsed() >= cooldown {
                                    if is_win7_ulw {
                                        win7_layered::reassert_topmost();
                                    } else {
                                        let app_for_top = app.clone();
                                        let _ = app.run_on_main_thread(move || {
                                            if let Some(w) = app_for_top.get_webview_window("main") {
                                                if w.is_visible().unwrap_or(false) {
                                                    win_gentle_reassert_topmost(&w);
                                                }
                                            }
                                        });
                                    }
                                    last_topmost_reassert = Instant::now();
                                    fg_above_attempts = fg_above_attempts.saturating_add(1);
                                }
                            }
                        }
                    }
                }
            } else {
                fg_just_changed = false;
                fg_above_attempts = 0;
            }
            let alt_override = input_hook::is_alt_pressed();
            let effective_locked = locked && !alt_override;

            // Map window-relative (rx, ry) into exact pet-local (pet_rx, pet_ry),
            // since at small scales (20%..90%) the pet is anchored bottom-right inside a min-safe menu window!
            let (bw, bh) = base_dimensions(layout_str);
            let win_log_w = (w / sf).max(1.0);
            let win_log_h = (h / sf).max(1.0);
            let pet_w_frac = ((bw * cur_scale) / win_log_w).clamp(0.05, 1.0);
            let pet_h_frac = ((bh * cur_scale) / win_log_h).clamp(0.05, 1.0);
            let pet_x0 = 1.0 - pet_w_frac;
            let pet_y0 = 1.0 - pet_h_frac;
            let pet_rx = (rx - pet_x0) / pet_w_frac;
            let pet_ry = (ry - pet_y0) / pet_h_frac;
            let inside_pet = pet_rx >= 0.0 && pet_rx <= 1.0 && pet_ry >= 0.0 && pet_ry <= 1.0;

            let (over_interactive, raw_over_cat) = if inside_pet {
                is_over_interactive_silhouette(pet_rx, pet_ry, is_108)
            } else {
                (false, false)
            };
            // While click-through is locked (and Alt is not held), suppress petting hover so users
            // aren't misled by squinting eyes when the cat is intentionally click-through.
            let over_cat = if effective_locked { false } else { raw_over_cat };

            #[cfg(windows)]
            if is_win7_ulw {
                win7_layered::set_over_cat(over_cat);
            }

            // Friend cat window: click-through by default so it never blocks the
            // desktop; holding Alt makes it interactive so it can be dragged.
            if let Some(peer_win) = app.get_webview_window(PEER_WINDOW_LABEL) {
                if peer_win.is_visible().unwrap_or(false) {
                    let want_ignore = !input_hook::is_alt_pressed();
                    if peer_last_ignore != Some(want_ignore) {
                        let _ = peer_win.set_ignore_cursor_events(want_ignore);
                        peer_last_ignore = Some(want_ignore);
                    }
                } else if peer_last_ignore != Some(true) {
                    let _ = peer_win.set_ignore_cursor_events(true);
                    peer_last_ignore = Some(true);
                }
            }

            let over_banner = is_banner && inside && ry <= banner_hit_ry_limit(win_log_h);
            // The picker lives inside the pet window, unlike the separate side menu.
            let over_tail_picker =
                inside && tail_picker_captures_cursor(picker_bounds, rx, ry, !is_win7_ulw);

            let should_ignore = if is_drag || over_banner || over_tail_picker {
                false
            } else if effective_locked {
                true
            } else if is_win7_ulw {
                false
            } else {
                !over_interactive
            };

            if last_ignore != Some(should_ignore) {
                #[cfg(windows)]
                if is_win7_ulw {
                    win7_layered::set_click_through(should_ignore);
                } else {
                    let _ = win.set_ignore_cursor_events(should_ignore);
                }
                #[cfg(not(windows))]
                {
                    let _ = win.set_ignore_cursor_events(should_ignore);
                }
                last_ignore = Some(should_ignore);
                #[cfg(windows)]
                {
                    drift_ticks = 0;
                }
            } else {
                // Self-heal Windows WS_EX_TRANSPARENT if DWM / Wallpaper Engine / Show-Desktop drifted it
                #[cfg(windows)]
                if inside || last_drift_check.elapsed() >= Duration::from_millis(250) {
                    last_drift_check = Instant::now();
                    let actual_opt = if is_win7_ulw {
                        win7_layered::is_click_through()
                    } else {
                        win_is_click_through(&win)
                    };
                    if let Some(actual_transparent) = actual_opt {
                        if actual_transparent != should_ignore {
                            drift_ticks = drift_ticks.saturating_add(1);
                            if drift_ticks >= 3 {
                                if is_win7_ulw {
                                    win7_layered::set_click_through(should_ignore);
                                } else {
                                    win_force_click_through(&win, should_ignore);
                                    let _ = win.set_ignore_cursor_events(should_ignore);
                                }
                                drift_ticks = 0;
                            }
                        } else {
                            drift_ticks = 0;
                        }
                    }
                }
            }

            let state_changed = over_cat != last_over_cat
                || inside_pet != last_inside_pet
                || alt_override != last_alt_override
                || effective_locked != last_effective_locked;
            let cursor_moved = last_cursor.0 == i32::MIN
                || (cx - last_cursor.0).abs() > 1
                || (cy - last_cursor.1).abs() > 1;
            let game_now = game_detect::is_game_active();
            let min_emit_gap = if inside_pet {
                Duration::from_millis(30)
            } else if game_now {
                Duration::from_millis(65)
            } else {
                Duration::from_millis(45)
            };

            if state_changed || (cursor_moved && last_cursor_emit.elapsed() >= min_emit_gap) {
                last_cursor = (cx, cy);
                last_over_cat = over_cat;
                last_inside_pet = inside_pet;
                last_alt_override = alt_override;
                last_effective_locked = effective_locked;
                last_cursor_emit = Instant::now();
                let _ = app.emit(
                    "kuro-cursor",
                    WindowCursorInfo {
                        rel_x: pet_rx,
                        rel_y: pet_ry,
                        inside_window: inside_pet,
                        over_cat,
                        alt_override,
                    },
                );
            }

            // Adaptive polling interval: fast 25ms near/over pet window, low-overhead 50-65ms elsewhere!
            let near_window = rx >= -0.15 && rx <= 1.15 && ry >= -0.15 && ry <= 1.15;
            sleep_ms = if near_window || is_drag || is_menu || picker_bounds.is_some() || alt_override {
                25
            } else if game_now {
                65
            } else {
                50
            };
        }
    });
}

#[cfg(windows)]
fn win_needs_win7_ulw() -> bool {
    use std::sync::OnceLock;
    static NEEDS_ULW: OnceLock<bool> = OnceLock::new();
    *NEEDS_ULW.get_or_init(|| {
        for var in ["KUROCAT_WIN7_ULW", "KUROCAT_WIN7_MASK"] {
            if let Ok(v) = std::env::var(var) {
                let t = v.trim();
                if t == "1" || t.eq_ignore_ascii_case("true") {
                    return true;
                }
                if t == "0" || t.eq_ignore_ascii_case("false") {
                    return false;
                }
            }
        }
        // Query real OS version via ntdll!RtlGetVersion (immune to compatibility manifest lies).
        // Windows 7 = 6.1, Windows 8 = 6.2, Windows 8.1 = 6.3, Windows 10/11 = 10.0+.
        #[repr(C)]
        struct RtlOsVersionInfoW {
            dw_os_version_info_size: u32,
            dw_major_version: u32,
            dw_minor_version: u32,
            dw_build_number: u32,
            dw_platform_id: u32,
            sz_csd_version: [u16; 128],
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn RtlGetVersion(lpVersionInformation: *mut RtlOsVersionInfoW) -> i32;
        }
        unsafe {
            let mut info: RtlOsVersionInfoW = std::mem::zeroed();
            info.dw_os_version_info_size = std::mem::size_of::<RtlOsVersionInfoW>() as u32;
            if RtlGetVersion(&mut info) == 0 {
                return info.dw_major_version < 10;
            }
        }
        false
    })
}

/// Build a rounded GDI HRGN from `[left, top, right, bottom, corner_radius_px]` tuples
/// for on-demand DOM overlays (`#context-menu` and `#update-toast`) on Windows 7/8.
#[cfg(windows)]
fn build_rounded_overlay_hrgn(rects: &[i32]) -> windows_sys::Win32::Graphics::Gdi::HRGN {
    use windows_sys::Win32::Graphics::Gdi::{
        CombineRgn, CreateRoundRectRgn, DeleteObject, HRGN, RGN_OR,
    };

    let mut final_rgn: HRGN = std::ptr::null_mut();
    for chunk in rects.chunks_exact(5) {
        let (l, t, r, b, radius) = (chunk[0], chunk[1], chunk[2], chunk[3], chunk[4]);
        if r <= l || b <= t {
            continue;
        }
        let w = r - l;
        let h = b - t;
        let diam = (radius.max(0) * 2).min(w).min(h);
        unsafe {
            let ov = CreateRoundRectRgn(l, t, r + 1, b + 1, diam, diam);
            if ov.is_null() {
                continue;
            }
            if final_rgn.is_null() {
                final_rgn = ov;
            } else {
                CombineRgn(final_rgn, final_rgn, ov, RGN_OR);
                DeleteObject(ov as _);
            }
        }
    }
    final_rgn
}

#[tauri::command]
fn is_win7_ulw_mode() -> bool {
    #[cfg(windows)]
    {
        return win_needs_win7_ulw();
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[tauri::command]
fn win7_present_frame(request: tauri::ipc::Request<'_>) {
    #[cfg(windows)]
    {
        if !win_needs_win7_ulw() {
            return;
        }
        let w = request
            .headers()
            .get("x-w")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let h = request
            .headers()
            .get("x-h")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if w == 0 || h == 0 || w > 4096 || h > 4096 {
            return;
        }
        if let tauri::ipc::InvokeBody::Raw(bytes) = request.body() {
            win7_layered::submit_rgba_frame(w, h, bytes);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = request;
    }
}

#[tauri::command]
fn win7_sync_overlay(window: tauri::WebviewWindow, rects: Vec<i32>, focus_menu: bool) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Gdi::{DeleteObject, SetWindowRgn};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetForegroundWindow, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
            SWP_NOZORDER,
        };
        if !win_needs_win7_ulw() {
            return;
        }
        let Ok(raw_hwnd) = window.hwnd() else {
            return;
        };
        let main_hwnd = raw_hwnd.0 as windows_sys::Win32::Foundation::HWND;

        let rgn = build_rounded_overlay_hrgn(&rects);
        if rgn.is_null() {
            win7_layered::set_overlay_active(false);
            unsafe {
                let _ = SetWindowRgn(main_hwnd, std::ptr::null_mut(), 0);
                let _ = SetWindowPos(
                    main_hwnd,
                    std::ptr::null_mut(),
                    WIN7_OFFSCREEN_PARK_POS,
                    WIN7_OFFSCREEN_PARK_POS,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            return;
        }

        // Ensure WebView2 overlay HWND is opaque & clickable on Win7 (no WS_EX_LAYERED/TRANSPARENT)
        win_force_click_through(&window, false);
        let Some((ux, uy, _, _)) = win7_layered::get_rect() else {
            unsafe {
                DeleteObject(rgn as _);
            }
            return;
        };
        win7_layered::set_overlay_active(true);
        unsafe {
            if SetWindowRgn(main_hwnd, rgn, 1) == 0 {
                DeleteObject(rgn as _);
            }
            let flags = if focus_menu {
                SWP_NOSIZE
            } else {
                SWP_NOSIZE | SWP_NOACTIVATE
            };
            let _ = SetWindowPos(main_hwnd, HWND_TOPMOST, ux, uy, 0, 0, flags);
            if focus_menu {
                let _ = SetForegroundWindow(main_hwnd);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (window, rects, focus_menu);
    }
}

/// Opens a lock file without sharing so a second instance cannot start on the
/// same data directory. The handle is intentionally kept for the process lifetime.
#[cfg(windows)]
fn win_acquire_instance_lock(
    dir: &std::path::Path,
) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_ALWAYS,
    };

    let _ = std::fs::create_dir_all(dir);
    let lock_path = dir.join("kurocat.lock");
    let wide: Vec<u16> = lock_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        None
    } else {
        Some(handle)
    }
}

#[cfg(windows)]
fn win_show_already_running() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
    let text: Vec<u16> = "KuroCat 已经在运行了。\n\n如果确实要同时开两个（例如自测联机），请给第二个实例一个独立的数据目录：\n\nset KUROCAT_DATA_DIR=%APPDATA%\\kuro-cat-pet-2\nKuroCat.exe"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = "KuroCat".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[cfg(windows)]
fn win_configure_local_webview2_runtime() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let mut found_local_rt = false;
    if std::env::var_os("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER").is_none() {
        for candidate in ["WebView2Runtime", "WebView2"] {
            let rt = dir.join(candidate);
            if rt.join("msedgewebview2.exe").is_file() {
                std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", &rt);
                found_local_rt = true;
                break;
            }
        }
    }
    // Isolate Fixed-Version WebView2 (v109 for Win7) user data folder so it never collides
    // with a newer system WebView2 (Chromium 13x/14x) profile when tested or co-installed.
    if (found_local_rt || cfg!(feature = "win7"))
        && std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none()
    {
        let udf = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .map(|base| base.join("KuroCat").join("EBWebView-Win7"))
            .unwrap_or_else(|| dir.join("WebView2Data"));
        let _ = std::fs::create_dir_all(&udf);
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &udf);
    }
    if win_needs_win7_ulw() {
        // Dark surface matching `#context-menu` / `#update-toast` so rounded overlays blend cleanly.
        if std::env::var_os("WEBVIEW2_DEFAULT_BACKGROUND_COLOR").is_none() {
            std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "FF18191E");
        }
        // Keep offscreen-parked WebView2 rendering smoothly at 30-60 FPS without Chromium occlusion throttling!
        let existing = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();
        let mut merged = existing.trim().to_string();
        for flag in [
            "--autoplay-policy=no-user-gesture-required",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
        ] {
            if !merged.contains(flag) {
                if !merged.is_empty() {
                    merged.push(' ');
                }
                merged.push_str(flag);
            }
        }
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", merged);
    }
}

#[cfg(target_os = "linux")]
fn linux_configure_display_env() {
    // Prevent WebKitGTK 4.1 DMABUF blank/black transparent window issues on KylinOS/Mesa/NVIDIA/VMs
    // while keeping RGBA compositing transparency intact.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    // Prefer X11/XWayland when DISPLAY is available so bottom-right window placement,
    // XShape click-through, and zero-root XRecord global input work reliably across desktop environments.
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false)
        && std::env::var_os("GDK_BACKEND").is_none()
        && std::env::var_os("KUROCAT_WAYLAND").is_none()
    {
        std::env::set_var("GDK_BACKEND", "x11");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_crash_logger();
    #[cfg(windows)]
    {
        win_wait_for_previous_pid_if_requested();
        // One instance per data directory: two pets would fight over settings.json.
        // Set KUROCAT_DATA_DIR to run a second profile side by side.
        if let Some(handle) = win_acquire_instance_lock(&config::app_data_dir()) {
            std::mem::forget(handle);
        } else {
            kuro_log("another instance is already running; exiting");
            win_show_already_running();
            return;
        }
        win_configure_local_webview2_runtime();
    }
    #[cfg(target_os = "linux")]
    {
        linux_configure_display_env();
    }

    let mut initial_settings = load_settings();
    // Respect persisted `clickThroughLocked` across restarts (Issue #41).
    // Emergency CLI escape hatch: launching with `--unlock` clears click-through lock immediately.
    if std::env::args().any(|a| a == "--unlock") && initial_settings.click_through_locked {
        initial_settings.click_through_locked = false;
        save_settings(&initial_settings);
    }
    let settings_arc = Arc::new(Mutex::new(initial_settings.clone()));
    let menu_open = Arc::new(AtomicBool::new(false));
    let menu_creating = Arc::new(AtomicBool::new(false));
    let menu_opened_at = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
    let dragging = Arc::new(AtomicBool::new(false));
    let mcp_enabled = Arc::new(AtomicBool::new(initial_settings.mcp_enabled));
    let update_banner_open = Arc::new(AtomicBool::new(false));
    let paused_guard = Arc::new(AtomicBool::new(false));

    let settings_for_setup = settings_arc.clone();
    let menu_for_setup = menu_open.clone();
    let drag_for_setup = dragging.clone();
    let mcp_for_setup = mcp_enabled.clone();
    let banner_for_setup = update_banner_open.clone();
    let paused_for_setup = paused_guard.clone();

    tauri::Builder::default()
        .manage(AppState {
            settings: settings_arc,
            menu_open,
            tail_picker_bounds: Arc::new(Mutex::new(None)),
            menu_creating,
            menu_close_gen: Arc::new(AtomicU64::new(0)),
            menu_opened_at,
            dragging,
            mcp_enabled,
            update_banner_open,
            paused_guard,
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            let s = settings_for_setup.lock().unwrap().clone();

            #[cfg(target_os = "macos")]
            {
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            if let Some(win) = app.get_webview_window("main") {
                let (win_w, win_h) = window_dimensions(&s.keyboard_layout, s.scale);
                let _ = win.set_size(Size::Logical(LogicalSize::new(win_w, win_h)));
                #[cfg(not(target_os = "macos"))]
                let (target_x, target_y, sz) = {
                    let sz = win.outer_size().unwrap_or(tauri::PhysicalSize::new(420, 440));
                    let sf = win.scale_factor().unwrap_or(1.0);
                    let (x, y) = if let (Some(x), Some(y)) = (s.x, s.y) {
                        // Monitor unplugged / DPI changed since the position was saved?
                        // Never start completely off-screen (the pet would "disappear"
                        // while the process keeps running).
                        if position_hits_any_monitor(
                            app.handle(),
                            x,
                            y,
                            sz.width as i32,
                            sz.height as i32,
                        ) {
                            (x, y)
                        } else {
                            kuro_log(&format!("saved position {x},{y} is off-screen; resetting"));
                            if let Ok(mut saved) = settings_for_setup.lock() {
                                saved.x = None;
                                saved.y = None;
                            }
                            default_bottom_right_position(sz.width as i32, sz.height as i32, sf)
                        }
                    } else {
                        default_bottom_right_position(sz.width as i32, sz.height as i32, sf)
                    };
                    (x, y, sz)
                };
                #[cfg(all(not(windows), not(target_os = "macos")))]
                let _ = sz;

                #[cfg(windows)]
                let (target_x, target_y, sz, anchored) = {
                    match windows_display::initial_placement(&win, &s) {
                        Some(target) => (target.position.x, target.position.y, target.size, true),
                        None => (target_x, target_y, sz, false),
                    }
                };

                #[cfg(windows)]
                if win_needs_win7_ulw() {
                    // On Windows 7/8: spawn our dedicated Win32 UpdateLayeredWindow (ULW_ALPHA)
                    // 256-level anti-aliased presenter window at (target_x, target_y), and park the
                    // WebView2 offscreen renderer at (-16000, -16000) so there is ZERO white box!
                    win_force_click_through(&win, false);
                    let _ = win.set_always_on_top(s.always_on_top);
                    let _ = win.set_position(PhysicalPosition::new(
                        WIN7_OFFSCREEN_PARK_POS,
                        WIN7_OFFSCREEN_PARK_POS,
                    ));
                    if anchored {
                        let _ = win.set_size(sz);
                    }
                    win7_layered::init(
                        handle.clone(),
                        target_x,
                        target_y,
                        sz.width,
                        sz.height,
                        s.always_on_top,
                    );
                    win7_layered::set_click_through(s.click_through_locked);
                } else {
                    let _ = win.set_always_on_top(s.always_on_top);
                    let _ = win.set_position(PhysicalPosition::new(target_x, target_y));
                    if anchored {
                        let _ = win.set_size(sz);
                    }
                }

                #[cfg(all(not(windows), not(target_os = "macos")))]
                {
                    let _ = win.set_always_on_top(s.always_on_top);
                    #[cfg(target_os = "linux")]
                    {
                        let _ = win.set_visible_on_all_workspaces(true);
                        let _ = win.set_skip_taskbar(true);
                    }
                    let _ = win.set_position(PhysicalPosition::new(target_x, target_y));
                }

                #[cfg(target_os = "macos")]
                {
                    macos_window::configure_fullscreen(&win, s.always_on_top);
                    match (s.display_id.as_deref(), s.x, s.y) {
                        (None, Some(x), Some(y)) => {
                            let _ = win.set_position(PhysicalPosition::new(x, y));
                        }
                        (preferred, _, _) => {
                            macos_window::place(&win, preferred);
                        }
                    }
                }
            }

            // A missing shell (kiosk / minimal RDP session) must not kill the pet:
            // the tray is a convenience, the cat window and the menu still work.
            if let Err(err) = tray::setup_tray(&handle, &s) {
                eprintln!("[kurocat] system tray unavailable: {err}");
            }
            input_hook::start_global_hooks(handle.clone());

            // ---- multiplayer ---------------------------------------------------
            // The friend window is created 1.5 s after startup: creating a second
            // WebView2 on top of the first one's initialisation can abort the app.
            ensure_peer_window_async(&handle);

            let settings_net_handle = handle.clone();
            handle.listen("kuro-settings", move |_| {
                broadcast_net_state(&settings_net_handle);
            });
            let cats_net_handle = handle.clone();
            handle.listen("kuro-cats", move |_| {
                broadcast_net_state(&cats_net_handle);
            });
            let peer_state_handle = handle.clone();
            handle.listen(net::EVENT_STATE, move |ev| {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(ev.payload()) else {
                    return;
                };
                let layout = value
                    .get("keyboardLayout")
                    .and_then(|v| v.as_str())
                    .unwrap_or("65")
                    .to_string();
                let scale = value.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
                apply_peer_window_geometry(&peer_state_handle, &layout, scale);
            });
            let presence_handle = handle.clone();
            handle.listen(net::EVENT_PRESENCE, move |ev| {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(ev.payload()) else {
                    return;
                };
                let count = value.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                // `count` includes ourselves, so a friend is present only at >= 2.
                let friend_present = count > 1;
                set_peer_window_visible(&presence_handle, friend_present);
                if friend_present {
                    // Re-announce our look when someone walks in: the state sent at
                    // join time reached an empty room.
                    let state_handle = presence_handle.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(60));
                        broadcast_net_state(&state_handle);
                    });
                }
            });
            let status_handle = handle.clone();
            handle.listen(net::EVENT_STATUS, move |ev| {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(ev.payload()) else {
                    return;
                };
                let state = value.get("state").and_then(|v| v.as_str()).unwrap_or("");
                if state == "idle" || state == "error" {
                    set_peer_window_visible(&status_handle, false);
                }
            });
            privacy::start_password_privacy_watcher(
                handle.clone(),
                settings_for_setup.clone(),
                paused_for_setup.clone(),
            );
            game_detect::start_game_detection_watcher(
                handle.clone(),
                settings_for_setup.clone(),
                paused_for_setup.clone(),
            );
            start_hit_test_loop(
                handle.clone(),
                settings_for_setup,
                menu_for_setup,
                drag_for_setup,
                banner_for_setup,
                paused_for_setup,
            );
            start_storage_watcher(handle.clone());
            updater::start_background_update_checker(handle.clone());

            let http_handle = handle.clone();
            let mcp_port = if s.mcp_port == 0 { 27420 } else { s.mcp_port };
            mcp::start_http_mcp_server(mcp_port, mcp_for_setup, move || {
                reload_and_emit_all(&http_handle);
            });

            // Forward settings / cats / privacy broadcasts to OBS SSE clients (1ns no-op when OBS is off)
            handle.listen("kuro-settings", |ev| {
                obs::broadcast_raw("settings", ev.payload());
            });
            handle.listen("kuro-cats", |ev| {
                obs::broadcast_raw("cats", ev.payload());
            });
            handle.listen("kuro-privacy", |ev| {
                let active = ev.payload().trim() == "true";
                obs::set_privacy_active(active);
            });
            handle.listen("kuro-game-active", |ev| {
                let raw = ev.payload();
                let active = serde_json::from_str::<serde_json::Value>(raw)
                    .ok()
                    .and_then(|v| v.get("active").and_then(|a| a.as_bool()))
                    .unwrap_or(false);
                obs::set_game_active(active, raw);
            });
            obs::sync_obs_server(&handle, s.obs_enabled, s.obs_port);
            if s.obs_enabled && s.obs_hide_pet {
                set_pet_visible(&handle, false);
            }

            if std::env::args().any(|a| a == "--demo") {
                let demo_handle = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1200));
                    let keys: &[u32] = &[0x46, 0x53, 0x41, 0x45, 0x20, 0xA0, 0x4B];
                    for &vk in keys {
                        let _ = demo_handle.emit(
                            "kuro-input",
                            serde_json::json!({ "kind": "keyDown", "vk": vk, "scan": 0, "repeat": false }),
                        );
                        std::thread::sleep(Duration::from_millis(100));
                        let _ = demo_handle.emit(
                            "kuro-input",
                            serde_json::json!({ "kind": "keyUp", "vk": vk, "scan": 0 }),
                        );
                    }
                    let _ = demo_handle.emit(
                        "kuro-input",
                        serde_json::json!({ "kind": "mouseMove", "x": 400, "y": 400, "nx": 0.25, "ny": 0.4 }),
                    );
                    let _ = demo_handle.emit(
                        "kuro-input",
                        serde_json::json!({ "kind": "mouseDown", "button": "left" }),
                    );
                    let _ = demo_handle.emit(
                        "kuro-input",
                        serde_json::json!({ "kind": "keyDown", "vk": 0x4B, "scan": 0, "repeat": false }),
                    );
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            get_resolved_locale,
            trim_idle_memory,
            get_cats,
            get_game_active,
            get_mcp_config,
            get_obs_url,
            get_app_version,
            update_settings,
            open_context_menu,
            close_context_menu,
            open_tail_picker_from_menu,
            preview_scale,
            set_menu_open,
            set_tail_picker_bounds,
            save_tail_picker_selection,
            set_dragging,
            start_pet_drag,
            set_update_banner_open,
            is_win7_ulw_mode,
            win7_present_frame,
            win7_sync_overlay,
            net_host,
            net_connect,
            net_disconnect,
            net_status,
            save_peer_window_pos,
            reset_peer_window_pos,
            save_window_pos,
            reset_position,
            create_desktop_shortcut,
            get_cached_update_info,
            check_for_update,
            install_update,
            open_release_page,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running KuroCat");
}

#[cfg(all(test, windows))]
mod windows_zorder_tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    #[test]
    fn detects_and_recovers_topmost_zorder_without_stealing_focus() {
        let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "KuroCatZOrderTest\0".encode_utf16().collect();
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            let model = CreateWindowExW(
                ex,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                100,
                100,
                200,
                200,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            let fg = CreateWindowExW(
                ex,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                120,
                120,
                200,
                200,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            assert!(!model.is_null() && !fg.is_null());
            assert!(win_is_topmost(model));
            assert!(win_is_topmost(fg));

            // Place simulated foreground topmost window above model
            SetWindowPos(
                fg,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            assert!(
                win_window_is_above(fg, model),
                "expected fg topmost window to be detected above model"
            );

            // Reassert model to HWND_TOPMOST (same call as win_gentle_reassert_topmost)
            SetWindowPos(
                model,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            assert!(
                !win_window_is_above(fg, model),
                "expected model to sit above fg after HWND_TOPMOST reassert"
            );

            // Move fg away so rectangles no longer overlap (e.g. another monitor)
            SetWindowPos(fg, HWND_TOPMOST, 2000, 2000, 200, 200, SWP_NOACTIVATE);
            assert!(
                !win_window_is_above(fg, model),
                "non-overlapping fg window should never trigger topmost reorder"
            );

            // Verify SetWindowPos allows borderless pet window top headroom above y=0 (no Aero top-edge lock!)
            SetWindowPos(
                model,
                std::ptr::null_mut(),
                300,
                -70,
                0,
                0,
                SWP_NOSIZE
                    | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                    | SWP_NOACTIVATE,
            );
            let mut rc = windows_sys::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            assert_ne!(
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(model, &mut rc),
                0
            );
            assert_eq!(rc.top, -70, "SetWindowPos should place pet window cleanly at y=-70 without top-edge clamp");

            let _ = DestroyWindow(fg);
            let _ = DestroyWindow(model);
        }

        // Verify elevation query runs safely without panicking
        let _ = win_is_elevated();
    }

    #[test]
    fn win7_rounded_overlay_region_clips_menu_and_toast_cleanly() {
        use windows_sys::Win32::Graphics::Gdi::{DeleteObject, PtInRegion};
        // Context menu card (radius 12) + update toast pill (radius 20)
        let rects = vec![
            10, 10, 224, 310, 12,
            60, 8, 340, 42, 20,
        ];
        unsafe {
            let rgn = build_rounded_overlay_hrgn(&rects);
            assert!(!rgn.is_null(), "CreateRoundRectRgn + CombineRgn should produce valid HRGN");
            assert_ne!(PtInRegion(rgn, 100, 150), 0, "inside context menu");
            assert_ne!(PtInRegion(rgn, 200, 24), 0, "inside update toast");
            assert_eq!(PtInRegion(rgn, 10, 10), 0, "sharp 12px corner tip should be rounded off");
        assert_eq!(PtInRegion(rgn, 350, 200), 0, "empty area outside overlays must be clipped away");
        DeleteObject(rgn as _);
    }
}
}

#[cfg(test)]
mod side_menu_layout_tests {
    use super::compute_side_menu_pos;

    #[test]
    fn places_menu_to_left_of_bottom_right_cat_with_zero_overlap() {
        // 1920x1040 monitor, cat at bottom-right (1580, 620, 315, 410)
        let pet = (1580, 620, 315, 410);
        let work = (0, 0, 1920, 1040);
        let menu = (266, 620);
        let (mx, my) = compute_side_menu_pos(pet, work, menu, 10, 10);
        assert_eq!(mx, 1580 - 266 - 10, "should open cleanly to the left of the cat");
        assert!(mx + menu.0 <= pet.0, "menu must NOT overlap the cat horizontally");
        assert_eq!(my, 620 + 410 - 620, "should align cleanly to cat bottom edge");
    }

    #[test]
    fn flips_menu_to_right_when_cat_sits_near_left_edge() {
        let pet = (16, 620, 315, 410);
        let work = (0, 0, 1920, 1040);
        let menu = (266, 620);
        let (mx, _my) = compute_side_menu_pos(pet, work, menu, 10, 10);
        assert_eq!(mx, 16 + 315 + 10, "should flip cleanly to the right of the cat");
        assert!(mx >= pet.0 + pet.2, "menu must NOT overlap the cat horizontally");
    }

    #[test]
    fn places_menu_above_cat_when_horizontal_space_is_tight() {
        let pet = (100, 560, 300, 300);
        let work = (0, 0, 520, 900);
        let menu = (266, 400);
        let (_mx, my) = compute_side_menu_pos(pet, work, menu, 10, 10);
        assert!(my + menu.1 <= pet.1, "should open cleanly above the cat");
    }

    #[test]
    fn drag_clamp_allows_cat_ears_to_reach_top_of_screen_without_losing_window() {
        // 1920x1080 virtual desktop, 420x440 window: transparent top headroom (~75px) may go above y=0
        let (x, y) = super::clamp_pet_drag_position(500, -140, 420, 440, 0, 0, 1920, 1080);
        assert_eq!(x, 500);
        assert_eq!(y, -75, "cat window should smoothly slide up by ~17% headroom so ears touch y=0");

        // Trying to drag way off-screen left/right/bottom keeps pet safely reachable
        let (lx, by) = super::clamp_pet_drag_position(-500, 2000, 420, 440, 0, 0, 1920, 1080);
        assert_eq!(lx, -42);
        assert!(by <= 1080 - 290 && by >= 700, "pet body stays safely visible at bottom edge");
    }

    #[test]
    fn banner_hit_limit_covers_toast_at_mini_scales() {
        // 100% scale (~440px tall): preserves classic 19% top band (~83.6px)
        assert!((super::banner_hit_ry_limit(440.0) - 0.19).abs() < 1e-6);
        // 36% scale (~160px tall): 54/160 = 0.3375 so 54px toast stays 100% clickable
        let lim36 = super::banner_hit_ry_limit(160.0);
        assert!(lim36 * 160.0 >= 53.9 && lim36 <= 0.55);
        // 20% mini scale (~95px tall): clamped at 55% so toast never steals the whole cat
        let lim20 = super::banner_hit_ry_limit(95.0);
        assert!((lim20 - 0.55).abs() < 1e-6);
    }

    #[test]
    fn click_through_lock_persists_in_settings_json() {
        let mut s = crate::config::AppSettings::default();
        s.click_through_locked = true;
        let json = serde_json::to_string(&s).expect("serialize settings");
        let parsed: crate::config::AppSettings = serde_json::from_str(&json).expect("deserialize settings");
        assert!(parsed.click_through_locked, "clickThroughLocked must round-trip as true");
    }
}
