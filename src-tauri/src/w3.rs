//! The Witcher 3: сохранение на сайт.
//!
//! Игра пишет сохранения в «Документы\The Witcher 3\gamesaves\*.sav» (рядом
//! .json с версией и .png-превью — не нужны), счётчик смертей прохождения —
//! в «Документы\The Witcher 3\customUserData.json». После выхода из игры (и при
//! старте агента) отправляем самое свежее .sav, если оно не то, что ушло в
//! прошлый раз (POST /api/agent/w3-save?name=…, тело — файл, 0,2–3 МБ), и
//! следом customUserData.json, если он поменялся. Пока witcher3.exe жив, файлы
//! не трогаем: игра их пишет.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// The Witcher 3: Wild Hunt в Steam — id игры в каталоге агента.
pub const STEAM_APPID: &str = "292030";
/// GOTY-издание — отдельный appid в Steam.
pub const STEAM_APPID_GOTY: &str = "499450";
/// Процесс игры: пока он жив, сохранения не читаем.
pub const EXE: &str = "witcher3.exe";
pub const USER_DATA: &str = "customUserData.json";

/// Что ушло на сайт в прошлый раз (w3.json рядом с config.json).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Sent {
    pub name: String,
    /// Время изменения файла, мс.
    pub mtime: u64,
    pub size: u64,
    /// Время изменения customUserData.json, мс (0 — не отправлялся).
    #[serde(default)]
    pub user_mtime: u64,
}

/// «Документы\The Witcher 3»: обычная папка или перенесённая в OneDrive.
pub fn game_dir() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("USERPROFILE") {
        roots.push(PathBuf::from(&home).join("Documents"));
        roots.push(PathBuf::from(&home).join("OneDrive").join("Documents"));
    }
    if let Ok(od) = std::env::var("OneDrive") {
        roots.push(PathBuf::from(od).join("Documents"));
    }
    roots.into_iter()
        .map(|r| r.join("The Witcher 3"))
        .find(|d| d.join("gamesaves").is_dir())
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Самое свежее сохранение *.sav (по времени изменения; при равенстве — по имени).
pub fn newest(saves: &Path) -> Option<(PathBuf, Sent)> {
    std::fs::read_dir(saves).ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.to_ascii_lowercase().ends_with(".sav") || name.contains("sync-conflict") {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((e.path(), Sent { name, mtime: mtime_ms(&meta)?, size: meta.len(), user_mtime: 0 }))
        })
        .max_by(|(_, a), (_, b)| (a.mtime, &a.name).cmp(&(b.mtime, &b.name)))
}

/// Время изменения customUserData.json (0 — файла нет).
pub fn user_data_mtime(dir: &Path) -> u64 {
    std::fs::metadata(dir.join(USER_DATA)).ok().and_then(|m| mtime_ms(&m)).unwrap_or(0)
}

/// Свежее сохранение, если оно не то, что уже ушло.
pub fn changed(saves: &Path, sent: Option<&Sent>) -> Option<(PathBuf, Sent)> {
    newest(saves).filter(|(_, cur)| {
        sent.map(|s| (&s.name, s.mtime, s.size) != (&cur.name, cur.mtime, cur.size)).unwrap_or(true)
    })
}

pub fn load_sent(path: &Path) -> Option<Sent> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok())
}

pub fn save_sent(path: &Path, sent: &Sent) {
    if let Ok(text) = serde_json::to_string(sent) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_newest_and_skips_sent() {
        let dir = std::env::temp_dir().join(format!("bb-w3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["AutoSave_1.sav", "AutoSave_1.json", "AutoSave_1.png"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(dir.join("ManualSave_2.sav"), b"xy").unwrap();
        std::fs::write(dir.join("ManualSave_2.png"), b"xyz").unwrap();
        let (_, cur) = newest(&dir).unwrap();
        assert_eq!(cur.name, "ManualSave_2.sav");
        assert_eq!(cur.size, 2);
        assert!(changed(&dir, None).is_some());
        let sent = Sent { user_mtime: 123, ..cur.clone() };
        assert!(changed(&dir, Some(&sent)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
