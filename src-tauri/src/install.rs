//! Первый запуск у нового человека: есть ли WebView2 и не запущены ли мы из «Загрузок».
//!
//! WebView2 (движок Edge) рисует все окна приложения: привязку, плашки, сохранения. В Windows 11 он
//! есть всегда, в Windows 10 приходит обновлениями, но в LTSC, корпоративных сборках и у тех, кто
//! вычищал Edge, его может не быть — тогда значок в трее есть, а окна не открываются молча. Проверяем
//! заранее и предлагаем поставить.
//!
//! Скачанный exe человек запускает прямо из «Загрузок» (или с рабочего стола), и автозапуск
//! прописался бы туда же — почистил загрузки, приложение пропало. Поэтому оттуда приложение
//! переносит себя в %LOCALAPPDATA%\BigBacklog Companion и запускается уже там. Если там уже стоит
//! копия, новую не кладем (она обновляется сама и может быть новее скачанной) — просто запускаем ее.

use std::path::{Path, PathBuf};

use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY};
use winreg::RegKey;

const WEBVIEW2_CLIENT: &str = r"Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
/// Установщик WebView2 Evergreen с сайта Microsoft.
const WEBVIEW2_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";
const INSTALL_DIR: &str = "BigBacklog Companion";
const EXE_NAME: &str = "bigbacklog-agent.exe";

/// Стоит ли WebView2 Runtime: версия в ключе EdgeUpdate (так проверяет и сам установщик Microsoft).
pub fn webview2_ok() -> bool {
    let good = |key: Option<RegKey>| {
        key.and_then(|k| k.get_value::<String, _>("pv").ok())
            .map(|v| !v.trim().is_empty() && v.trim() != "0.0.0.0")
            .unwrap_or(false)
    };
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    good(hklm.open_subkey_with_flags(format!(r"SOFTWARE\{WEBVIEW2_CLIENT}"), KEY_READ | KEY_WOW64_32KEY).ok())
        || good(hklm.open_subkey_with_flags(format!(r"SOFTWARE\{WEBVIEW2_CLIENT}"), KEY_READ).ok())
        || good(hkcu.open_subkey_with_flags(format!(r"Software\{WEBVIEW2_CLIENT}"), KEY_READ).ok())
}

/// Нет WebView2: обычное окно Windows с предложением скачать, «Да» открывает страницу Microsoft.
pub fn webview2_prompt() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONWARNING, MB_YESNO};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let text = wide(
        "Для работы BigBacklog Companion нужен компонент Microsoft WebView2 (движок браузера Edge). \
         На этом компьютере его нет.\n\nСкачать его с сайта Microsoft? После установки запустите \
         BigBacklog Companion еще раз.",
    );
    let title = wide("BigBacklog Companion");
    let answer = unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING) };
    if answer == IDYES {
        crate::win::open_url(WEBVIEW2_URL);
    }
}

fn user_shell_folder(name: &str) -> Option<PathBuf> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders")
        .ok()?;
    let raw: String = key.get_value(name).ok()?;
    // Значение с %USERPROFILE% и т. п.: раскрываем руками.
    let mut out = raw.clone();
    for (k, v) in std::env::vars() {
        out = out.replace(&format!("%{k}%"), &v).replace(&format!("%{}%", k.to_uppercase()), &v);
    }
    Some(PathBuf::from(out))
}

/// Папки, откуда жить приложению нельзя: «Загрузки», рабочий стол, временные.
fn unsafe_places() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = user_shell_folder("{374DE290-123F-4565-9164-39C4925E467B}") {
        out.push(d);
    }
    if let Some(d) = user_shell_folder("Desktop") {
        out.push(d);
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        out.push(Path::new(&home).join("Downloads"));
        out.push(Path::new(&home).join("Desktop"));
    }
    for var in ["TEMP", "TMP"] {
        if let Ok(t) = std::env::var(var) {
            out.push(PathBuf::from(t));
        }
    }
    out
}

fn lower(p: &Path) -> String {
    p.display().to_string().to_lowercase().replace('/', "\\").trim_end_matches('\\').to_string()
}

fn under(path: &Path, dir: &Path) -> bool {
    let (p, d) = (lower(path), lower(dir));
    !d.is_empty() && (p == d || p.starts_with(&format!("{d}\\")))
}

pub fn install_dir() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA").ok().map(|d| Path::new(&d).join(INSTALL_DIR))
}

/// Запущены из «Загрузок» и т. п. — переезжаем в постоянную папку и запускаемся оттуда. true — этот
/// процесс должен выйти (запущена копия в постоянной папке).
pub fn relocate() -> bool {
    if crate::DEV {
        return false; // dev-сборку владелец ставит руками
    }
    let Ok(me) = std::env::current_exe() else { return false };
    let Some(dir) = me.parent().map(Path::to_path_buf) else { return false };
    // Портативный режим (config.json рядом с exe) — человек сам выбрал место.
    if dir.join("config.json").is_file() {
        return false;
    }
    let Some(home) = install_dir() else { return false };
    if under(&dir, &home) || !unsafe_places().iter().any(|u| under(&dir, u)) {
        return false;
    }
    let target = home.join(EXE_NAME);
    if !target.is_file() {
        if std::fs::create_dir_all(&home).is_err() || std::fs::copy(&me, &target).is_err() {
            crate::log("установка: не скопировать себя в постоянную папку, работаю отсюда");
            return false;
        }
        crate::log(&format!("установка: перенес себя в {}", home.display()));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(&target).args(&args).spawn() {
        Ok(_) => true,
        Err(e) => {
            crate::log(&format!("установка: не запустить {}: {e}", target.display()));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::under;
    use std::path::Path;

    #[test]
    fn under_dir() {
        assert!(under(Path::new(r"C:\Users\x\Downloads"), Path::new(r"C:\Users\X\Downloads")));
        assert!(under(Path::new(r"C:\Users\x\Downloads\sub"), Path::new(r"C:\Users\x\Downloads\")));
        assert!(!under(Path::new(r"C:\Users\x\DownloadsOld"), Path::new(r"C:\Users\x\Downloads")));
        assert!(!under(Path::new(r"C:\Games"), Path::new("")));
    }
}
