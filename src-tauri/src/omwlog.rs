//! Свои достижения Big Backlog в OpenMW (мод mods/BigBacklogAchievements у
//! игры): Lua в OpenMW не может писать файлы, поэтому мод печатает строки
//! «[BBACH] {json}» в «Документы\My Games\OpenMW\openmw.log».
//!
//! Агент переносит их в ящик (inbox.rs) файлами того же вида, что пишет мод
//! Sacred: открытие — файл на событие, снимок прогресса — по файлу на героя.
//! Отправляет их уже inbox_sync, в том числе позже, если сети не было: файл
//! уходит из ящика только после ответа сервера.
//!
//! Лог игра перезаписывает при каждом запуске, а мод после каждой загрузки
//! повторяет полный снимок (список открытого с временем), поэтому пропущенная
//! строка ничего не теряет, а повтор безвреден: сервер открывает один раз,
//! снимок перезаписывается.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const MARK: &str = "[BBACH] ";
const SOURCE: &str = "bb-openmw";

struct Seen {
    /// Размер и время изменения лога при прошлом чтении: не менялся — не читаем.
    stamp: Option<(u64, SystemTime)>,
    /// Что уже положено в ящик в этом запуске агента (имя + тело).
    written: HashSet<String>,
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

pub fn log_path() -> Option<PathBuf> {
    crate::openmw::documents().map(|d| d.join("My Games").join("OpenMW").join("openmw.log"))
}

/// Строки мода из текста лога, по порядку.
pub fn events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| l.find(MARK).map(|i| &l[i + MARK.len()..]))
        .filter_map(|j| serde_json::from_str::<Value>(j.trim()).ok())
        .filter(|v| v.get("source").and_then(Value::as_str) == Some(SOURCE))
        .collect()
}

/// Имя героя или код в имени файла: латиница, цифры и «_», остальное — hex байтов.
fn file_key(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b == b'_' { (b as char).to_string() } else { format!("{b:02x}") })
        .collect::<String>()
        .chars()
        .take(48)
        .collect()
}

/// Файлы для ящика: (имя, тело). Открытие получает время из unlocked_at,
/// из снимков остаётся последний у каждого героя. device — «pc».
pub fn to_inbox(events: &[Value], device: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut last: BTreeMap<String, Value> = BTreeMap::new();
    for ev in events {
        let game = ev.get("game").and_then(Value::as_str).unwrap_or("openmw_morrowind").to_string();
        let mut body = ev.clone();
        body["device"] = Value::from(device);
        match ev.get("event").and_then(Value::as_str) {
            Some("unlock") => {
                let Some(code) = ev.get("code").and_then(Value::as_str) else { continue };
                let at = ev
                    .get("unlocked_at")
                    .and_then(|m| m.get(code))
                    .and_then(Value::as_i64)
                    .or_else(|| ev.get("now").and_then(Value::as_i64))
                    .unwrap_or(0);
                body["at"] = Value::from(at);
                out.push((format!("{game}.{}.{at}.json", file_key(code)), body));
            }
            Some("progress") => {
                let hero = ev.get("character").and_then(Value::as_str).unwrap_or("").to_string();
                last.insert(format!("{game}.{}", file_key(&hero)), body);
            }
            _ => {}
        }
    }
    for (stem, body) in last {
        out.push((format!("{stem}.progress.json"), body));
    }
    out
}

/// Атомарно: сначала «имя.tmp» (ящик его пропускает), потом переименование.
fn put(dir: &Path, name: &str, body: &Value) -> bool {
    let tmp = dir.join(format!("{name}.tmp"));
    if fs::write(&tmp, body.to_string()).is_err() {
        return false;
    }
    fs::rename(&tmp, dir.join(name)).is_ok()
}

/// Новые строки лога — в ящик. Дёшево, когда лог не менялся (один stat).
pub fn pump(inbox: &Path) {
    let Some(path) = log_path() else { return };
    let Ok(meta) = fs::metadata(&path) else { return };
    let stamp = (meta.len(), meta.modified().unwrap_or(UNIX_EPOCH));
    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(|| Seen { stamp: None, written: HashSet::new() });
    if seen.stamp == Some(stamp) {
        return;
    }
    seen.stamp = Some(stamp);
    let Ok(bytes) = fs::read(&path) else { return };
    let text = String::from_utf8_lossy(&bytes);
    if !text.contains(MARK) {
        return;
    }
    let _ = fs::create_dir_all(inbox);
    for (name, body) in to_inbox(&events(&text), "pc") {
        let key = format!("{name}|{body}");
        if seen.written.contains(&key) {
            continue;
        }
        if put(inbox, &name, &body) {
            seen.written.insert(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = concat!(
        "[16:13:09.823 I] Loading cell\n",
        "[16:18:15.583 I] L@0x11[scripts/bb_achievements/player.lua]:\t[BBACH] {\"source\":\"bb-openmw\",",
        "\"game\":\"openmw_morrowind\",\"event\":\"progress\",\"character\":\"Vasyan\",\"counters\":{\"kills\":3},",
        "\"unlocked\":[\"spymaster\"],\"unlocked_at\":{\"spymaster\":1791379095},\"now\":1791379100}\n",
        "[16:19:00.000 I] L@0x11[x]:\t[BBACH] {\"source\":\"bb-openmw\",\"game\":\"openmw_morrowind\",",
        "\"event\":\"unlock\",\"code\":\"kills_10\",\"character\":\"Vasyan\",\"unlocked\":[\"spymaster\",\"kills_10\"],",
        "\"unlocked_at\":{\"kills_10\":1791379140},\"now\":1791379141}\n",
        "[16:20:00.000 I] L@0x11[x]:\t[BBACH] {\"source\":\"bb-openmw\",\"game\":\"openmw_morrowind\",",
        "\"event\":\"progress\",\"character\":\"Vasyan\",\"counters\":{\"kills\":11}}\n",
        "[16:20:01.000 I] [BBACH] {broken\n",
    );

    #[test]
    fn log_lines_become_inbox_files() {
        let files = to_inbox(&events(LOG), "pc");
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["openmw_morrowind.kills_10.1791379140.json", "openmw_morrowind.Vasyan.progress.json"]);
        assert_eq!(files[0].1["at"], 1791379140);
        assert_eq!(files[0].1["device"], "pc");
        // Из снимков героя — последний.
        assert_eq!(files[1].1["counters"]["kills"], 11);
        assert!(crate::inbox::is_snapshot(&files[1].0) && crate::inbox::is_event_file(&files[0].0));
    }

    #[test]
    fn hero_names_are_safe_in_file_names() {
        assert_eq!(file_key("Вася"), "d092d0b0d181d18f");
        assert_eq!(file_key("fa"), "fa");
    }
}
