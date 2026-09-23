use crate::state::Shared;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Installs a system wide low level keyboard hook on its own thread.
/// The thread only ever runs a message loop, so the hook stays cheap and stable.
pub fn start(shared: Arc<Shared>) -> Result<(), String> {
    let _ = SHARED.set(shared);
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("mechkeys-hook".into())
        .spawn(move || unsafe {
            // A null module handle is valid here: the hook proc lives in this process.
            let module = GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default();
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
            match hook {
                Ok(_hook) => {
                    let _ = tx.send(Ok(()));
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("keyboard hook was rejected by Windows: {e}")));
                }
            }
        })
        .map_err(|e| e.to_string())?;

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(_) => Err("timed out while installing the keyboard hook".into()),
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        if let Some(shared) = SHARED.get() {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let message = wparam.0 as u32;
            let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
            let up = message == WM_KEYUP || message == WM_SYSKEYUP;
            if down || up {
                // The E0 prefix is folded into the scan code as `sound::EXT`,
                // which is how arrows are told apart from the keypad.
                let scan = if kb.flags.0 & LLKHF_EXTENDED.0 != 0 {
                    kb.scanCode | crate::sound::EXT
                } else {
                    kb.scanCode
                };
                shared.on_key(kb.vkCode, scan, down);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}
