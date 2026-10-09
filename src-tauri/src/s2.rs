//! Sacred 2 Remaster: сохранения на сайт.
//!
//! Игра пишет героев в «%LOCALAPPDATA%\Sacred2Remaster\user»: heroNN.sacred2save
//! (сам герой, сжатый), heroNN.sacred2stats (текстовая сводка, которую игра
//! считает сама: броня, навыки, смерти, достижения) и общий chest.sacred2chest.
//! После выхода из игры (и при старте агента) отправляем КАЖДЫЙ изменившийся
//! файл, от старых к новым: сайт сам раскладывает их по героям (класс и имя
//! есть в обоих файлах героя), первый раз уходят все
//! (POST /api/agent/s2-save?device=pc&name=…, тело — файл, единицы КБ).
//! Отметка — s2.json рядом с config.json: {имя: время изменения, мс}.
//! Пока sacred2.exe запущен, файлы не трогаем: игра их пишет.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Sacred 2 Remaster в Steam — id игры в каталоге агента.
pub const STEAM_APPID: &str = "3906660";
/// Процесс игры: пока он жив, сохранения не читаем.
pub const EXE: &str = "sacred2.exe";

/// %LOCALAPPDATA%\Sacred2Remaster\user, если есть.
pub fn saves_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("Sacred2Remaster").join("user");
    dir.is_dir().then_some(dir)
}

/// heroNN.sacred2save, heroNN.sacred2stats, chest.sacred2chest (регистр любой).
pub fn is_save(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "chest.sacred2chest" {
        return true;
    }
    let Some(rest) = lower.strip_prefix("hero") else { return false };
    let num = rest.strip_suffix(".sacred2save").or_else(|| rest.strip_suffix(".sacred2stats"));
    num.is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Файлы героев папки с временем изменения (мс), от старых к новым.
pub fn all(dir: &Path) -> Vec<(PathBuf, String, u64)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(PathBuf, String, u64)> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !is_save(&name) {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
            Some((e.path(), name, mtime))
        })
        .collect();
    // Сохранение героя раньше его сводки: на сайте сводка ложится к уже известному герою.
    out.sort_by(|a, b| (a.2, a.1.ends_with("stats"), &a.1).cmp(&(b.2, b.1.ends_with("stats"), &b.1)));
    out
}

/// Файлы, которых в отметке нет или которые с тех пор переписаны.
pub fn changed(dir: &Path, sent: &HashMap<String, u64>) -> Vec<(PathBuf, String, u64)> {
    all(dir).into_iter().filter(|(_, name, mtime)| sent.get(name) != Some(mtime)).collect()
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
    fn names() {
        assert!(is_save("hero01.sacred2save"));
        assert!(is_save("HERO12.sacred2stats"));
        assert!(is_save("chest.sacred2chest"));
        assert!(!is_save("latest"));
        assert!(!is_save("hero.sacred2save"));
        assert!(!is_save("steam_autocloud.vdf"));
    }

    #[test]
    fn sends_changed_only() {
        let dir = std::env::temp_dir().join(format!("bb-s2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["hero01.sacred2save", "hero01.sacred2stats", "chest.sacred2chest", "latest"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let first = changed(&dir, &HashMap::new());
        assert_eq!(first.len(), 3);
        let sent: HashMap<String, u64> = first.iter().map(|(_, n, m)| (n.clone(), *m)).collect();
        assert!(changed(&dir, &sent).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
