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
    quit: &'static str,
}

fn labels(lang: &str) -> Labels {
    if lang == "fa" {
        Labels {
            title: "MechKeys — صدای کیبورد مکانیکی",
            tooltip: "MechKeys — صدای کیبورد مکانیکی",
            show: "نمایش پنجره",
            toggle: "روشن / خاموش کردن صدا",
            quit: "خروج",
        }
    } else {
        Labels {
            title: "MechKeys — mechanical keyboard sound",
            tooltip: "MechKeys — mechanical keyboard sound",
            show: "Show window",
            toggle: "Sound on / off",
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
    let quit = MenuItem::with_id(app, "quit", t.quit, true, None::<&str>)?;
    Menu::with_items(app, &[&show, &toggle, &separator, &quit])
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

fn setup_tray(app: &AppHandle, lang: &str) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let menu = build_menu(app, lang)?;
    let t = labels(lang);

    // Copied into an owned image so the tray does not borrow the app handle.
    let icon = app
        .default_window_icon()
        .map(|i| tauri::image::Image::new_owned(i.rgba().to_vec(), i.width(), i.height()))
        .unwrap_or_else(fallback_icon);

    TrayIconBuilder::with_id(TRAY)
        .icon(icon)
        .tooltip(t.tooltip)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "toggle" => {
                let st = app.state::<AppState>();
                let enabled = !st.shared.enabled.load(Ordering::Relaxed);
                st.shared.enabled.store(enabled, Ordering::Relaxed);
                let mut cfg = st.cfg.lock().unwrap();
                cfg.enabled = enabled;
                config::save(&cfg);
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
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_title(t.title);
    }
    Ok(())
}

/// Used only if the bundled .ico could not be decoded: an amber-lit keycap on a slate plate.
fn fallback_icon() -> tauri::image::Image<'static> {
    const PLATE: [u8; 3] = [0x1b, 0x21, 0x29];
    const TOP: [u8; 3] = [0xd5, 0xdc, 0xe4];
    const SIDE: [u8; 3] = [0x6f, 0x7b, 0x88];
    const AMBER: [u8; 3] = [0xf2, 0xa3, 0x3c];

    let (w, h) = (32u32, 32u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dx = (fx - 16.0).abs();
            let top = dx / 9.0 + (fy - 13.0).abs() / 5.0 <= 1.0;
            let skirt = dx / 9.0 + (fy - 17.0).abs() / 5.0;
            let arc = {
                let r = ((fx - 25.0).powi(2) + (fy - 8.0).powi(2)).sqrt();
                (4.2..=5.4).contains(&r) && fy <= 12.0
            };
            let rgb = if top {
                Some(TOP)
            } else if arc {
                Some(AMBER)
            } else if skirt <= 1.0 {
                Some(if skirt > 0.88 && fx <= 16.0 { AMBER } else { SIDE })
            } else if (fx - 16.0).abs() < 15.5 && (fy - 16.0).abs() < 15.5 {
                Some(PLATE)
            } else {
                None
            };
            let i = ((y * w + x) * 4) as usize;
            if let Some(c) = rgb {
                rgba[i..i + 3].copy_from_slice(&c);
                rgba[i + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, w, h)
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
