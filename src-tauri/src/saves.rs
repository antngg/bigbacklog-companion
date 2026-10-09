//! Сохранения игр: какие игры агент шлёт на сайт и откуда.
//!
//! По умолчанию — никакие: человек включает отправку по игре сам (кнопка на
//! вкладке «Прогресс» сайта или переключатель в окне «Сохранения игр»). Флаги
//! живут на сервере (app/save_sync.py) и приходят в /api/agent/state; пока
//! первого ответа не было, ничего не уходит.
//!
//! Папка — та, где игра хранит сохранения по умолчанию (d2r.rs, sacred.rs,
//! w3.rs), или своя из окна (config.json, save_dirs) для нестандартной установки.
//!
//! GAMES — игры, которые агент знает сам. Остальные приходят описанием с сервера
//! (specs.rs, save_specs в /api/agent/state) и идут тем же окном и флагами.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

/// id игры (как на сервере) и её название в окне.
pub const GAMES: [(&str, &str); 8] = [
    ("w3", "The Witcher 3"),
    ("d2r", "Diablo II: Resurrected"),
    ("sacred", "Sacred Gold"),
    ("isaac", "The Binding of Isaac"),
    ("s2", "Sacred 2 Remaster"),
    ("ror2", "Risk of Rain 2"),
    ("cp77", "Cyberpunk 2077"),
    ("omw", "OpenMW (Morrowind)"),
];

/// id встроенных игр: их описания с сервера агент не берет.
pub fn builtin_ids() -> Vec<&'static str> {
    GAMES.iter().map(|(id, _)| *id).collect()
}

/// Все игры окна: встроенные и из описаний сервера.
pub fn games() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = GAMES.iter().map(|(id, n)| (id.to_string(), n.to_string())).collect();
    out.extend(crate::specs::all().into_iter().map(|s| (s.id, s.name)));
    out
}

/// Флаги с сервера; None — ещё не было ответа.
static FLAGS: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
/// Свои папки из config.json (save_dirs).
static DIRS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
/// «Отправить сейчас» и только что включённая отправка — для track_loop.
static NOW_W3: AtomicBool = AtomicBool::new(false);
static NOW_D2R: AtomicBool = AtomicBool::new(false);
static NOW_SACRED: AtomicBool = AtomicBool::new(false);
static NOW_ISAAC: AtomicBool = AtomicBool::new(false);
static NOW_S2: AtomicBool = AtomicBool::new(false);
static NOW_ROR2: AtomicBool = AtomicBool::new(false);
static NOW_CP77: AtomicBool = AtomicBool::new(false);
static NOW_OMW: AtomicBool = AtomicBool::new(false);
/// То же для игр из описаний сервера.
static NOW_SPEC: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn now_flag(id: &str) -> Option<&'static AtomicBool> {
    match id {
        "w3" => Some(&NOW_W3),
        "d2r" => Some(&NOW_D2R),
        "sacred" => Some(&NOW_SACRED),
        "isaac" => Some(&NOW_ISAAC),
        "s2" => Some(&NOW_S2),
        "ror2" => Some(&NOW_ROR2),
        "cp77" => Some(&NOW_CP77),
        "omw" => Some(&NOW_OMW),
        _ => None,
    }
}

/// Флаги из ответа /api/agent/state. Игра, которую только что включили, —
/// сразу «отправить сейчас».
pub fn set_flags_from_state(state: &Value) {
    crate::specs::set_from_state(state, &builtin_ids());
    let Some(obj) = state.get("save_sync").and_then(|v| v.as_object()) else { return };
    let new: HashMap<String, bool> = obj.iter().map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(false))).collect();
    let mut flags = FLAGS.lock().unwrap();
    // Первый ответ после запуска — не «включили»: флаги просто становятся известны. Иначе каждая
    // включенная игра получала «Отправить сейчас», и Starfield (шлет последнее сохранение
    // принудительно) заново отправлял то же сохранение на каждом запуске приложения.
    if let Some(old) = flags.as_ref() {
        for (id, on) in &new {
            if *on && !old.get(id).copied().unwrap_or(false) {
                request_now(id);
            }
        }
    }
    *flags = Some(new);
}

pub fn enabled(id: &str) -> bool {
    FLAGS.lock().unwrap().as_ref().and_then(|f| f.get(id)).copied().unwrap_or(false)
}

pub fn set_enabled(id: &str, on: bool) {
    let mut flags = FLAGS.lock().unwrap();
    flags.get_or_insert_with(HashMap::new).insert(id.to_string(), on);
    drop(flags);
    if on {
        request_now(id);
    }
}

