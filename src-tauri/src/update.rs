//! Самообновление BigBacklog Companion через наш сервер (app/routers/companion.py).
//!
//! Через минуту после старта, раз в EVERY и после каждой игровой сессии (check_soon) спрашиваем
//! /api/agent/app/latest. Если там версия новее своей, качаем exe рядом с собой
//! (bigbacklog-agent.new.exe) и сверяем sha256. Меняемся, только когда тихо (quiet): игра не идет,
//! сессии отправлены, очередь плашек пуста и после последней прошло QUIET_AFTER — иначе перезапуск
//! съел бы итог после игры, он живет только в памяти. Сама замена: Windows дает переименовать запущенный exe, поэтому свой файл уходит в
//! bigbacklog-agent.old.exe (откат руками), новый встает на его место, запускается с --wait-pid
//! (ждет, пока этот процесс выйдет, иначе single-instance отдал бы управление старому) и мы выходим.
//! Путь автозапуска Windows не меняется: имя файла то же.
//!
//! Новая версия получает еще --updated-from <старая> и показывает плашку «Приложение обновлено».
//! Пункт трея «Проверить обновления» проверяет сразу и отвечает плашкой (check_manual).
//! Dev-сборка (crate::DEV) берет выпуски из своего канала ?channel=dev: его сервер отдает только
//! владельцу, и публичный выпуск ее не заменит.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::Shared;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const QUERY: &str = if crate::DEV { "?channel=dev" } else { "" };
const FIRST: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(6 * 3600);
/// Пока не тихо, проверяем снова вот так часто.
const WAIT_GAME: Duration = Duration::from_secs(10);
/// После последней плашки — столько тишины до замены (опыт иногда приходит следующим опросом).
const QUIET_AFTER_MS: i64 = 30_000;
/// Просьба проверить вне расписания (вышли из игры); check_loop смотрит раз в TICK.
static CHECK_SOON: AtomicBool = AtomicBool::new(false);
const TICK: Duration = Duration::from_secs(15);
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Проверка уже идет (плановая или по кнопке), в том числе ждет конца игры.
static CHECKING: AtomicBool = AtomicBool::new(false);

/// Чем кончилась проверка, если не ошибкой и не заменой себя.
enum Outcome {
    NoToken,
    Latest,
    /// Новая версия запущена, этот процесс выходит: плашку покажет она.
    Restarting,
}

/// «0.10.2» новее «0.9.7»: сравнение по числам через точку.
pub fn newer(remote: &str, local: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (a, b) = (parse(remote), parse(local));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn get(server: &str, token: &str, path: &str) -> Result<ureq::Response, String> {
    crate::tls::agent()
        .timeout(Duration::from_secs(300))
        .build()
        .get(&format!("{server}{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", crate::USER_AGENT)
        .call()
        .map_err(|e| e.to_string())
}

/// Скачать и проверить новую версию; Ok(путь к файлу) или причина.
fn fetch(server: &str, token: &str, info: &Value, dir: &Path) -> Result<PathBuf, String> {
    let want = info["sha256"].as_str().unwrap_or_default().to_ascii_lowercase();
    let path = dir.join("bigbacklog-agent.new.exe");
    if let Ok(data) = std::fs::read(&path) {
        if crate::openmw::sha_hex(&data) == want {
            return Ok(path); // уже скачано прошлым заходом
        }
    }
    let resp = get(server, token, &format!("/api/agent/app/file{QUERY}"))?;
    let mut data = Vec::new();
    use std::io::Read;
    resp.into_reader().take(MAX_BYTES + 1).read_to_end(&mut data).map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_BYTES || crate::openmw::sha_hex(&data) != want {
        return Err("файл обновления пришел битым".into());
    }
    std::fs::write(&path, &data).map_err(|e| e.to_string())?;
    Ok(path)
}

fn game_running(s: &Shared) -> bool {
    !s.tracker.lock().unwrap().live.is_empty()
}

/// Можно меняться: игра не идет, сыгранное отправлено, плашек нет ни в очереди, ни на экране, и после
/// последней прошло QUIET_AFTER_MS.
fn quiet(s: &Shared) -> bool {
    let t = s.tracker.lock().unwrap();
    if !t.live.is_empty() || !t.finished.is_empty() {
        return false;
    }
    drop(t);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    s.pending.lock().unwrap().is_empty()
        && !s.busy.load(Ordering::SeqCst)
        && now - s.last_toast_ms.load(Ordering::SeqCst) >= QUIET_AFTER_MS
}

/// Вышли из игры: проверить обновления в ближайший TICK.
pub fn check_soon() {
    CHECK_SOON.store(true, Ordering::SeqCst);
}

/// Встать новой версией: свой exe в .old, новый на его место, запуск нового, выход.
fn swap(s: &Shared, new: &Path) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = me.with_file_name("bigbacklog-agent.old.exe");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&me, &old).map_err(|e| format!("не переименовать себя: {e}"))?;
    if let Err(e) = std::fs::rename(new, &me) {
        let _ = std::fs::rename(&old, &me);
        return Err(format!("не поставить новую версию: {e}"));
    }
    let started = std::process::Command::new(&me)
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .arg("--updated-from")
        .arg(VERSION)
        .spawn();
    if let Err(e) = started {
        // Не запустилась — возвращаем себя на место, новая подождет следующей попытки.
        let _ = std::fs::rename(&me, new);
        let _ = std::fs::rename(&old, &me);
        return Err(format!("не запустить новую версию: {e}"));
    }
    crate::log("обновление: новая версия запущена, выхожу");
    match s.app.lock().unwrap().clone() {
        Some(app) => app.exit(0),
        None => std::process::exit(0),
    }
    Ok(())
}

