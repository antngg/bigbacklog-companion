//! Сохранения игр по описанию с сервера: новая игра без новой сборки агента.
//!
//! Сервер (app/save_specs.py) отдает в /api/agent/state список `save_specs`: где игра
//! пишет сохранения, какие файлы брать, какой процесс ждать и куда слать. Агент ищет
//! файлы, после выхода из игры (и при своем старте) отправляет изменившиеся тем же
//! способом, что и встроенные игры: POST <url>?device=pc&name=<файл>&folder=<папка>,
//! тело — сам файл. Отметка «<id>.json» рядом с config.json: {путь от корня: время, мс}.
//!
//! Описание (все поля, кроме id/url/win/files, необязательны):
//!   id, name          ключ игры на сервере и название в окне
//!   steam_appids      id в Steam: конец сессии этой игры — повод отправить, иконка окна
//!   exe               процессы игры: пока жив любой, файлы не трогаем
//!   url               ручка приема (/api/agent/<id>-save)
//!   win               корни на Windows: %ПЕРЕМЕННАЯ%, {steam} (папка Steam), {app}
//!                     (папка установки игры по первому appid), «*» в имени папки
//!   pick              all — все найденные корни, newest — корень с самым свежим файлом
//!   depth             0 — файлы прямо в корне, 1 — и в его подпапках (аккаунты Steam)
//!   files             маски имен (регистр любой, «*» — любые знаки)
//!   first             имена, которые слать раньше остальных
//!   send              changed — все изменившиеся, newest — только самый свежий из них
//!   max_kb, skip_empty  не слать большие и пустые
//!
//! Игры, которые агент знает сам (saves::GAMES), из списка не берутся.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    pub id: String,
    pub name: String,
    pub steam_appids: Vec<String>,
    pub exe: Vec<String>,
    pub url: String,
    pub win: Vec<String>,
    pub pick_newest: bool,
    pub depth: u8,
    pub files: Vec<String>,
    pub first: Vec<String>,
    pub newest_only: bool,
    pub max_bytes: u64,
    pub skip_empty: bool,
    /// Спутник другой игры (снимок героя Starfield): в окне «Сохранения игр» не своя строка,
    /// а файлы в строке игры part_of с подписью part_label; «Отправить сейчас» игры шлет и его.
    pub part_of: Option<String>,
    pub part_label: String,
}

