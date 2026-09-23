#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod config;
mod hook;
mod lowlat;
mod sessions;
mod sound;
mod state;
mod update;

use state::{Shared, StatsDto, UiError, UiState};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State, WindowEvent};

pub struct AppState {
    pub shared: Arc<Shared>,
    pub cfg: Mutex<config::Config>,
    pub errors: Mutex<Vec<UiError>>,
    /// The last release GitHub described, kept here so the download step can
    /// only ever fetch what the check actually found.
    pub release: Mutex<Option<update::Release>>,
}

const TRAY: &str = "mechkeys";

impl AppState {
    fn lang(&self) -> &'static str {
        config::lang(&self.cfg.lock().unwrap().lang)
    }
}

#[tauri::command]
fn get_state(st: State<'_, AppState>) -> UiState {
    let cfg = st.cfg.lock().unwrap().clone();
    UiState {
        enabled: cfg.enabled,
        volume: cfg.volume,
        profile: sound::profile_name(state::profile_index(&cfg.profile)).to_string(),
        up_sound: cfg.up_sound,
        exclusive: cfg.exclusive,
        auto_release: cfg.auto_release,
        check_updates: cfg.check_updates,
        autostart: autostart::is_enabled(),
        portable: config::portable(),
        lang: config::lang(&cfg.lang).to_string(),
        errors: st.errors.lock().unwrap().clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[tauri::command]
fn set_enabled(st: State<'_, AppState>, enabled: bool) {
    st.shared.enabled.store(enabled, Ordering::Relaxed);
    let mut cfg = st.cfg.lock().unwrap();
    cfg.enabled = enabled;
    config::save(&cfg);
}

#[tauri::command]
fn set_volume(st: State<'_, AppState>, volume: f32) {
    let v = volume.clamp(0.0, 1.0);
    st.shared.volume.store(v.to_bits(), Ordering::Relaxed);
    let mut cfg = st.cfg.lock().unwrap();
    cfg.volume = v;
    config::save(&cfg);
}

#[tauri::command]
fn set_profile(st: State<'_, AppState>, profile: String) {
    let index = state::profile_index(&profile);
    st.shared.profile.store(index, Ordering::Relaxed);
    let mut cfg = st.cfg.lock().unwrap();
    cfg.profile = sound::profile_name(index).to_string();
    config::save(&cfg);
    // An instrument is rendered, not recorded, so the voices have to be built
    // before the keys change pitch. Off the audio thread, and off this one.
    st.shared.rebuild_bank_soon();
}

#[tauri::command]
fn set_up_sound(st: State<'_, AppState>, enabled: bool) {
    st.shared.up_sound.store(enabled, Ordering::Relaxed);
    let mut cfg = st.cfg.lock().unwrap();
    cfg.up_sound = enabled;
    config::save(&cfg);
}

#[tauri::command]
fn set_exclusive(st: State<'_, AppState>, enabled: bool) {
    // The audio thread drops the old stream before opening the new one, which
    // is what lets exclusive mode actually take the endpoint over.
    st.shared.want_exclusive.store(enabled, Ordering::Relaxed);
    st.shared.reopen.store(true, Ordering::Relaxed);
    let mut cfg = st.cfg.lock().unwrap();
    cfg.exclusive = enabled;
    config::save(&cfg);
}

#[tauri::command]
fn test_sound(st: State<'_, AppState>) {
    st.shared.push_test();
}

/// The preview of one particular cap, for a click on the board.
#[tauri::command]
fn test_key(st: State<'_, AppState>, vk: u32, scan: u32) {
    st.shared.push_key(vk, scan);
}

/// What went down since the window last looked. It asks often, and the board
/// replays the whole run, so a fast passage lights every cap it touched rather
/// than only the last one.
#[tauri::command]
fn recent_keys(st: State<'_, AppState>) -> Vec<u32> {
    st.shared.take_recent()
}

/// Let the monitor thread hand the endpoint over while nobody is typing.
#[tauri::command]
fn set_auto_release(st: State<'_, AppState>, enabled: bool) {
    st.shared.auto_release.store(enabled, Ordering::Relaxed);
    // Switching it off has to give the endpoint back at once, not on the next
    // keystroke: the monitor only notices on its own poll.
    if !enabled && st.shared.hold_shared.swap(false, Ordering::Relaxed) {
        st.shared.reopen.store(true, Ordering::Relaxed);
    }
    let mut cfg = st.cfg.lock().unwrap();
    cfg.auto_release = enabled;
    config::save(&cfg);
}

#[tauri::command]
fn set_check_updates(st: State<'_, AppState>, enabled: bool) {
    let mut cfg = st.cfg.lock().unwrap();
    cfg.check_updates = enabled;
    config::save(&cfg);
}

/// Asks GitHub for the newest release. A command, not a thread: the window
/// waits on it, and the answer is only ever as good as the last question.
#[tauri::command]
fn check_update(st: State<'_, AppState>) -> Result<update::Update, String> {
    let release = update::latest(st.lang())?;
    let have = env!("CARGO_PKG_VERSION");
    let available = update::is_newer(&release.version, have);
    *st.release.lock().unwrap() = available.then(|| release.clone());
    Ok(update::Update::from_release(&release, available))
}

/// Fetches the installer of the release that was just checked, and says where
/// it put it. Running it stays the user's click.
#[tauri::command]
fn download_update(st: State<'_, AppState>) -> Result<String, String> {
    let release = st
        .release
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "check-first".to_string())?;
    let path = update::download(&release, st.lang())?;
    Ok(path.display().to_string())
}

/// The releases page, for when the automatic path is not the one you want.
#[tauri::command]
fn releases_page() -> String {
    update::release_page()
}

#[tauri::command]
fn stats(st: State<'_, AppState>) -> StatsDto {
    st.shared.stats()
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<bool, String> {
    autostart::set(enabled)?;
    Ok(autostart::is_enabled())
}

/// Switches the language of everything this side of the webview: the tray menu,
/// its tooltip and the title bar. The page itself re-renders from its own
/// dictionary, so the two halves never need to be in step beyond this call.
#[tauri::command]
fn set_lang(app: AppHandle, st: State<'_, AppState>, lang: String) {
    let lang = config::lang(&lang);
    {
        let mut cfg = st.cfg.lock().unwrap();
        cfg.lang = lang.to_string();
        config::save(&cfg);
    }
    retranslate(&app, lang);
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

/// Hand a link to the default browser. https only, and no shell in between:
/// explorer takes the whole address as one argument, so nothing in it can be
/// re-parsed as a command.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") || url.len() > 200 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        // A code, not a sentence: the page is the one that knows its language.
        return Err("https-only".into());
    }
    std::process::Command::new("explorer")
        .arg(&url)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        selftest(args.iter().any(|a| a == "--exclusive"));
    }
    if args.iter().any(|a| a == "--probe") {
        lowlat::probe();
    }
    if args.iter().any(|a| a == "--sessions") {
        sessions::report();
        return;
    }
    let start_hidden = args.iter().any(|a| a == "--minimized");

    let cfg = config::load();
    let shared = Shared::new(&cfg);
    let app_state = AppState {
        shared: shared.clone(),
        cfg: Mutex::new(cfg),
        errors: Mutex::new(Vec::new()),
        release: Mutex::new(None),
    };

    tauri::Builder::default()
        .manage(app_state)
        .setup(move |app| {
            let os_shared = app.state::<AppState>().shared.clone();
            let lang = app.state::<AppState>().lang();
            let fail = |kind: &str, e: String| UiError { kind: kind.into(), message: e };
            let mut errors = Vec::new();
            if let Err(e) = audio::start(os_shared.clone()) {
                errors.push(fail("audio", e));
            }
            if let Err(e) = hook::start(os_shared.clone()) {
                errors.push(fail("hook", e));
            }
            // The exclusive mode referee: harmless if the toggle is off, and it
            // costs one poll every half second when it is not.
            if let Err(e) = sessions::start(os_shared) {
                errors.push(fail("sessions", e));
            }
            for e in &errors {
                eprintln!("MechKeys: {}: {}", e.kind, e.message);
            }
            *app.state::<AppState>().errors.lock().unwrap() = errors;

            setup_tray(app.handle(), lang)?;

            if let Some(window) = app.get_webview_window("main") {
                set_native_icons(&window);
            }

            if start_hidden {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_enabled,
            set_volume,
            set_profile,
            set_up_sound,
            set_exclusive,
            set_auto_release,
            set_check_updates,
            check_update,
            download_update,
            releases_page,
            test_sound,
            test_key,
            recent_keys,
            stats,
            set_autostart,
            set_lang,
            hide_window,
            open_url
        ])
        .run(tauri::generate_context!())
        .expect("MechKeys failed to start");
}

/// The words this side of the webview: the title bar and the tray. Everything
/// else is worded in the page, which re-renders on the same switch.
struct Labels {
    title: &'static str,
    tooltip: &'static str,
    show: &'static str,
    toggle: &'static str,
    update: &'static str,
    quit: &'static str,
}

fn labels(lang: &str) -> Labels {
    if lang == "fa" {
        Labels {
            title: "MechKeys — صدای کیبورد مکانیکی",
            tooltip: "MechKeys — صدای کیبورد مکانیکی",
            show: "نمایش پنجره",
            toggle: "روشن / خاموش کردن صدا",
            update: "بررسی به‌روزرسانی",
            quit: "خروج",
        }
    } else {
        Labels {
            title: "MechKeys — mechanical keyboard sound",
            tooltip: "MechKeys — mechanical keyboard sound",
            show: "Show window",
            toggle: "Sound on / off",
            update: "Check for updates",
            quit: "Quit",
        }
    }
}

fn build_menu(app: &AppHandle, lang: &str) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let t = labels(lang);
    let show = MenuItem::with_id(app, "show", t.show, true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", t.toggle, true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let update = MenuItem::with_id(app, "update", t.update, true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", t.quit, true, None::<&str>)?;
    Menu::with_items(app, &[&show, &toggle, &separator, &update, &separator2, &quit])
}

