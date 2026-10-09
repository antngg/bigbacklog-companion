//! Diablo II: Resurrected: офлайн-сохранения на сайт.
//!
//! Игра пишет персонажей в «Saved Games\Diablo II Resurrected» профиля:
//! <Имя>.d2s и общий тайник ModernSharedStashSoftCoreV2.d2i. После выхода из
//! игры (и при старте агента) отправляем на сайт то, что изменилось с прошлой
//! отправки, по одному файлу (POST /api/agent/d2r-save?name=…, тело — файл).
//! Сервер сам разбирает файлы и показывает персонажа во вкладке «Прогресс».
//! Онлайн-персонажи (.ctlo/.keyo — служебные заглушки) и копии Syncthing
//! (*.sync-conflict-*) не трогаем.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// D2R в Steam (Infernal Edition) — id игры в каталоге агента.
pub const STEAM_APPID: &str = "2536520";
/// Процесс игры (Battle.net и Steam одинаково): пока он жив, сохранения не шлем.
pub const EXE: &str = "D2R.exe";
const STASH: &str = "ModernSharedStashSoftCoreV2.d2i";

pub fn saves_dir() -> Option<PathBuf> {
    let base = std::env::var("USERPROFILE").ok()?;
    let dir = PathBuf::from(base).join("Saved Games").join("Diablo II Resurrected");
    dir.is_dir().then_some(dir)
}

/// Файлы для сайта с временем изменения (секунды): сначала персонажи, потом
/// тайник — сервер не примет тайник, пока у него нет ни одного персонажа.
pub fn files(dir: &Path) -> Vec<(PathBuf, String, u64)> {
    let mut out: Vec<(PathBuf, String, u64)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let lower = name.to_lowercase();
                    let wanted = (lower.ends_with(".d2s") || name == STASH) && !lower.contains("sync-conflict");
                    if !wanted {
                        return None;
                    }
                    let mtime = e.metadata().ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
                    Some((e.path(), name, mtime))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|(_, name, _)| (name == STASH, name.clone()));
    out
}

/// Что изменилось с прошлой отправки.
pub fn changed(dir: &Path, sent: &HashMap<String, u64>) -> Vec<(PathBuf, String, u64)> {
    files(dir).into_iter().filter(|(_, name, mtime)| sent.get(name) != Some(mtime)).collect()
}

pub fn load_sent(path: &Path) -> HashMap<String, u64> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_sent(path: &Path, sent: &HashMap<String, u64>) {
    if let Ok(text) = serde_json::to_string(sent) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_characters_then_stash_and_skips_noise() {
        let dir = std::env::temp_dir().join(format!("bb-d2r-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [STASH, "Magicman.d2s", "Freya90080365.ctlo", "Magicman.sync-conflict-1.d2s", "Settings.json"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let names: Vec<String> = files(&dir).into_iter().map(|(_, n, _)| n).collect();
        assert_eq!(names, vec!["Magicman.d2s".to_string(), STASH.to_string()]);
        let mut sent = HashMap::new();
        for (_, n, m) in files(&dir) {
            sent.insert(n, m);
        }
        assert!(changed(&dir, &sent).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