fn strs(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

impl Spec {
    pub fn from_value(v: &Value) -> Option<Spec> {
        let id = v.get("id")?.as_str()?.to_string();
        let url = v.get("url")?.as_str()?.to_string();
        // Только ручки этого сайта и только простой ключ: описание приходит с сервера,
        // но лишний раз не доверяем ему путь отправки.
        if !url.starts_with("/api/agent/") || id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        let spec = Spec {
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or(&id).to_string(),
            steam_appids: strs(v, "steam_appids"),
            exe: strs(v, "exe"),
            url,
            win: strs(v, "win"),
            pick_newest: v.get("pick").and_then(|x| x.as_str()) == Some("newest"),
            depth: v.get("depth").and_then(|x| x.as_u64()).unwrap_or(0).min(2) as u8,
            files: strs(v, "files"),
            first: strs(v, "first"),
            newest_only: v.get("send").and_then(|x| x.as_str()) == Some("newest"),
            max_bytes: v.get("max_kb").and_then(|x| x.as_u64()).unwrap_or(1024) * 1024,
            skip_empty: v.get("skip_empty").and_then(|x| x.as_bool()).unwrap_or(true),
            part_of: v.get("part_of").and_then(|x| x.as_str()).filter(|p| !p.is_empty()).map(str::to_string),
            part_label: v.get("part_label").and_then(|x| x.as_str()).unwrap_or("файл").to_string(),
            id,
        };
        (!spec.win.is_empty() && !spec.files.is_empty()).then_some(spec)
    }
}

/// Описания с сервера; None — ответа еще не было.
static SPECS: Mutex<Option<Vec<Spec>>> = Mutex::new(None);

/// Из ответа /api/agent/state; встроенные игры пропускаются.
pub fn set_from_state(state: &Value, builtin: &[&str]) {
    let Some(arr) = state.get("save_specs").and_then(|v| v.as_array()) else { return };
    let list: Vec<Spec> = arr
        .iter()
        .filter_map(Spec::from_value)
        .filter(|s| !builtin.contains(&s.id.as_str()))
        .collect();
    *SPECS.lock().unwrap() = Some(list);
}

pub fn all() -> Vec<Spec> {
    SPECS.lock().unwrap().clone().unwrap_or_default()
}

pub fn get(id: &str) -> Option<Spec> {
    all().into_iter().find(|s| s.id == id)
}

/// Маска имени: «*» — любые знаки, регистр не важен.
pub fn glob(pat: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some((b'*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some((c, rest)) => n.first().is_some_and(|x| x.eq_ignore_ascii_case(c)) && go(rest, &n[1..]),
        }
    }
    go(pat.as_bytes(), name.as_bytes())
}

/// %ПЕРЕМЕННАЯ% из окружения; неизвестная — None (корень пропускается).
fn expand_env(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let j = tail.find('%')?;
        out.push_str(&std::env::var(&tail[..j]).ok()?);
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Корень из описания → существующие папки (по «*» их может быть несколько).
pub fn expand_root(pattern: &str, appids: &[String], steam: Option<&Path>, app: &dyn Fn(&str) -> Option<PathBuf>) -> Vec<PathBuf> {
    let mut s = match expand_env(pattern) {
        Some(s) => s,
        None => return Vec::new(),
    };
    if s.contains("{steam}") {
        let Some(st) = steam else { return Vec::new() };
        s = s.replace("{steam}", &st.display().to_string());
    }
    if s.contains("{app}") {
        let Some(dir) = appids.first().and_then(|a| app(a)) else { return Vec::new() };
        s = s.replace("{app}", &dir.display().to_string());
    }
    let s = s.replace('/', "\\");
    let mut parts = s.split('\\').filter(|p| !p.is_empty());
    let Some(first) = parts.next() else { return Vec::new() };
    // Диск «C:» или начало UNC — как есть.
    let mut cur = vec![PathBuf::from(if first.ends_with(':') { format!("{first}\\") } else { first.to_string() })];
    for part in parts {
        let mut next = Vec::new();
        for base in cur {
            if part.contains('*') {
                if let Ok(rd) = std::fs::read_dir(&base) {
                    for e in rd.flatten() {
                        if e.path().is_dir() && glob(part, &e.file_name().to_string_lossy()) {
                            next.push(e.path());
                        }
                    }
                }
            } else {
                next.push(base.join(part));
            }
        }
        cur = next;
    }
    cur.retain(|p| p.is_dir());
    cur
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Файл к отправке: путь, отметка (путь от корня через «/»), имя, папка, время, мс.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub mark: String,
    pub name: String,
    pub folder: PathBuf,
    pub mtime: u64,
}

fn walk(spec: &Spec, root: &Path, dir: &Path, depth: u8, out: &mut Vec<Found>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            if depth > 0 {
                walk(spec, root, &p, depth - 1, out);
            }
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if !spec.files.iter().any(|f| glob(f, &name)) {
            continue;
        }
        if (spec.skip_empty && meta.len() == 0) || meta.len() > spec.max_bytes {
            continue;
        }
        let Some(mtime) = mtime_ms(&meta) else { continue };
        let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        out.push(Found { mark: rel, name, folder: dir.to_path_buf(), mtime, path: p });
    }
}

/// Все подходящие файлы из корней (или из своей папки, если она задана).
pub fn files_in(spec: &Spec, roots: &[PathBuf]) -> Vec<Found> {
    let mut per_root: Vec<Vec<Found>> = roots
        .iter()
        .map(|r| {
            let mut v = Vec::new();
            walk(spec, r, r, spec.depth, &mut v);
            v
        })
        .filter(|v| !v.is_empty())
        .collect();
    if spec.pick_newest && per_root.len() > 1 {
        per_root.sort_by_key(|v| std::cmp::Reverse(v.iter().map(|f| f.mtime).max().unwrap_or(0)));
        per_root.truncate(1);
    }
    let mut out: Vec<Found> = per_root.into_iter().flatten().collect();
    let rank = |f: &Found| spec.first.iter().position(|n| n.eq_ignore_ascii_case(&f.name)).unwrap_or(usize::MAX);
    out.sort_by(|a, b| (rank(a), a.mtime, &a.mark).cmp(&(rank(b), b.mtime, &b.mark)));
    out
}

/// Корни по умолчанию на этом ПК.
pub fn default_roots(spec: &Spec) -> Vec<PathBuf> {
    let steam = crate::scan::steam_path();
    let app = |a: &str| crate::scan::steam_app_dir(a);
    let mut out = Vec::new();
    for pat in &spec.win {
        for p in expand_root(pat, &spec.steam_appids, steam.as_deref(), &app) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Что слать: изменившиеся с прошлой отметки. У send=newest — только самый свежий файл папки,
/// и только если именно он еще не ушел: иначе после отправки свежего агент уходил бы назад по
/// истории и слал старые файлы (так 07.10 вместо скриншота меню ушли старые кадры Starfield).
pub fn changed(spec: &Spec, roots: &[PathBuf], sent: &HashMap<String, u64>) -> Vec<Found> {
    to_send(spec, roots, sent, false)
}

/// То же, а force («Отправить сейчас», только что включенная отправка) у send=newest шлет
/// самый свежий файл повторно, даже если он уже ушел.
pub fn to_send(spec: &Spec, roots: &[PathBuf], sent: &HashMap<String, u64>, force: bool) -> Vec<Found> {
    let all = files_in(spec, roots);
    if spec.newest_only {
        let best = all.into_iter().max_by_key(|f| (f.mtime, f.mark.clone()));
        return best.filter(|f| force || sent.get(&f.mark) != Some(&f.mtime)).into_iter().collect();
    }
    all.into_iter().filter(|f| sent.get(&f.mark) != Some(&f.mtime)).collect()
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
    use serde_json::json;

    fn spec() -> Spec {
        Spec::from_value(&json!({
            "id": "megabonk", "name": "Megabonk", "steam_appids": ["3405340"], "exe": ["Megabonk.exe"],
            "url": "/api/agent/megabonk-save", "win": ["%BB_SPEC_TEST%\\CloudDir"], "depth": 1,
            "files": ["progression.json", "stats.json"], "first": ["progression.json"], "max_kb": 256,
        }))
        .unwrap()
    }

    #[test]
    fn parses_and_rejects() {
        let s = spec();
        assert_eq!(s.depth, 1);
        assert!(s.skip_empty && !s.newest_only && !s.pick_newest);
        assert!(Spec::from_value(&json!({"id": "x", "url": "https://evil/x", "win": ["c:\\"], "files": ["*"]})).is_none());
        assert!(Spec::from_value(&json!({"id": "../x", "url": "/api/agent/x", "win": ["c:\\"], "files": ["*"]})).is_none());
        assert!(Spec::from_value(&json!({"id": "x", "url": "/api/agent/x", "win": [], "files": ["*"]})).is_none());
    }

    #[test]
    fn glob_matches() {
        assert!(glob("*.d2s", "Hero.D2S"));
        assert!(glob("rep*persistentgamedata?.dat".replace('?', "*").as_str(), "rep+persistentgamedata1.dat"));
        assert!(!glob("*.d2s", "hero.d2s.bak"));
        assert!(glob("progression.json", "Progression.json"));
    }

    #[test]
    fn finds_changed_files_across_accounts() {
        let base = std::env::temp_dir().join(format!("bb-spec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let cloud = base.join("CloudDir");
        let (a, b) = (cloud.join("76561197980994168"), cloud.join("76561197960265729"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("progression.json"), b"x").unwrap();
        std::fs::write(a.join("stats.json"), b"s").unwrap();
        std::fs::write(a.join("controller_config.json"), b"x").unwrap();
        std::fs::write(b.join("progression.json"), b"xy").unwrap();
        std::fs::write(b.join("stats.json"), b"").unwrap(); // пустой из облака не шлем
        std::fs::write(cloud.join("steam_autocloud.vdf"), b"x").unwrap();
        std::env::set_var("BB_SPEC_TEST", &base);
        let s = spec();
        let roots = default_roots(&s);
        assert_eq!(roots, vec![cloud.clone()]);
        let first = changed(&s, &roots, &HashMap::new());
        assert_eq!(first.len(), 3);
        assert_eq!(first[2].name, "stats.json");
        assert_eq!(first[0].folder.parent(), Some(cloud.as_path()));
        assert!(first.iter().any(|f| f.mark == "76561197980994168/progression.json"));
        let mut sent: HashMap<String, u64> = first.iter().map(|f| (f.mark.clone(), f.mtime)).collect();
        assert!(changed(&s, &roots, &sent).is_empty());
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(b.join("progression.json"), b"xyz").unwrap();
        let again = changed(&s, &roots, &sent);
        assert_eq!(again.len(), 1);
        sent.insert(again[0].mark.clone(), again[0].mtime);
        let mark = base.join("megabonk.json");
        save_sent(&mark, &sent);
        assert_eq!(load_sent(&mark), sent);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn newest_never_walks_back_to_older_files() {
        let base = std::env::temp_dir().join(format!("bb-specs-newest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("old.jpg"), b"1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(base.join("new.jpg"), b"2").unwrap();
        let s = Spec::from_value(&json!({
            "id": "t", "name": "T", "url": "/api/agent/t-save", "win": ["c:/x"], "files": ["*.jpg"], "send": "newest",
        }))
        .unwrap();
        let roots = vec![base.clone()];
        let first = changed(&s, &roots, &HashMap::new());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "new.jpg");
        let sent: HashMap<String, u64> = first.iter().map(|f| (f.mark.clone(), f.mtime)).collect();
        // Свежий ушел: старый не шлется, хотя его отметки нет.
        assert!(changed(&s, &roots, &sent).is_empty());
        // «Отправить сейчас»: снова самый свежий, а не старый.
        let again = to_send(&s, &roots, &sent, true);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].name, "new.jpg");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn star_in_root_and_newest() {
        let base = std::env::temp_dir().join(format!("bb-spec2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for acc in ["111", "222"] {
            std::fs::create_dir_all(base.join("userdata").join(acc).join("250900").join("remote")).unwrap();
        }
        std::fs::write(base.join("userdata/111/250900/remote/rep+persistentgamedata1.dat"), b"a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(base.join("userdata/222/250900/remote/rep+persistentgamedata2.dat"), b"b").unwrap();
        std::fs::write(base.join("userdata/222/250900/remote/rep+persistentgamedata1.dat"), b"c").unwrap();
        let s = Spec::from_value(&json!({
            "id": "isaac", "url": "/api/agent/isaac-save", "win": ["{steam}\\userdata\\*\\250900\\remote"],
            "pick": "newest", "files": ["rep*persistentgamedata*.dat"], "send": "changed",
        }))
        .unwrap();
        let roots = expand_root(&s.win[0], &[], Some(&base), &|_| None);
        assert_eq!(roots.len(), 2);
        let found = changed(&s, &roots, &HashMap::new());
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.folder.ends_with("222\\250900\\remote") || f.folder.ends_with("222/250900/remote")));
        let newest = Spec { newest_only: true, ..s };
        assert_eq!(changed(&newest, &roots, &HashMap::new()).len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }
}
