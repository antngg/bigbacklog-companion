//! Только dev-сборка владельца: схемы достижений игр из Steam-клиента на сайт.
//!
//! Steam-клиент держит полную схему каждой открытой игры в
//! appcache\stats\UserGameStatsSchema_<appid>.bin: все языки, тексты скрытых
//! достижений и достижения свежего обновления, которых Web API еще не отдает
//! (Call of Duty®: в клиенте 175, на сайте 157). Файл общий для всех игроков игры,
//! личных данных в нем нет (личный прогресс — соседние UserGameStats_<accountid>_*,
//! их не трогаем). Сервер (/api/agent/steam-schema) доливает из него описания
//! и недостающие достижения. Отметка об ушедших файлах — steamschema.json рядом
//! с config.json: файл уходит снова, только когда Steam его переписал.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const PREFIX: &str = "UserGameStatsSchema_";
/// Как часто смотреть папку: схемы меняются, когда открываешь достижения игры в
/// клиенте или игра обновилась.
pub const EVERY: Duration = Duration::from_secs(30 * 60);
/// Файлов за один заход: в первый раз их сотни, отправка растягивается на несколько
/// заходов, а не идет залпом.
pub const PER_RUN: usize = 40;

/// Схемы в папке статистики Steam: (путь, appid, время изменения).
pub fn files(dir: &Path) -> Vec<(PathBuf, String, u64)> {
    let mut out: Vec<(PathBuf, String, u64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let appid = name.strip_prefix(PREFIX)?.strip_suffix(".bin")?.to_string();
            if appid.is_empty() || !appid.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
            Some((e.path(), appid, mtime))
        })
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2)); // свежие первыми
    out
}

pub fn changed(dir: &Path, sent: &HashMap<String, u64>) -> Vec<(PathBuf, String, u64)> {
    files(dir).into_iter().filter(|(_, appid, mtime)| sent.get(appid) != Some(mtime)).collect()
}

pub fn load_sent(path: &Path) -> HashMap<String, u64> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_sent(path: &Path, sent: &HashMap<String, u64>) {
    if let Ok(text) = serde_json::to_string(sent) {
        let _ = std::fs::write(path, text);
    }
}

/// Один заход отправки — строка истории в подменю трея (steamschema-log.json рядом с config.json).
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Run {
    /// Unix-секунды конца захода.
    pub at: i64,
    /// Ушло файлов.
    pub files: usize,
    pub added: i64,
    pub filled: i64,
    /// «Отправить сейчас» из трея, а не по таймеру.
    #[serde(default)]
    pub manual: bool,
    /// Заход прервался (нет связи и т. п.).
    #[serde(default)]
    pub error: Option<String>,
}

/// Сколько заходов помнить и показывать.
pub const HISTORY: usize = 8;

pub fn load_runs(path: &Path) -> Vec<Run> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Новый заход в начало истории, лишнее с конца — прочь.
pub fn push_run(path: &Path, run: Run) -> Vec<Run> {
    let mut runs = load_runs(path);
    runs.insert(0, run);
    runs.truncate(HISTORY);
    if let Ok(text) = serde_json::to_string(&runs) {
        let _ = std::fs::write(path, text);
    }
    runs
}

/// Что показать после даты: «вручную · схем 3 · достижений +18 · описаний 5».
pub fn run_summary(r: &Run) -> String {
    let mut parts: Vec<String> = Vec::new();
    if r.manual {
        parts.push("вручную".into());
    }
    if r.files == 0 && r.error.is_none() {
        parts.push("нового нет".into());
    } else {
        parts.push(format!("схем {}", r.files));
    }
    if r.added > 0 {
        parts.push(format!("достижений +{}", r.added));
    }
    if r.filled > 0 {
        parts.push(format!("описаний {}", r.filled));
    }
    if let Some(e) = &r.error {
        parts.push(e.clone());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_schemas_only_and_skips_sent() {
        let dir = std::env::temp_dir().join(format!("bb-steamschema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["UserGameStatsSchema_1938090.bin", "UserGameStats_20728440_1938090.bin",
                     "UserGameStatsSchema_x.bin", "UserGameStatsSchema_42.tmp"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let found = files(&dir);
        assert_eq!(found.iter().map(|f| f.1.as_str()).collect::<Vec<_>>(), vec!["1938090"]);
        let mut sent = HashMap::new();
        sent.insert("1938090".to_string(), found[0].2);
        assert!(changed(&dir, &sent).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_keeps_newest_and_reads_short() {
        let path = std::env::temp_dir().join(format!("bb-schemalog-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        for i in 0..(HISTORY + 2) {
            push_run(&path, Run { at: i as i64, files: i, ..Default::default() });
        }
        let runs = load_runs(&path);
        assert_eq!(runs.len(), HISTORY);
        assert_eq!(runs[0].at, (HISTORY + 1) as i64);
        let _ = std::fs::remove_file(&path);
        assert_eq!(run_summary(&Run { files: 0, manual: true, ..Default::default() }), "вручную · нового нет");
        assert_eq!(run_summary(&Run { files: 3, added: 18, filled: 5, ..Default::default() }),
                   "схем 3 · достижений +18 · описаний 5");
        assert_eq!(run_summary(&Run { files: 1, error: Some("нет связи".into()), ..Default::default() }),
                   "схем 1 · нет связи");
    }
}
