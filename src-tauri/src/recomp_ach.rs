//! Родные достижения рекомпов Xbox 360 (порты на ReXGlue: Forza Horizon в PinyonShift и др.).
//!
//! Игра открывает достижение, рантайм порта пишет его в `user\achievements\<TitleID>.toml`
//! (`[unlocked.155]`, `filetime = …`), но плашки не показывает и на сайт ничего не шлет. Приложение
//! следит за этим файлом: новое открытие — плашка (buildAchievementPopup в static/popups.js, со
//! звуком Xbox 360) и полный список открытого на сервер (app/recomp_achievements.py).
//!
//! Какие порты и где искать файл — из /api/agent/state (`recomp_ach`: Title ID, exe, папки с `*`
//! и `%ПЕРЕМЕННЫМИ%`); кроме папок из списка — вверх от exe запущенного порта. Названия для плашек
//! приходят один раз на игру и лежат на диске: открытое без сети показывается сразу, а на сервер
//! уходит, когда связь вернется.
//!
//! Задним числом плашек нет: первый раз увиденный файл только запоминается (и отправляется — это
//! настоящие достижения игры с их временем), плашки — за то, что открылось после.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Секунды между FILETIME (1601) и unix-временем.
const FILETIME_UNIX_DIFF: u64 = 11_644_473_600;
/// Сколько уровней вверх от exe порта искать его папку пользователя.
const UP_LEVELS: usize = 5;

#[derive(Clone, Debug, Default)]
pub struct Title {
    pub title_id: String,
    pub exes: Vec<String>,
    pub dirs: Vec<String>,
}

static TITLES: Mutex<Vec<Title>> = Mutex::new(Vec::new());

pub fn set_from_state(state: &Value) {
    let Some(arr) = state.get("recomp_ach").and_then(|v| v.as_array()) else { return };
    let strs = |v: &Value, key: &str| -> Vec<String> {
        v.get(key).and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let list = arr.iter().filter_map(|v| {
        let tid = v.get("title_id")?.as_str()?.to_uppercase();
        (tid.len() == 8).then(|| Title { title_id: tid, exes: strs(v, "exes"), dirs: strs(v, "dirs") })
    }).collect();
    *TITLES.lock().unwrap() = list;
}

pub fn titles() -> Vec<Title> {
    TITLES.lock().unwrap().clone()
}

/// Открытое из файла порта: id → unix-время.
pub fn parse_unlocks(text: &str) -> BTreeMap<u32, u64> {
    let mut out = BTreeMap::new();
    let mut current: Option<u32> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[unlocked.") {
            current = rest.trim_end_matches(']').trim().parse().ok();
            if let Some(id) = current {
                out.insert(id, 0);
            }
        } else if line.starts_with('[') {
            current = None;
        } else if let (Some(id), Some(rest)) = (current, line.strip_prefix("filetime")) {
            let ft: u64 = rest.trim_start_matches([' ', '=']).trim().parse().unwrap_or(0);
            let unix = (ft / 10_000_000).saturating_sub(FILETIME_UNIX_DIFF);
            out.insert(id, unix);
        }
    }
    out
}

/// `%LOCALAPPDATA%\PinyonShift\source\*\.local\preview\user\achievements` → существующие папки.
pub fn expand(pattern: &str) -> Vec<PathBuf> {
    let mut text = String::new();
    let mut rest = pattern;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else { break };
        text.push_str(&rest[..start]);
        let name = &rest[start + 1..start + 1 + len];
        match std::env::var(name) {
            Ok(v) => text.push_str(&v),
            Err(_) => return Vec::new(),
        }
        rest = &rest[start + 2 + len..];
    }
    text.push_str(rest);
    let text = text.replace('/', "\\");
    let mut parts = text.split('\\').filter(|p| !p.is_empty());
    let Some(first) = parts.next() else { return Vec::new() };
    let mut found = vec![PathBuf::from(format!("{first}\\"))];
    for part in parts {
        let mut next = Vec::new();
        for base in &found {
            if part == "*" {
                if let Ok(rd) = std::fs::read_dir(base) {
                    next.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()));
                }
            } else {
                let p = base.join(part);
                if p.exists() {
                    next.push(p);
                }
            }
        }
        found = next;
        if found.is_empty() {
            break;
        }
    }
    found
}

