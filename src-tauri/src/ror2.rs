//! Risk of Rain 2: профиль игры на сайт.
//!
//! Игра пишет профиль в Steam Cloud: «<Steam>\userdata\<номер>\632360\remote\UserProfiles»,
//! файл <GUID>.xml (журнал, испытания, статистика, 200–400 КБ). После выхода из игры
//! (и при старте агента) отправляем КАЖДЫЙ изменившийся профиль
//! (POST /api/agent/ror2-save?device=pc&name=…, тело — файл). Отметка — ror2.json
//! рядом с config.json: {имя: время изменения, мс}.
//! Пока «Risk of Rain 2.exe» запущен, файл не трогаем: игра его пишет.
//!
//! История попыток (журнал «История попыток») лежит не в облаке, а в папке игры:
//! «Risk of Rain 2_Data\RunReports\History\<runGuid>.xml», до 30 последних забегов.
//! Ее файлы уходят той же ручкой (сайт различает профиль и отчет по содержимому),
//! отметка в том же ror2.json под ключом «runs/<имя>».

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// Risk of Rain 2 в Steam — id игры в каталоге агента.
pub const STEAM_APPID: &str = "632360";
/// Процесс игры: пока он жив, профиль не читаем.
pub const EXE: &str = "Risk of Rain 2.exe";

/// Профиль: <что-то>.xml (регистр любой), без временных файлов игры.
pub fn is_save(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !lower.starts_with('.') && lower.len() > 4 && lower.ends_with(".xml")
}

fn has_saves(dir: &Path) -> Option<u64> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| is_save(&e.file_name().to_string_lossy()))
        .filter_map(|e| mtime_ms(&e.metadata().ok()?))
        .max()
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

fn steam_root() -> Option<PathBuf> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam").ok()?;
    let path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(path.replace('/', "\\")))
}

/// Среди кандидатов папка с самым свежим профилем: в userdata бывает несколько
/// аккаунтов Steam, берем тот, где играли последним.
pub fn pick_dir(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .filter(|d| d.is_dir())
        .filter_map(|d| has_saves(&d).map(|t| (t, d)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, d)| d)
}

pub fn saves_dir() -> Option<PathBuf> {
    let root = steam_root()?;
    let read = std::fs::read_dir(root.join("userdata")).ok()?;
    pick_dir(
        read.filter_map(|e| e.ok())
            .map(|e| e.path().join(STEAM_APPID).join("remote").join("UserProfiles")),
    )
}

/// Папка истории попыток в установленной игре (MorgueManager игры пишет туда отчет
/// о каждом законченном забеге).
pub fn history_dir() -> Option<PathBuf> {
    crate::scan::steam_app_dir(STEAM_APPID)
        .map(|d| d.join("Risk of Rain 2_Data").join("RunReports").join("History"))
        .filter(|d| d.is_dir())
}

/// Профиль: имя, время изменения (мс), размер.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub name: String,
    pub mtime: u64,
    pub size: u64,
}

/// Все профили папки, от старых к новым (при равном времени — по имени).
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
            Some((e.path(), Sent { name, mtime: mtime_ms(&meta)?, size: meta.len() }))
        })
        .collect();
    out.sort_by(|(_, a), (_, b)| (a.mtime, &a.name).cmp(&(b.mtime, &b.name)));
    out
}

/// Профили, которых в отметке нет или которые с тех пор переписаны.
pub fn changed(dir: &Path, sent: &HashMap<String, u64>) -> Vec<(PathBuf, Sent)> {
    all(dir).into_iter().filter(|(_, cur)| sent.get(&cur.name) != Some(&cur.mtime)).collect()
}

pub fn load_sent(path: &Path) -> HashMap<String, u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
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
        assert!(is_save("ac01da9f-6218-4d43-bf69-225e56e47f71.xml"));
        assert!(is_save("Profile.XML"));
        assert!(!is_save(".xml"));
        assert!(!is_save(".tmp.xml"));
        assert!(!is_save("profile.xml.bak"));
        assert!(!is_save("remotecache.vdf"));
    }

    #[test]
    fn sends_every_changed_profile_and_picks_newest_folder() {
        let base = std::env::temp_dir().join(format!("bb-ror2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("old.xml"), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(b.join("p1.xml"), b"xy").unwrap();
        std::fs::write(b.join("p2.xml"), b"xyz").unwrap();
        std::fs::write(b.join("notes.txt"), b"x").unwrap();
        assert_eq!(pick_dir([a.clone(), base.join("нет"), b.clone()]), Some(b.clone()));
        let first = changed(&b, &HashMap::new());
        assert_eq!(first.len(), 2);
        let mut sent: HashMap<String, u64> = first.iter().map(|(_, s)| (s.name.clone(), s.mtime)).collect();
        assert!(changed(&b, &sent).is_empty());
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(b.join("p1.xml"), b"xyzw").unwrap();
        let again = changed(&b, &sent);
        assert_eq!(again.len(), 1);
        sent.insert(again[0].1.name.clone(), again[0].1.mtime);
        let mark = b.join("ror2.json");
        save_sent(&mark, &sent);
        assert_eq!(load_sent(&mark), sent);
        let _ = std::fs::remove_dir_all(&base);
    }
}