pub fn request_now(id: &str) {
    if let Some(f) = now_flag(id) {
        f.store(true, Ordering::SeqCst);
    } else {
        NOW_SPEC.lock().unwrap().get_or_insert_with(HashSet::new).insert(id.to_string());
    }
    // Спутники игры (снимок героя Starfield) уходят вместе с ней.
    let parts: Vec<String> = crate::specs::all().into_iter()
        .filter(|s| s.part_of.as_deref() == Some(id))
        .map(|s| s.id)
        .collect();
    if !parts.is_empty() {
        NOW_SPEC.lock().unwrap().get_or_insert_with(HashSet::new).extend(parts);
    }
}

/// Просили отправить сейчас (флаг снимается).
pub fn take_now(id: &str) -> bool {
    match now_flag(id) {
        Some(f) => f.swap(false, Ordering::SeqCst),
        None => NOW_SPEC.lock().unwrap().as_mut().map(|s| s.remove(id)).unwrap_or(false),
    }
}

pub fn set_dirs(dirs: &HashMap<String, String>) {
    *DIRS.lock().unwrap() = Some(dirs.clone());
}

fn custom_dir(id: &str) -> Option<PathBuf> {
    let dirs = DIRS.lock().unwrap();
    let p = dirs.as_ref()?.get(id)?.trim().to_string();
    if p.is_empty() {
        return None;
    }
    let p = PathBuf::from(p);
    // Ведьмак: можно указать и «The Witcher 3», и саму gamesaves внутри неё.
    if id == "w3" && p.join("gamesaves").is_dir() {
        return Some(p.join("gamesaves"));
    }
    Some(p)
}

/// Папка по умолчанию — там, где игра пишет сохранения сама.
pub fn default_dir(id: &str) -> Option<PathBuf> {
    match id {
        "w3" => crate::w3::game_dir().map(|d| d.join("gamesaves")),
        "d2r" => crate::d2r::saves_dir(),
        "sacred" => crate::sacred::saves_dir(),
        "isaac" => crate::isaac::saves_dir(),
        "s2" => crate::s2::saves_dir(),
        "ror2" => crate::ror2::saves_dir(),
        "cp77" => crate::cp77::saves_dir(),
        "omw" => crate::openmw::default_saves_dir(),
        _ => crate::specs::get(id).and_then(|s| crate::specs::default_roots(&s).into_iter().next()),
    }
}

/// Корни поиска у игры из описания: своя папка из окна или все по умолчанию.
pub fn spec_roots(spec: &crate::specs::Spec) -> Vec<PathBuf> {
    match custom_dir(&spec.id).filter(|p| p.is_dir()) {
        Some(p) => vec![p],
        None => crate::specs::default_roots(spec),
    }
}

/// Папка, откуда агент берёт сохранения: своя (если указана и есть) или по умолчанию.
pub fn dir(id: &str) -> Option<PathBuf> {
    custom_dir(id).filter(|p| p.is_dir()).or_else(|| default_dir(id))
}