/// Папки порта над его exe, где ReXGlue держит `user\achievements`.
fn from_exe(exe_path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dir = exe_path.parent();
    for _ in 0..UP_LEVELS {
        let Some(d) = dir else { break };
        for sub in [&["user", "achievements"][..], &[".local", "preview", "user", "achievements"][..]] {
            let p = sub.iter().fold(d.to_path_buf(), |acc, s| acc.join(s));
            if p.is_dir() {
                out.push(p);
            }
        }
        dir = d.parent();
    }
    out
}

/// Файл достижений игры: из всех найденных — самый свежий (у порта бывают копии в резервных папках).
pub fn find_file(t: &Title, exe_path: impl Fn(&str) -> Option<String>) -> Option<(PathBuf, SystemTime)> {
    let mut dirs: Vec<PathBuf> = t.dirs.iter().flat_map(|d| expand(d)).collect();
    for exe in &t.exes {
        if let Some(path) = exe_path(exe) {
            dirs.extend(from_exe(Path::new(&path)));
        }
    }
    dirs.iter()
        .map(|d| d.join(format!("{}.toml", t.title_id)))
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|m| (p, m)))
        .max_by_key(|(_, m)| *m)
}

// ---------- Что уже видели и отправили (на диске) ----------

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct TitleState {
    /// Открытое, о котором уже знаем (плашка за него была или не нужна).
    pub seen: Vec<u32>,
    /// Последний снимок дошел до сервера.
    pub sent: bool,
    /// Полный снимок открытого: id → unix.
    pub unlocked: BTreeMap<u32, u64>,
    /// Названия для плашек с сервера: id → {name, desc, gamerscore, icon}.
    #[serde(default)]
    pub names: HashMap<String, Value>,
}

pub type State = HashMap<String, TitleState>;

fn state_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("BigBacklogAgent").join("recomp_ach.json")
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

/// Новый снимок файла: что из открытого — новое (для плашек). У игры, которую видим впервые, нового
/// нет: все открытое запоминается молча.
pub fn absorb(st: &mut TitleState, first: bool, unlocked: BTreeMap<u32, u64>) -> Vec<u32> {
    let fresh: Vec<u32> = if first {
        Vec::new()
    } else {
        unlocked.keys().filter(|id| !st.seen.contains(id)).copied().collect()
    };
    if unlocked != st.unlocked {
        st.sent = false;
    }
    st.seen = unlocked.keys().copied().collect();
    st.unlocked = unlocked;
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rexglue_unlock_file() {
        let text = "# Achievement unlock state - managed by ReXGlue runtime\n\n[unlocked.155]\nfiletime = 134371455832734179\n\n[unlocked.160]\nfiletime = 134371455900000000\n";
        let got = parse_unlocks(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[&155], 1_792_671_983);
        assert!(got[&160] > got[&155]);
    }

    #[test]
    fn first_sight_is_silent_then_new_ones_pop() {
        let mut st = TitleState::default();
        let first = parse_unlocks("[unlocked.155]\nfiletime = 134371455832734179\n");
        assert!(absorb(&mut st, true, first).is_empty());
        assert!(!st.sent);
        st.sent = true;
        let next = parse_unlocks("[unlocked.155]\nfiletime = 134371455832734179\n[unlocked.156]\nfiletime = 134371455900000000\n");
        assert_eq!(absorb(&mut st, false, next.clone()), vec![156]);
        assert!(!st.sent);
        st.sent = true;
        assert!(absorb(&mut st, false, next).is_empty());
        assert!(st.sent);
    }

    #[test]
    fn expands_env_and_star() {
        let tmp = std::env::temp_dir().join("bb_recomp_ach_test");
        let deep = tmp.join("source").join("0.1.1").join("user");
        std::fs::create_dir_all(&deep).unwrap();
        std::env::set_var("BB_RA_TEST", tmp.to_string_lossy().to_string());
        let got = expand("%BB_RA_TEST%\\source\\*\\user");
        assert_eq!(got, vec![deep]);
        assert!(expand("%BB_RA_NOPE%\\x").is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
