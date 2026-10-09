//! Cyberpunk 2077: сохранение на сайт.
//!
//! Игра пишет сохранения папками в «%USERPROFILE%\Saved Games\CD Projekt Red\Cyberpunk 2077»:
//! AutoSave-N, ManualSave-N, QuickSave-N, EndGameSave-N, PointOfNoReturnSave…, в каждой
//! sav.dat (3–8 МБ), metadata.9.json и screenshot.png. Герой на сайте — прохождение
//! (Data.metadata.playthroughID из metadata.9.json), у него десятки папок, сайт держит
//! самую свежую. Поэтому после выхода из игры (и при старте агента) у каждого прохождения
//! берем только самую свежую папку (по времени sav.dat) и шлем, если она изменилась:
//! POST /api/agent/cp77-save?device=pc&name=<папка>&folder=…, тело — metadata.9.json и сразу
//! sav.dat; при ответе «saved» следом снимок экрана (part=shot&key=…).
//! Отметка — cp77.json рядом с config.json: {имя папки: время sav.dat, мс}, только у
//! последних папок прохождений. Пока Cyberpunk2077.exe жив, файлы не трогаем.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Cyberpunk 2077 в Steam — id игры в каталоге агента.
pub const STEAM_APPID: &str = "1091500";
/// То же издание в GOG Galaxy.
pub const GOG_ID: &str = "1423049311";
/// Процесс игры: пока он жив, сохранения не читаем.
pub const EXE: &str = "Cyberpunk2077.exe";
pub const SAV: &str = "sav.dat";
pub const META: &str = "metadata.9.json";
pub const SHOT: &str = "screenshot.png";

pub fn saves_dir() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    let dir = PathBuf::from(home).join("Saved Games").join("CD Projekt Red").join("Cyberpunk 2077");
    dir.is_dir().then_some(dir)
}

/// Имя папки, которое примет сервер (FOLDER_RE в app/routers/cp77.py).
pub fn is_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// Папка сохранения: имя, время sav.dat (мс), прохождение ("" — metadata нет или не читается).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Save {
    pub name: String,
    pub mtime: u64,
    pub playthrough: String,
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

fn playthrough(folder: &Path) -> String {
    std::fs::read_to_string(folder.join(META))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.pointer("/Data/metadata/playthroughID").and_then(|p| p.as_str()).map(String::from))
        .unwrap_or_default()
}

/// У каждого прохождения самая свежая папка (при равном времени — по имени), от старых к новым.
pub fn newest_per_playthrough(dir: &Path) -> Vec<(PathBuf, Save)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut best: HashMap<String, (PathBuf, Save)> = HashMap::new();
    for e in read.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if !is_folder_name(&name) || !e.path().is_dir() {
            continue;
        }
        let Some(mtime) = std::fs::metadata(e.path().join(SAV)).ok().filter(|m| m.is_file()).and_then(|m| mtime_ms(&m))
        else {
            continue;
        };
        let cur = Save { name, mtime, playthrough: playthrough(&e.path()) };
        let newer = best
            .get(&cur.playthrough)
            .map(|(_, b)| (cur.mtime, &cur.name) > (b.mtime, &b.name))
            .unwrap_or(true);
        if newer {
            best.insert(cur.playthrough.clone(), (e.path(), cur));
        }
    }
    let mut out: Vec<(PathBuf, Save)> = best.into_values().collect();
    out.sort_by(|(_, a), (_, b)| (a.mtime, &a.name).cmp(&(b.mtime, &b.name)));
    out
}

/// Последние папки прохождений, которых в отметке нет или которые с тех пор переписаны.
pub fn changed(dir: &Path, sent: &HashMap<String, u64>) -> Vec<(PathBuf, Save)> {
    newest_per_playthrough(dir).into_iter().filter(|(_, s)| sent.get(&s.name) != Some(&s.mtime)).collect()
}

/// Тело для сервера: metadata.9.json и сразу sav.dat (без metadata — один sav.dat).
pub fn body(folder: &Path) -> Option<Vec<u8>> {
    let sav = std::fs::read(folder.join(SAV)).ok()?;
    let mut out = std::fs::read(folder.join(META)).unwrap_or_default();
    out.extend_from_slice(&sav);
    Some(out)
}

/// Отметка без устаревших папок: держим только последние папки прохождений.
pub fn prune(sent: &mut HashMap<String, u64>, current: &[(PathBuf, Save)]) {
    sent.retain(|k, _| current.iter().any(|(_, s)| &s.name == k));
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

    fn save(dir: &Path, name: &str, pt: Option<&str>) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        if let Some(pt) = pt {
            let meta = format!("{{\"RootType\":\"saveMetadataContainer\",\"Data\":{{\"metadata\":{{\"playthroughID\":\"{pt}\"}}}}}}");
            std::fs::write(d.join(META), meta).unwrap();
        }
        std::fs::write(d.join(SAV), b"VASC....").unwrap();
    }

    #[test]
    fn folder_names() {
        assert!(is_folder_name("AutoSave-0"));
        assert!(is_folder_name("PointOfNoReturnSave"));
        assert!(!is_folder_name(""));
        assert!(!is_folder_name("Сохранение"));
        assert!(!is_folder_name("a b"));
    }

    #[test]
    fn newest_folder_of_each_playthrough() {
        let base = std::env::temp_dir().join(format!("bb-cp77-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        save(&base, "AutoSave-0", Some("aaaa"));
        save(&base, "ManualSave-0", Some("bbbb"));
        std::fs::write(base.join("user.gls"), b"x").unwrap();
        std::fs::create_dir_all(base.join("Empty-1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        save(&base, "QuickSave-0", Some("aaaa"));
        let first = changed(&base, &HashMap::new());
        let names: Vec<&str> = first.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(names, ["ManualSave-0", "QuickSave-0"]);
        let mut sent: HashMap<String, u64> = first.iter().map(|(_, s)| (s.name.clone(), s.mtime)).collect();
        sent.insert("AutoSave-9".into(), 1);
        prune(&mut sent, &newest_per_playthrough(&base));
        assert_eq!(sent.len(), 2);
        assert!(changed(&base, &sent).is_empty());
        std::thread::sleep(std::time::Duration::from_millis(30));
        save(&base, "AutoSave-0", Some("aaaa"));
        let again = changed(&base, &sent);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].1.name, "AutoSave-0");
        let b = body(&again[0].0).unwrap();
        assert!(b.starts_with(b"{") && b.ends_with(b"VASC...."));
        let mark = base.join("cp77.json");
        save_sent(&mark, &sent);
        assert_eq!(load_sent(&mark), sent);
        let _ = std::fs::remove_dir_all(&base);
    }
}
