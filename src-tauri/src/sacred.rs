//! Sacred Gold: сохранение на сайт.
//!
//! Игра пишет сохранения в «<папка игры>\save\GAMEnn.PAK» (герои *.pax рядом
//! не нужны). После выхода из игры (и при старте агента) отправляем на сайт
//! КАЖДОЕ изменившееся GAME*.PAK, от старых к новым: у человека бывает
//! несколько героев, и сайт держит каждого по классу и имени из самого файла —
//! герой, которым давно не играли, тоже должен доехать. Первый раз уходят все
//! (POST /api/agent/sacred-save?device=pc&name=…, тело — файл, ~2,6 МБ).
//! Отметка — sacred.json рядом с config.json: {имя: время изменения, мс}.
//! Следом — параметры героя из окна персонажа, которые мод геймпада пишет
//! рядом с каждым сохранением: GAMEnn.bb.json того же слота
//! (POST /api/agent/sacred-stats?device=pc&name=…, тело — JSON).
//! Пока Sacred.exe запущен, файл не трогаем: игра его пишет.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// Sacred Gold в Steam — id игры в каталоге агента.
pub const STEAM_APPID: &str = "12320";
/// Процесс игры: пока он жив, сохранение не читаем.
pub const EXE: &str = "sacred.exe";

/// Сохранение в папке игры: имя, время изменения (мс), размер.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub name: String,
    /// Время изменения файла, мс.
    pub mtime: u64,
    pub size: u64,
}

pub fn saves_dir() -> Option<PathBuf> {
    let dir = crate::scan::steam_app_dir(STEAM_APPID)?.join("save");
    dir.is_dir().then_some(dir)
}

/// GAMEnn.PAK: «GAME», две и больше цифр, «.PAK» (регистр любой).
fn is_save(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(num) = lower.strip_prefix("game").and_then(|s| s.strip_suffix(".pak")) else { return false };
    num.len() >= 2 && num.bytes().all(|b| b.is_ascii_digit())
}

/// Все сохранения папки, от старых к новым (при равном времени — по имени).
pub fn all(dir: &Path) -> Vec<(PathBuf, Sent)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(PathBuf, Sent)> = read
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
            Some((e.path(), Sent { name, mtime, size: meta.len() }))
        })
        .collect();
    out.sort_by(|(_, a), (_, b)| (a.mtime, &a.name).cmp(&(b.mtime, &b.name)));
    out
}

/// Сохранения, которых в отметке нет или которые с тех пор переписаны.
pub fn changed(dir: &Path, sent: &HashMap<String, u64>) -> Vec<(PathBuf, Sent)> {
    all(dir).into_iter().filter(|(_, cur)| sent.get(&cur.name) != Some(&cur.mtime)).collect()
}

/// Допуск по времени: на FAT (карта памяти) время файла хранится с шагом 2 с.
const STATS_SLACK_MS: u64 = 2000;

fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// GAMEnn.bb.json рядом с сохранением (регистр имени любой), если он не старше
/// самого сохранения: старый файл описывает героя прошлой записи слота.
pub fn stats_for(pak: &Path) -> Option<(PathBuf, String)> {
    let dir = pak.parent()?;
    let stem = pak.file_stem()?.to_string_lossy().to_string();
    let want = format!("{stem}.bb.json").to_ascii_lowercase();
    let pak_ms = mtime_ms(&std::fs::metadata(pak).ok()?)?;
    std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).find_map(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        if name.to_ascii_lowercase() != want {
            return None;
        }
        let meta = e.metadata().ok()?;
        (meta.is_file() && mtime_ms(&meta)? + STATS_SLACK_MS >= pak_ms).then(|| (e.path(), name))
    })
}

/// Отметка {имя: время, мс}. Старая отметка одного файла ({name, mtime, size},
/// агент до 06.10) читается как отметка этого файла.
pub fn load_sent(path: &Path) -> HashMap<String, u64> {
    let Some(v) = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return HashMap::new();
    };
    if let Ok(old) = serde_json::from_value::<Sent>(v.clone()) {
        return HashMap::from([(old.name, old.mtime)]);
    }
    serde_json::from_value(v).unwrap_or_default()
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
    fn save_names() {
        assert!(is_save("GAME01.PAK"));
        assert!(is_save("game123.pak"));
        assert!(!is_save("GAME1.PAK"));
        assert!(!is_save("GAME.PAK"));
        assert!(!is_save("hero06.pax"));
        assert!(!is_save("GAME01.PAK.bak"));
        assert!(!is_save("GAMEab.PAK"));
    }

    #[test]
    fn sends_every_changed_save() {
        let dir = std::env::temp_dir().join(format!("bb-sacred-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["GAME01.PAK", "hero06.pax", "Settings.cfg"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(dir.join("GAME02.PAK"), b"xy").unwrap();
        // Первый раз — все сохранения, от старых к новым.
        let first = changed(&dir, &HashMap::new());
        let names: Vec<_> = first.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(names, ["GAME01.PAK", "GAME02.PAK"]);
        assert_eq!(first[1].1.size, 2);
        let mut sent: HashMap<String, u64> = first.iter().map(|(_, s)| (s.name.clone(), s.mtime)).collect();
        assert!(changed(&dir, &sent).is_empty());
        // Переписан один слот — уходит только он.
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(dir.join("GAME01.PAK"), b"xyz").unwrap();
        let again = changed(&dir, &sent);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].1.name, "GAME01.PAK");
        sent.insert(again[0].1.name.clone(), again[0].1.mtime);
        // Отметка на диске и чтение старой (одного файла).
        let mark = dir.join("sacred.json");
        save_sent(&mark, &sent);
        assert_eq!(load_sent(&mark), sent);
        std::fs::write(&mark, "{\"name\":\"GAME02.PAK\",\"mtime\":5,\"size\":2}").unwrap();
        assert_eq!(load_sent(&mark), HashMap::from([("GAME02.PAK".to_string(), 5)]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_next_to_save() {
        let dir = std::env::temp_dir().join(format!("bb-sacred-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pak = dir.join("GAME02.PAK");
        std::fs::write(&pak, b"x").unwrap();
        assert!(stats_for(&pak).is_none());
        std::fs::write(dir.join("GAME01.bb.json"), b"{}").unwrap();
        assert!(stats_for(&pak).is_none());
        std::fs::write(dir.join("game02.BB.json"), b"{}").unwrap();
        let (_, name) = stats_for(&pak).unwrap();
        assert_eq!(name, "game02.BB.json");
        // Старше сохранения больше чем на допуск — не его.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(dir.join("game02.BB.json")).unwrap().set_modified(old).unwrap();
        assert!(stats_for(&pak).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