/// Что уже ушло на сайт, по отметкам рядом с config.json: [(файл, время файла в мс,
/// подпись)], свежее первым. D2R шлёт каждого офлайн-персонажа (.d2s) и общий тайник
/// (.d2i), Ведьмак и Sacred — последнее сохранение.
fn sent_files(config_dir: &std::path::Path, id: &str) -> Vec<(String, u64, &'static str)> {
    let read = |name: &str| -> Option<Value> {
        std::fs::read_to_string(config_dir.join(name)).ok().and_then(|s| serde_json::from_str(&s).ok())
    };
    let mut out = Vec::new();
    // OpenMW: синк в обе стороны, отметки по файлам — в openmw.json (marks).
    if id == "omw" {
        let marks = read("openmw.json").and_then(|v| v.get("marks").and_then(|m| m.as_object()).cloned());
        for (k, m) in marks.unwrap_or_default() {
            let Some(t) = m.get("mtime").and_then(|t| t.as_u64()) else { continue };
            out.push((k, t * 1000, "сохранение"));
        }
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        return out;
    }
    let spec_file = format!("{id}.json");
    let file = match id {
        "w3" => "w3.json",
        "sacred" => "sacred.json",
        "isaac" => "isaac.json",
        "s2" => "s2.json",
        "ror2" => "ror2.json",
        "cp77" => "cp77.json",
        "d2r" => "d2r.json",
        _ if crate::specs::get(id).is_some() => spec_file.as_str(),
        _ => return out,
    };
    let Some(v) = read(file) else { return out };
    // Два вида отметок: {name, mtime(мс), ...} — одно последнее сохранение, или
    // {имя файла: время} — по каждому файлу (D2R в секундах, Sacred по героям в мс).
    if let (Some(n), Some(t)) = (v.get("name").and_then(|n| n.as_str()), v.get("mtime").and_then(|t| t.as_u64())) {
        out.push((n.to_string(), t, "сохранение"));
    } else if let Some(obj) = v.as_object() {
        for (k, t) in obj {
            let t = t.as_u64().or_else(|| t.get("mtime").and_then(|m| m.as_u64()));
            let Some(t) = t else { continue };
            let ms = if t < 100_000_000_000 { t * 1000 } else { t };
            let low = k.to_ascii_lowercase();
            let kind = if low.ends_with(".d2i") {
                "общий тайник"
            } else if low.ends_with(".d2s") || low.ends_with(".sacred2save") {
                "персонаж"
            } else if low.ends_with(".sacred2stats") {
                "сводка героя"
            } else if low.ends_with(".sacred2chest") {
                "общий сундук"
            } else if low.starts_with("runs/") {
                "отчет о забеге"
            } else if low.ends_with(".xml") {
                "профиль"
            } else {
                "сохранение"
            };
            out.push((k.strip_prefix("runs/").unwrap_or(k).to_string(), ms, kind));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// id игры в Steam: по нему окно показывает иконку игры. У Ведьмака два издания.
fn steam_appids(id: &str) -> Vec<String> {
    let fixed: &[&str] = match id {
        "w3" => &[crate::w3::STEAM_APPID, crate::w3::STEAM_APPID_GOTY],
        "d2r" => &[crate::d2r::STEAM_APPID],
        "sacred" => &[crate::sacred::STEAM_APPID],
        "isaac" => &[crate::isaac::STEAM_APPID],
        "s2" => &[crate::s2::STEAM_APPID],
        "ror2" => &[crate::ror2::STEAM_APPID],
        "cp77" => &[crate::cp77::STEAM_APPID],
        "omw" => &[crate::openmw::STEAM_APPID],
        _ => return crate::specs::get(id).map(|s| s.steam_appids).unwrap_or_default(),
    };
    fixed.iter().map(|s| s.to_string()).collect()
}

/// Иконки игр (data:-адрес) по id, один раз за запуск: окно перерисовывается раз в 5 с.
static ICONS: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

/// Иконка игры 32×32 из кэша библиотеки Steam (appcache\librarycache\<appid>\<хэш>.jpg),
/// без похода в сеть. Нет Steam или игры в кэше — None, окно рисует букву.
fn icon(id: &str) -> Option<String> {
    let mut guard = ICONS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(v) = map.get(id) {
        return v.clone();
    }
    let found = steam_appids(id).iter().find_map(|appid| {
        let dir = crate::scan::steam_path()?.join("appcache").join("librarycache").join(appid);
        std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).find_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let stem = name.strip_suffix(".jpg")?;
            if stem.len() != 40 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let data = std::fs::read(e.path()).ok().filter(|d| d.len() < 64 * 1024)?;
            Some(format!("data:image/jpeg;base64,{}", base64(&data)))
        })
    });
    map.insert(id.to_string(), found.clone());
    found
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Список для окна «Сохранения игр».
pub fn list(config_dir: &std::path::Path) -> Value {
    let known = FLAGS.lock().unwrap().is_some();
    let parts: Vec<crate::specs::Spec> = crate::specs::all().into_iter().filter(|s| s.part_of.is_some()).collect();
    let games: Vec<Value> = games()
        .iter()
        .filter(|(id, _)| !parts.iter().any(|p| &p.id == id))
        .map(|(id, name)| {
            let d = dir(id);
            let mut sent: Vec<(String, u64, String)> = sent_files(config_dir, id)
                .into_iter()
                .map(|(f, t, kind)| (f, t, kind.to_string()))
                .collect();
            for p in parts.iter().filter(|p| p.part_of.as_deref() == Some(id.as_str())) {
                sent.extend(sent_files(config_dir, &p.id).into_iter().map(|(f, t, _)| (f, t, p.part_label.clone())));
            }
            sent.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let sent: Vec<Value> = sent
                .into_iter()
                .map(|(f, t, kind)| json!({ "file": f, "mtime": t, "kind": kind }))
                .collect();
            json!({
                "id": id,
                "name": name,
                "enabled": enabled(id),
                "dir": d.as_ref().map(|p| p.display().to_string()),
                "found": d.as_ref().map(|p| p.is_dir()).unwrap_or(false),
                "custom": custom_dir(id).map(|p| p.display().to_string()),
                "default": default_dir(id).map(|p| p.display().to_string()),
                "sent": sent,
                "icon": icon(id),
            })
        })
        .collect();
    json!({ "known": known, "games": games })
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn base64_matches_standard() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
