//! Входящие от игровых модов: %APPDATA%\BigBacklogAgent\inbox.
//!
//! Мод (пока Sacred Gold) кладёт туда JSON-файлы атомарно: пишет name.tmp и
//! переименовывает в *.json. Два вида: событие на файл
//! (`sacred_gold.<code>.<unix>.json`, event "unlock") и один перезаписываемый
//! снимок (`sacred_gold.progress.json`, event "progress"). Агент пачками до
//! MAX_BATCH шлёт их на POST /api/agent/custom-achievements и удаляет
//! отправленное; снимок прогресса — только если мод не перезаписал его, пока
//! шла отправка. Папка всегда в %APPDATA%, даже в портативном режиме: мод не
//! знает, где лежит exe агента.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Предел сервера на один запрос.
pub const MAX_BATCH: usize = 200;
/// Неразборчивый файл старше суток — мусор (недописанный или битый), удаляем.
const BROKEN_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

pub fn dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("BigBacklogAgent").join("inbox")
}

/// Подходит ли имя: только готовые *.json (временные *.tmp мод ещё пишет).
pub fn is_event_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".json") && !lower.starts_with('.')
}

/// Перезаписываемый снимок — удалять только если он не менялся.
pub fn is_snapshot(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".progress.json")
}

/// Дешёвая проверка «есть ли что слать» — для такта агента.
pub fn has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).any(|e| is_event_file(&e.file_name().to_string_lossy())))
        .unwrap_or(false)
}

/// Имена подходящих файлов по порядку (события одной игры — по времени в имени).
pub fn pick(mut names: Vec<String>) -> Vec<String> {
    names.retain(|n| is_event_file(n));
    names.sort();
    names
}

pub struct Item {
    pub path: PathBuf,
    pub event: Value,
    /// Время изменения на момент чтения — для снимка прогресса.
    pub mtime: Option<SystemTime>,
    pub snapshot: bool,
}

/// До `limit` разобранных событий. Неразборчивые свежие файлы пропускаем (мод
/// мог ещё не дописать), старше суток — удаляем.
pub fn collect(dir: &Path, limit: usize, now: SystemTime) -> Vec<Item> {
    let names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for name in pick(names) {
        if out.len() >= limit {
            break;
        }
        let path = dir.join(&name);
        // Время — до чтения: перезапись во время чтения сменит его, и снимок
        // останется до следующего раза.
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(|v| v.is_object());
        match parsed {
            Some(event) => out.push(Item { path, event, mtime, snapshot: is_snapshot(&name) }),
            None => {
                let old = mtime.and_then(|m| now.duration_since(m).ok()).map(|age| age > BROKEN_MAX_AGE).unwrap_or(false);
                if old {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    out
}

/// Убрать отправленное. Снимок прогресса — только если мод его не перезаписал.
pub fn remove_sent(items: &[Item]) {
    for it in items {
        if it.snapshot {
            let now = std::fs::metadata(&it.path).and_then(|m| m.modified()).ok();
            if now.is_none() || now != it.mtime {
                continue;
            }
        }
        let _ = std::fs::remove_file(&it.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_json_sorted_and_skips_tmp() {
        let names = vec![
            "sacred_gold.progress.json".to_string(),
            "sacred_gold.level_10.1791208152.json.tmp".to_string(),
            "sacred_gold.level_10.1791208152.json".to_string(),
            "sacred_gold.boss_1.1791208100.tmp".to_string(),
            "sacred_gold.boss_1.1791208100.json".to_string(),
            "readme.txt".to_string(),
        ];
        assert_eq!(
            pick(names),
            vec![
                "sacred_gold.boss_1.1791208100.json".to_string(),
                "sacred_gold.level_10.1791208152.json".to_string(),
                "sacred_gold.progress.json".to_string(),
            ]
        );
        assert!(is_snapshot("sacred_gold.progress.json"));
        assert!(!is_snapshot("sacred_gold.level_10.1791208152.json"));
    }

    #[test]
    fn collects_parses_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("bb-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.unlock.1.json"), br#"{"event":"unlock","code":"a"}"#).unwrap();
        std::fs::write(dir.join("b.broken.2.json"), b"{not json").unwrap();
        std::fs::write(dir.join("c.half.3.json.tmp"), b"{").unwrap();
        std::fs::write(dir.join("sacred_gold.progress.json"), br#"{"event":"progress"}"#).unwrap();
        assert!(has_files(&dir));

        // Свежий битый файл остаётся, лимит считает только разобранные.
        let items = collect(&dir, 1, SystemTime::now());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event["code"], "a");
        let items = collect(&dir, MAX_BATCH, SystemTime::now());
        assert_eq!(items.len(), 2);
        assert!(items[1].snapshot);
        assert!(dir.join("b.broken.2.json").exists());

        // Снимок перезаписан после чтения — не удаляется, событие — удаляется.
        let mut items = items;
        items[1].mtime = Some(SystemTime::UNIX_EPOCH);
        remove_sent(&items);
        assert!(!dir.join("a.unlock.1.json").exists());
        assert!(dir.join("sacred_gold.progress.json").exists());

        // Через двое суток битый файл убирается; .tmp не трогаем.
        let later = SystemTime::now() + Duration::from_secs(2 * 24 * 3600);
        let items = collect(&dir, MAX_BATCH, later);
        assert_eq!(items.len(), 1);
        assert!(!dir.join("b.broken.2.json").exists());
        assert!(dir.join("c.half.3.json.tmp").exists());

        // Нетронутый снимок удаляется.
        remove_sent(&items);
        assert!(!dir.join("sacred_gold.progress.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