/// Puts the tray and the title bar into a language. Called once at startup and
/// again on every switch; the menu ids stay the same, so the handler below keeps
/// working across the swap.
fn retranslate(app: &AppHandle, lang: &str) {
    let t = labels(lang);
    match build_menu(app, lang) {
        Ok(menu) => {
            if let Some(tray) = app.tray_by_id(TRAY) {
                let _ = tray.set_menu(Some(menu));
                let _ = tray.set_tooltip(Some(t.tooltip));
            }
        }
        Err(e) => eprintln!("MechKeys: tray menu: {e}"),
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_title(t.title);
    }
}

/// The icon set carries a raw RGBA frame at each size the shell can ask for.
/// `include_bytes!` because `Image::new` borrows its pixels, so picking a frame
/// costs no copy.
fn icon_frames() -> &'static [(u32, &'static [u8])] {
    const F16: &[u8] = include_bytes!("../icons/tray-16.rgba");
    const F20: &[u8] = include_bytes!("../icons/tray-20.rgba");
    const F24: &[u8] = include_bytes!("../icons/tray-24.rgba");
    const F32: &[u8] = include_bytes!("../icons/tray-32.rgba");
    const F48: &[u8] = include_bytes!("../icons/tray-48.rgba");
    const F64: &[u8] = include_bytes!("../icons/tray-64.rgba");
    const FRAMES: &[(u32, &[u8])] = &[
        (16, F16),
        (20, F20),
        (24, F24),
        (32, F32),
        (48, F48),
        (64, F64),
    ];
    FRAMES
}