pub fn check_loop(s: Arc<Shared>) {
    std::thread::sleep(FIRST);
    loop {
        if !CHECKING.swap(true, Ordering::SeqCst) {
            if let Err(e) = check_once(&s, false) {
                crate::log(&format!("обновление: {e}"));
            }
            CHECKING.store(false, Ordering::SeqCst);
        }
        // До следующей плановой проверки, или раньше — по check_soon.
        let mut waited = Duration::ZERO;
        while waited < EVERY && !CHECK_SOON.swap(false, Ordering::SeqCst) {
            std::thread::sleep(TICK);
            waited += TICK;
        }
    }
}

/// Кнопка «Проверить обновления» в трее: проверить сейчас и ответить плашкой. Нашлась новая
/// версия и игры нет — приложение сразу встает новой, плашку «обновлено» покажет уже она.
pub fn check_manual(s: Arc<Shared>) {
    let name = format!("BigBacklog Companion {VERSION}");
    if CHECKING.swap(true, Ordering::SeqCst) {
        crate::push_notice(&s, "Проверка обновлений", &name, "уже идет");
        return;
    }
    let res = check_once(&s, true);
    CHECKING.store(false, Ordering::SeqCst);
    match res {
        Ok(Outcome::Latest) => crate::push_notice(&s, "Обновлений нет", &name, "это последняя версия"),
        Ok(Outcome::Restarting) => {}
        Ok(Outcome::NoToken) => crate::push_notice(&s, "Обновления не проверить", &name, "сначала подключите приложение"),
        Err(e) => {
            crate::log(&format!("обновление: {e}"));
            crate::push_notice(&s, "Обновления не проверить", &name, "нет связи с сервером, попробуйте позже");
        }
    }
}

fn check_once(s: &Shared, manual: bool) -> Result<Outcome, String> {
    let cfg = s.cfg.lock().unwrap().clone();
    let token = cfg.token.trim().to_string();
    if token.is_empty() {
        return Ok(Outcome::NoToken);
    }
    let server = crate::server_of(&cfg);
    let info: Value = get(&server, &token, &format!("/api/agent/app/latest{QUERY}"))?.into_json().map_err(|e| e.to_string())?;
    let remote = info["version"].as_str().unwrap_or_default();
    if !info["available"].as_bool().unwrap_or(false) || !newer(remote, VERSION) {
        return Ok(Outcome::Latest);
    }
    let dir = std::env::current_exe().map_err(|e| e.to_string())?.parent().map(Path::to_path_buf).unwrap_or_default();
    let new = fetch(&server, &token, &info, &dir)?;
    crate::log(&format!("обновление: скачана версия {remote}, жду, когда не будет игры"));
    if manual && game_running(s) {
        crate::push_notice(s, "Нашлось обновление", &format!("BigBacklog Companion {remote}"),
                           "встанет само, когда закончите играть");
    }
    while !quiet(s) {
        std::thread::sleep(WAIT_GAME);
    }
    swap(s, &new)?;
    Ok(Outcome::Restarting)
}

/// Запуск после самообновления: с какой версии обновились (--updated-from), иначе None.
pub fn updated_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == "--updated-from")?;
    args.get(i + 1).filter(|v| !v.is_empty() && v.len() < 20).cloned()
}

/// Запуск новой версией: подождать, пока старый процесс выйдет (до 30 с).
pub fn wait_pid_from_args() {
    let args: Vec<String> = std::env::args().collect();
    let Some(i) = args.iter().position(|a| a == "--wait-pid") else { return };
    let Some(pid) = args.get(i + 1).and_then(|p| p.parse::<u32>().ok()) else { return };
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 30_000);
            CloseHandle(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99.99"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
        assert!(!newer("", "0.1.0"));
    }
}
