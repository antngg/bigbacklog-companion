//! The Binding of Isaac (Repentance и Repentance+): сохранение на сайт.
//!
//! Игра с Steam Cloud пишет профили в «<Steam>\userdata\<номер>\250900\remote»:
//! rep+persistentgamedata1..3.dat (Repentance+) и rep_persistentgamedata1..3.dat
//! (Repentance), по файлу на слот. Без облака они лежат в «Документах», в
//! «My Games\Binding of Isaac Repentance+» (и «… Repentance»). После выхода из игры
//! (и при старте агента) отправляем КАЖДЫЙ изменившийся файл
//! (POST /api/agent/isaac-save?device=pc&name=…, тело — файл, ~7 КБ). Отметка —
//! isaac.json рядом с config.json: {имя: время изменения, мс}.
//! Пока isaac-ng.exe запущен, файл не трогаем: игра его пишет.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// The Binding of Isaac в Steam — id игры в каталоге агента.
pub const STEAM_APPID: &str = "250900";
/// Процесс игры: пока он жив, сохранение не читаем.
pub const EXE: &str = "isaac-ng.exe";

/// rep+persistentgamedata1.dat или rep_persistentgamedata3.dat (регистр любой).
pub fn is_save(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("rep") else { return false };
    let Some(rest) = rest.strip_prefix('+').or_else(|| rest.strip_prefix('_')) else { return false };
    let Some(num) = rest.strip_prefix("persistentgamedata").and_then(|s| s.strip_suffix(".dat")) else { return false };
    matches!(num, "1" | "2" | "3")
}

fn has_saves(dir: &Path) -> Option<u64> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| is_save(&e.file_name().to_string_lossy()))
        .filter_map(|e| mtime_ms(&e.metadata().ok()?))
        .max()?;
    Some(newest)
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

fn steam_root() -> Option<PathBuf> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam").ok()?;
    let path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(path.replace('/', "\\")))
}

/// Среди кандидатов папка с самым свежим сохранением Isaac. В userdata бывает
/// несколько аккаунтов Steam, в «Документах» — старые копии: берем ту, куда игра
/// писала последней.
pub fn pick_dir(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .filter(|d| d.is_dir())
        .filter_map(|d| has_saves(&d).map(|t| (t, d)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, d)| d)
}

pub fn saves_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = steam_root() {
        if let Ok(read) = std::fs::read_dir(root.join("userdata")) {
            candidates.extend(read.filter_map(|e| e.ok()).map(|e| e.path().join(STEAM_APPID).join("remote")));
        }
    }
    if let Some(docs) = dirs_documents() {
        for name in ["Binding of Isaac Repentance+", "Binding of Isaac Repentance"] {
            candidates.push(docs.join("My Games").join(name));
        }
    }
    pick_dir(candidates)
}

/// «Документы» пользователя: USERPROFILE\Documents (перенос в OneDrive не учитываем:
/// там игра сама пишет в облако Steam).
fn dirs_documents() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Documents"))
}

/// Сохранение: имя, время изменения (мс), размер.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub name: String,
    pub mtime: u64,
    pub size: u64,
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
            Some((e.path(), Sent { name, mtime: mtime_ms(&meta)?, size: meta.len() }))
        })
        .collect();
    out.sort_by(|(_, a), (_, b)| (a.mtime, &a.name).cmp(&(b.mtime, &b.name)));
    out
}

/// Сохранения, которых в отметке нет или которые с тех пор переписаны.
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
        assert!(is_save("rep+persistentgamedata1.dat"));
        assert!(is_save("REP_PersistentGameData3.dat"));
        assert!(!is_save("persistentgamedata1.dat"));
        assert!(!is_save("rep+persistentgamedata4.dat"));
        assert!(!is_save("rep+gamestate1.dat"));
        assert!(!is_save("rep+persistentgamedata1.dat.bak"));
        assert!(!is_save("afterbirthplus_persistentgamedata1.dat"));
    }

    #[test]
    fn sends_every_changed_save_and_picks_newest_folder() {
        let base = std::env::temp_dir().join(format!("bb-isaac-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("rep_persistentgamedata1.dat"), b"x").unwrap();
        std::fs::write(a.join("rep+gamestate1.dat"), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(b.join("rep+persistentgamedata1.dat"), b"xy").unwrap();
        std::fs::write(b.join("rep+persistentgamedata2.dat"), b"xyz").unwrap();
        assert_eq!(pick_dir([a.clone(), base.join("нет"), b.clone()]), Some(b.clone()));
        let first = changed(&b, &HashMap::new());
        assert_eq!(first.len(), 2);
        let mut sent: HashMap<String, u64> = first.iter().map(|(_, s)| (s.name.clone(), s.mtime)).collect();
        assert!(changed(&b, &sent).is_empty());
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(b.join("rep+persistentgamedata1.dat"), b"xyzw").unwrap();
        let again = changed(&b, &sent);
        assert_eq!(again.len(), 1);
        sent.insert(again[0].1.name.clone(), again[0].1.mtime);
        let mark = b.join("isaac.json");
        save_sent(&mark, &sent);
        assert_eq!(load_sent(&mark), sent);
        let _ = std::fs::remove_dir_all(&base);
    }
}
