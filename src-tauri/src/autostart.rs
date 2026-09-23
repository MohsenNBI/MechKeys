use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "MechKeys";

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

pub fn set(enabled: bool) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .map_err(|e| format!("cannot open registry run key: {e}"))?;
    if enabled {
        // Start hidden in the tray, otherwise a boot would pop the window open.
        key.set_value(VALUE_NAME, &format!("\"{}\" --minimized", exe_path()))
            .map_err(|e| format!("cannot write registry value: {e}"))
    } else {
        match key.delete_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove registry value: {e}")),
        }
    }
}

pub fn is_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ) else {
        return false;
    };
    key.get_value::<String, _>(VALUE_NAME).is_ok()
}