/// The first frame big enough for `want`, so a size the set does not carry still
/// gets a picture that only has to shrink a little.
fn frame_at(want: u32) -> (u32, &'static [u8]) {
    let frames = icon_frames();
    *frames
        .iter()
        .find(|(s, _)| *s >= want)
        .unwrap_or(frames.last().unwrap())
}

/// The tray is drawn at the small-icon size the shell uses — 16 px at 100 %
/// scale, more on a high-DPI screen. Feeding it the 256 px window icon let
/// Windows grind it down to a smudge, so this takes a frame cut for the size.
fn tray_icon(scale: f64) -> tauri::image::Image<'static> {
    let (size, rgba) = frame_at((16.0 * scale).round() as u32);
    tauri::image::Image::new(rgba, size, size)
}

/// The title bar and the taskbar ask the *window* for its icon, and Tauri hands
/// both of them the 256 px image it decoded out of `icon.ico` — so Windows
/// squashes 256 px into a 16 px slot, and that is the blur. The frames below are
/// already cut for those slots, so build a real HICON at the size the shell
/// reports and install it over the one Tauri set.
#[cfg(windows)]
fn set_native_icons(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIcon, GetSystemMetrics, SendMessageW, ICON_BIG, ICON_SMALL, SM_CXICON, SM_CXSMICON,
        WM_SETICON,
    };

    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    unsafe {
        for (kind, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let wanted = GetSystemMetrics(metric).max(1) as u32;
            let (size, rgba) = frame_at(wanted);
            // CreateIcon wants BGRA plus the inverted alpha as a mask.
            let mut bgra = Vec::with_capacity((size * size) as usize * 4);
            let mut mask = Vec::with_capacity((size * size) as usize);
            for px in rgba.chunks_exact(4) {
                bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                mask.push(px[3].wrapping_sub(255));
            }
            let Ok(hicon) = CreateIcon(
                None,
                size as i32,
                size as i32,
                1,
                32,
                mask.as_ptr(),
                bgra.as_ptr(),
            ) else {
                continue;
            };
            let _ = SendMessageW(
                HWND(hwnd.0),
                WM_SETICON,
                WPARAM(kind as usize),
                LPARAM(hicon.0 as isize),
            );
        }
    }
}

#[cfg(not(windows))]
fn set_native_icons(_window: &tauri::WebviewWindow) {}

