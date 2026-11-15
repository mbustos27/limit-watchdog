//! The one place that shows a Windows toast or touches the registry key that lets lw's toasts appear.
//! Uses WinRT's ToastNotificationManager directly (no PowerShell).
//!
//! Windows shows toasts only for a registered app id. `lw toast-setup` writes the per-user key
//! `HKCU\Software\Classes\AppUserModelId\LimitWatchdog.lw` (no admin); `lw toast-setup --remove` deletes it.
//! Nothing else writes the key.
//!
//! Test hook: with `LW_TOAST_FILE` set, nothing touches Windows: a toast is appended to that file as one line of JSON.
//! `LW_TOAST_FAIL` makes a delivery fail.
use std::io::Write;
use std::path::PathBuf;

pub const APP_ID: &str = "LimitWatchdog.lw";
pub const DISPLAY_NAME: &str = "Limit watchdog";
pub const HOOK_FILE: &str = "LW_TOAST_FILE";
pub const HOOK_FAIL: &str = "LW_TOAST_FAIL";

#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    Sent(String),
    NotSetUp,
    Failed(String),
}

fn hook_file() -> Option<PathBuf> {
    std::env::var_os(HOOK_FILE).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn key_path() -> String {
    format!(r"Software\Classes\AppUserModelId\{APP_ID}")
}

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\t' => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max - 1).collect::<String>() + "\u{2026}" }
}

pub fn xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape(&clip(title, 120)),
        escape(&clip(body, 400))
    )
}

pub fn deliver(title: &str, body: &str) -> Delivery {
    if let Some(f) = hook_file() {
        if std::env::var_os(HOOK_FAIL).is_some() {
            return Delivery::Failed("test hook: delivery made to fail".into());
        }
        let line = serde_json::json!({ "title": title, "body": body }).to_string();
        return match std::fs::OpenOptions::new().create(true).append(true).open(&f).and_then(|mut h| writeln!(h, "{line}")) {
            Ok(()) => Delivery::Sent("test hook".into()),
            Err(e) => Delivery::Failed(e.to_string()),
        };
    }
    match sys::registered() {
        Ok(true) => {}
        Ok(false) => return Delivery::NotSetUp,
        Err(e) => return Delivery::Failed(e),
    }
    sys::show(&xml(title, body))
}

pub fn registered() -> Result<bool, String> {
    if hook_file().is_some() {
        return Ok(true);
    }
    sys::registered()
}

pub fn register() -> Result<String, String> {
    if hook_file().is_some() {
        return Ok("test hook: nothing to register".into());
    }
    sys::register()
}

pub fn unregister() -> Result<String, String> {
    if hook_file().is_some() {
        return Ok("test hook: nothing to remove".into());
    }
    sys::unregister()
}

#[cfg(windows)]
mod sys {
    use std::sync::mpsc;
    use std::time::Duration;

    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegOpenKeyExW,
        RegQueryValueExW, RegSetValueExW,
    };
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::{HSTRING, PCWSTR};

    use super::{APP_ID, DISPLAY_NAME, Delivery, key_path};

    const LIMIT: Duration = Duration::from_secs(20);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn registered() -> Result<bool, String> {
        let path = wide(&key_path());
        let mut key = HKEY::default();
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut key) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("the registry key could not be opened: error {}", rc.0));
        }
        let name = wide("DisplayName");
        let mut size: u32 = 0;
        let rc = unsafe { RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, None, None, Some(&mut size)) };
        let _ = unsafe { RegCloseKey(key) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("the registry value could not be read: error {}", rc.0));
        }
        Ok(size > 2)
    }

    pub fn register() -> Result<String, String> {
        let path = wide(&key_path());
        let mut key = HKEY::default();
        let rc = unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None)
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("the registry key could not be created: error {}", rc.0));
        }
        let name = wide("DisplayName");
        let bytes: Vec<u8> = wide(DISPLAY_NAME).iter().flat_map(|c| c.to_le_bytes()).collect();
        let rc = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&bytes)) };
        let _ = unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS {
            return Err(format!("the DisplayName value could not be written: error {}", rc.0));
        }
        Ok(format!("wrote HKCU\\{} DisplayName = \"{DISPLAY_NAME}\"", key_path()))
    }

    pub fn unregister() -> Result<String, String> {
        let path = wide(&key_path());
        let rc = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok("it was not registered".into());
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("the registry key could not be removed: error {}", rc.0));
        }
        Ok(format!(r"removed HKCU\{}", key_path()))
    }

    fn show_now(xml: &str) -> Result<String, String> {
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID)).map_err(|e| format!("Windows would not give a notifier: {e}"))?;
        // A new app id may not be listed in Settings yet (the setting cannot be read): that is not a refusal.
        if let Ok(s) = notifier.Setting()
            && s != NotificationSetting::Enabled
        {
            return Err("Windows has notifications switched off for this app (Settings > System > Notifications)".into());
        }
        let doc = XmlDocument::new().map_err(|e| e.to_string())?;
        doc.LoadXml(&HSTRING::from(xml)).map_err(|e| format!("the toast text was refused: {e}"))?;
        let note = ToastNotification::CreateToastNotification(&doc).map_err(|e| e.to_string())?;
        // A tag per toast, so one toast does not replace the previous one in the Action Center.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() % 1_000_000_000_000).unwrap_or(0);
        note.SetTag(&HSTRING::from(format!("lw-{nanos}"))).map_err(|e| e.to_string())?;
        note.SetGroup(&HSTRING::from("lw")).map_err(|e| e.to_string())?;
        notifier.Show(&note).map_err(|e| format!("Windows refused the toast: {e}"))?;
        Ok("handed to Windows".into())
    }

    pub fn show(xml: &str) -> Delivery {
        let (tx, rx) = mpsc::channel();
        let owned = xml.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(show_now(&owned));
        });
        match rx.recv_timeout(LIMIT) {
            Ok(Ok(how)) => Delivery::Sent(how),
            Ok(Err(e)) => Delivery::Failed(e),
            Err(_) => Delivery::Failed("Windows did not take the toast in time".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Windows' own parser must accept the XML, hostile text included. Parsing shows nothing.
        #[test]
        fn windows_accepts_the_toast_xml() {
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            let doc = XmlDocument::new().unwrap();
            doc.LoadXml(&HSTRING::from(super::super::xml("</text><b a='1'>&\"", "five < six & \u{7}\u{1f600}"))).unwrap();
            assert_eq!(doc.GetElementsByTagName(&HSTRING::from("text")).unwrap().Length().unwrap(), 2);
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Delivery;
    pub fn registered() -> Result<bool, String> {
        Ok(false)
    }
    pub fn register() -> Result<String, String> {
        Err("toasts are Windows only".into())
    }
    pub fn unregister() -> Result<String, String> {
        Ok("it was not registered".into())
    }
    pub fn show(_: &str) -> Delivery {
        Delivery::NotSetUp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_escaped_and_clipped() {
        let x = xml("a<b", &"z".repeat(1000));
        assert!(x.contains("a&lt;b"));
        assert!(x.chars().filter(|c| *c == 'z').count() < 400);
    }
}
