//! Родные трофеи рекомпов PS4 (Uncharted for Windows: три игры коллекции, у каждой свой набор).
//!
//! Порт сам показывает плашку трофея, а полученное пишет в `<папка сохранений>\trophy\<NPWR id>.log`
//! строками «id unix-время». На сайт он ничего не шлет: приложение следит за логами и отправляет
//! полный список полученного каждого набора (app/recomp_trophies.py) и показывает свою плашку,
//! как у достижений Xbox 360 (у порта своя тоже есть: BB_TROPHY_POPUP=0 в лаунчере ее прячет).
//!
//! Какие порты и наборы — из /api/agent/state (`recomp_trophies`: exe, NPWR id, файл настроек
//! лаунчера с выбранной человеком папкой сохранений). Папка сохранений — из настроек лаунчера,
//! иначе вверх от exe запущенного порта (`user\trophy`); найденная запоминается на диске, чтобы
//! после перезапуска приложения ее не ждать до следующего запуска игры.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::recomp_ach::{expand, TitleState};

/// Сколько уровней вверх от exe порта искать его папку `user`.
const UP_LEVELS: usize = 4;

#[derive(Clone, Debug, Default)]
pub struct Port {
    pub slug: String,
    pub exes: Vec<String>,
    pub sets: Vec<String>,
    /// Файл настроек лаунчера и ключ в нем с папкой сохранений (пусто — папка по умолчанию).
    pub user_json: Option<(String, String)>,
}

static PORTS: Mutex<Vec<Port>> = Mutex::new(Vec::new());

pub fn set_from_state(state: &Value) {
    let Some(arr) = state.get("recomp_trophies").and_then(|v| v.as_array()) else { return };
    let strs = |v: &Value, key: &str| -> Vec<String> {
        v.get(key).and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let list = arr.iter().filter_map(|v| {
        let slug = v.get("slug")?.as_str()?.to_string();
        let user_json = v.get("user_json").and_then(|u| {
            Some((u.get("file")?.as_str()?.to_string(), u.get("key")?.as_str()?.to_string()))
        });
        Some(Port { slug, exes: strs(v, "exes"), sets: strs(v, "sets"), user_json })
    }).collect();
    *PORTS.lock().unwrap() = list;
}

pub fn ports() -> Vec<Port> {
    PORTS.lock().unwrap().clone()
}

/// Полученное из лога порта: id → самое раннее unix-время. Битые строки пропускаются.
pub fn parse_log(text: &str) -> BTreeMap<u32, u64> {
    let mut out: BTreeMap<u32, u64> = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(id), Some(at)) = (parts.next(), parts.next()) else { continue };
        let (Ok(id), Ok(at)) = (id.parse::<u32>(), at.parse::<u64>()) else { continue };
        out.entry(id).and_modify(|t| *t = (*t).min(at)).or_insert(at);
    }
    out
}

/// Папка `trophy`, выбранная человеком в лаунчере (ключ настроек не пустой).
fn from_settings(file: &str, key: &str) -> Option<PathBuf> {
    let path = expand(file).into_iter().next()?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let dir = v.get(key)?.as_str()?.trim();
    (!dir.is_empty()).then(|| PathBuf::from(dir).join("trophy")).filter(|p| p.is_dir())
}

/// `user\trophy` над exe порта (лаунчер лежит в корне порта, сама игра в `bin`).
fn from_exe(exe_path: &Path) -> Option<PathBuf> {
    let mut dir = exe_path.parent();
    for _ in 0..UP_LEVELS {
        let d = dir?;
        let p = d.join("user").join("trophy");
        if p.is_dir() {
            return Some(p);
        }
        dir = d.parent();
    }
    None
}

/// Папка логов трофеев порта: настройки лаунчера, запущенный exe, запомненная раньше.
pub fn find_dir(p: &Port, known: Option<&String>, exe_path: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = p.user_json.as_ref().and_then(|(f, k)| from_settings(f, k)) {
        return Some(dir);
    }
    if let Some(dir) = p.exes.iter().filter_map(|e| exe_path(e)).find_map(|path| from_exe(Path::new(&path))) {
        return Some(dir);
    }
    known.map(PathBuf::from).filter(|d| d.is_dir())
}

// ---------- Что уже отправили (на диске) ----------

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    /// По набору (NPWR id): снимок полученного и дошел ли он до сервера.
    #[serde(default)]
    pub sets: HashMap<String, TitleState>,
    /// По порту (slug): найденная папка логов.
    #[serde(default)]
    pub dirs: HashMap<String, String>,
}

fn state_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("BigBacklogAgent").join("recomp_trophy.json")
}

pub fn load_state() -> State {
    std::fs::read_to_string(state_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_state(state: &State) {
    let path = state_path();
    let tmp = path.with_extension("tmp");
    if let Ok(text) = serde_json::to_string(state) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_log_keeping_earliest() {
        let got = parse_log("5 1792793619\n\nbad line\n12 1792800000\n5 1792700000\n7\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[&5], 1_792_700_000);
        assert_eq!(got[&12], 1_792_800_000);
    }

    #[test]
    fn finds_user_trophy_above_exe() {
        let tmp = std::env::temp_dir().join("bb_recomp_trophy_test");
        let trophy = tmp.join("user").join("trophy");
        std::fs::create_dir_all(&trophy).unwrap();
        std::fs::create_dir_all(tmp.join("bin")).unwrap();
        assert_eq!(from_exe(&tmp.join("bin").join("bb-probe.exe")), Some(trophy.clone()));
        assert_eq!(from_exe(&tmp.join("Uncharted.exe")), Some(trophy.clone()));
        let settings = tmp.join("settings.json");
        std::fs::write(&settings, format!("{{\"user_dir\": {:?}}}", tmp.join("user").to_string_lossy())).unwrap();
        assert_eq!(from_settings(&settings.to_string_lossy(), "user_dir"), Some(trophy));
        std::fs::write(&settings, "{\"user_dir\": \"\"}").unwrap();
        assert_eq!(from_settings(&settings.to_string_lossy(), "user_dir"), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