/// Brings the window forward. The tray offers it three times over: the menu, a
/// left click, and any menu item that needs the page on screen to be answered.
fn reveal(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn setup_tray(app: &AppHandle, lang: &str) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let menu = build_menu(app, lang)?;
    let t = labels(lang);
    let scale = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .unwrap_or(1.0);

    TrayIconBuilder::with_id(TRAY)
        .icon(tray_icon(scale))
        .tooltip(t.tooltip)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => reveal(app),
            "toggle" => {
                let st = app.state::<AppState>();
                let enabled = !st.shared.enabled.load(Ordering::Relaxed);
                st.shared.enabled.store(enabled, Ordering::Relaxed);
                let mut cfg = st.cfg.lock().unwrap();
                cfg.enabled = enabled;
                config::save(&cfg);
            }
            // The words of an update check live in the page, so the menu item
            // only opens it and asks; the page does the rest.
            "update" => {
                reveal(app);
                use tauri::Emitter;
                let _ = app.emit("tray-update", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal(tray.app_handle());
            }
        })
        .build(app)?;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_title(t.title);
    }
    Ok(())
}

/// Headless check of the whole pipeline: hook -> lock free queue -> audio voices.
fn selftest(exclusive: bool) -> ! {
    use std::time::Duration;
    println!("== MechKeys selftest ==");

    let mut cfg = config::Config::default();
    cfg.exclusive = exclusive;
    let shared = Shared::new(&cfg);

    lowlat::TRACE_FILLS.store(true, Ordering::Relaxed);
    if let Err(e) = audio::start(shared.clone()) {
        eprintln!("audio: {e}");
        std::process::exit(1);
    }
    println!("audio stream: ok");

    if let Err(e) = hook::start(shared.clone()) {
        eprintln!("hook: {e}");
        std::process::exit(1);
    }
    println!("keyboard hook: ok");

    std::thread::sleep(Duration::from_millis(400));
    let before = shared.keys.load(Ordering::Relaxed);
    const PRESSES: u64 = 20;
    let mut samples = Vec::new();
    let mut injected = 0u32;
    for _ in 0..PRESSES {
        injected += send_key(false);
        std::thread::sleep(Duration::from_millis(35));
        injected += send_key(true);
        // Long enough for the audio callback to have picked the event up.
        std::thread::sleep(Duration::from_millis(70));
        let us = shared.latency_us.load(Ordering::Relaxed);
        if us > 0 {
            samples.push(us as f64 / 1000.0);
        }
    }
    std::thread::sleep(Duration::from_millis(400));

    let captured = shared.keys.load(Ordering::Relaxed) - before;
    let played = shared.played.load(Ordering::Relaxed);
    let frames = shared.cb_frames.load(Ordering::Relaxed);
    let sr = shared.sample_rate.load(Ordering::Relaxed);
    if sr > 0 && frames > 0 {
        println!(
            "output buffer : {frames} frames @ {sr} Hz = {:.2} ms",
            frames as f32 * 1000.0 / sr as f32
        );
    }
    println!("voices started: {played}");
    if !samples.is_empty() {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let min = samples[0];
        let max = samples[samples.len() - 1];
        let avg = samples.iter().sum::<f64>() / samples.len() as f64;
        let median = samples[samples.len() / 2];
        println!(
            "hook->voice   : min {min:.2} ms / median {median:.2} ms / avg {avg:.2} ms / max {max:.2} ms  ({} samples)",
            samples.len()
        );
    }
    println!("keys captured : {captured}/{PRESSES}");

    // Holding a key: Windows keeps sending down events at the repeat rate once
    // the repeat delay has passed. Only the first press may sound.
    const REPEATS: u64 = 12;
    let total_inputs = PRESSES * 2 + REPEATS + 2;
    let held_before = shared.played.load(Ordering::Relaxed);
    injected += send_key(false);
    for _ in 0..REPEATS {
        std::thread::sleep(Duration::from_millis(35));
        injected += send_key(false);
    }
    std::thread::sleep(Duration::from_millis(140));
    let held_presses = shared.played.load(Ordering::Relaxed) - held_before;
    injected += send_key(true);
    std::thread::sleep(Duration::from_millis(140));
    let held_total = shared.played.load(Ordering::Relaxed) - held_before;
    println!(
        "held key      : {held_presses} voice(s) for {} downs, {held_total} after release (1 then 2 expected)",
        REPEATS + 1
    );
    println!("events injected: {injected}/{total_inputs}");

    // The up events double the voice count, so expect two per press.
    let expected = PRESSES * 2;
    if captured >= PRESSES && played >= expected && held_presses == 1 && held_total == 2 {
        println!("RESULT: OK");
        std::process::exit(0);
    }
    println!("RESULT: FAILED");
    std::process::exit(1);
}

/// Injects a harmless F13 key press (no application reacts to it).
fn send_key(up: bool) -> u32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };
    let flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0x7C),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) }
}
