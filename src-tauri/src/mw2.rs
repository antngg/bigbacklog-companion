//! Modern Warfare 2 (IW4x, мод Bot Warfare): прокачка между ПК, Декой и AYN Thor через сервер
//! (app/handheld_saves.py, kind=pc, family=mw2) — тем же порядком, что скрипт запуска на Деке
//! (~/bb-mw2/sync.py) и Black Ops на Thor (android/…/play/Bo1.kt).
//!
//! Прокачка — один файл «<папка игры>\players\mods\mp_bots\iw4x.stat» (8208 байт, разбор у сайта
//! app/mw2_save.py). На сервер он уходит как есть, под именем iw4x.stat, поэтому sha серверной
//! строки и sha файла здесь совпадают.
//!
//! Решение — по отметке прошлого обмена (mw2.json рядом с config.json): sha серверной строки
//! (server_sha) и sha содержимого файла здесь (content). Поменялось только на сервере — забрать
//! (запись через временный файл), только здесь — отдать (current=true), в обоих местах — свежее
//! по времени побеждает, проигравшее уходит на сервер копией (current=false). Удаление не
//! синкается: пропавший здесь файл просто ничего не отдаёт, отметка удаления на сайте его не трогает.
//!
//! Флаг — «mw2» (save_sync с сервера, окно «Сохранения игр»): выключено — ни скачивания, ни отправки.
//!
//! Когда: при старте агента, сразу после включения флага и по «Отправить сейчас», после выхода из
//! игры (забрать перед следующим запуском, отдать сыгранное) и раз в EVERY, пока игра не запущена.
//! Пока iw4x.exe или iw4mp.exe жив, файл не трогаем: игра держит прокачку в памяти и пишет сама.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::openmw::{self, Api, NetError, ServerSave};

/// Call of Duty: Modern Warfare 2 (2009) и его сетевая часть: папка игры у них общая.
pub const STEAM_APPID: &str = "10180";
pub const STEAM_APPID_MP: &str = "10190";
/// Процессы игры: клиент IW4x и родной сетевой exe.
pub const EXES: [&str; 2] = ["iw4x.exe", "iw4mp.exe"];
/// Как часто сверяться с сервером, пока игра не запущена.
pub const EVERY: Duration = Duration::from_secs(5 * 60);
const FAMILY: &str = "mw2";
const NAME: &str = "iw4x.stat";
/// Размер iw4x.stat (app/mw2_save.py FILE_SIZE): другой — не профиль, не шлём и не пишем.
const FILE_SIZE: usize = 8208;
const TIMEOUT: Duration = Duration::from_secs(30);

/// Сервер уже принимал прокачку MW2: больше не проверяем до перезапуска агента.
static KNOWN: AtomicBool = AtomicBool::new(false);
/// Обмен идёт — следующий не запускается.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Занять обмен; false — уже идёт.
pub fn try_start() -> bool {
    !BUSY.swap(true, Ordering::SeqCst)
}

pub fn finish() {
    BUSY.store(false, Ordering::SeqCst);
}

/// Игра запущена (любой из её exe).
pub fn running() -> bool {
    EXES.iter().any(|e| crate::scan::exe_running(e))
}

/// Папка игры в Steam: по любому из двух appid.
fn game_dir() -> Option<PathBuf> {
    crate::scan::steam_app_dir(STEAM_APPID).or_else(|| crate::scan::steam_app_dir(STEAM_APPID_MP))
}

/// Папка по умолчанию: «<папка игры>\players\mods\mp_bots».
pub fn default_dir() -> Option<PathBuf> {
    game_dir().map(|g| g.join("players").join("mods").join("mp_bots"))
}

/// Папка прокачки: своя из окна «Сохранения игр» или по умолчанию. Папки mp_bots ещё нет (мод
/// не запускали) — годится, если рядом стоит IW4x: создадим её при первой записи с сервера.
fn profile_dir() -> Option<PathBuf> {
    crate::saves::dir("mw2").filter(|d| {
        d.is_dir() || d.ancestors().nth(3).is_some_and(|game| game.join(EXES[0]).is_file())
    })
}

/// MW2 с IW4x на этом ПК есть: без него и с сервером не сверяемся.
pub fn present() -> bool {
    profile_dir().is_some()
}

/// mw2.json рядом с config.json. name и mtime (мс) — для окна «Сохранения игр» (saves.rs).
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct State {
    device_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(skip_serializing_if = "is_zero")]
    mtime: u64,
    /// sha серверной строки при прошлом обмене.
    server_sha: String,
    /// sha файла здесь при прошлом обмене.
    content: String,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

fn state_path() -> PathBuf {
    crate::config_path().with_file_name("mw2.json")
}

fn load_state(path: &Path) -> State {
    let mut st: State = fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if st.device_id.is_empty() {
        st.device_id = openmw::new_device_id();
    }
    st
}

fn save_state(path: &Path, st: &State) {
    let tmp = path.with_extension("json.tmp");
    let ok = serde_json::to_vec_pretty(st)
        .ok()
        .is_some_and(|b| fs::write(&tmp, b).and_then(|_| fs::rename(&tmp, path)).is_ok());
    if !ok {
        crate::log("MW2: не удалось записать mw2.json");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Содержимое совпало: только обновить отметку.
    Same,
    /// Отдать своё как текущее.
    Upload,
    /// Своё — копией на сервер, серверное — сюда.
    Both,
    Download,
}

/// Что делать: local — (время файла, sha) здесь, srv — живая серверная строка, st — отметка
/// прошлого обмена. Порядок Bo1.kt: there/here по отметке, в обоих — свежее побеждает.
fn decide(local: Option<(i64, &str)>, srv: Option<&ServerSave>, st: &State) -> Option<Step> {
    if let (Some((_, sha)), Some(s)) = (local, srv) {
        if sha == s.sha256 {
            return Some(Step::Same);
        }
    }
    let there = srv.is_some_and(|s| s.sha256 != st.server_sha);
    let here = local.is_some_and(|(_, sha)| sha != st.content);
    match (here, there) {
        (true, true) => {
            let (mtime, _) = local?;
            let s = srv?;
            let has_mark = !st.content.is_empty() || !st.server_sha.is_empty();
            // Играли и тут, и на другом устройстве с прошлого обмена: свежее побеждает.
            Some(if openmw::trusted_mtime(mtime, has_mark) > s.mtime { Step::Upload } else { Step::Both })
        }
        (true, false) => Some(Step::Upload),
        (false, true) => Some(Step::Download),
        (false, false) => None,
    }
}

/// Обмен прокачкой MW2. Блокирующий — зовётся из своего потока.
pub fn sync(server: &str, token: &str) {
    if !crate::saves::enabled("mw2") {
        return;
    }
    let Some(root) = profile_dir() else { return };
    let api = Api { server, token, family: FAMILY, agent: crate::tls::agent().timeout(TIMEOUT).build() };
    let manifest = match api.manifest() {
        Ok(m) => m,
        Err(NetError::Missing) => return,
        Err(e) => {
            crate::log(&format!("MW2: прокачку не сверить: {}", e.text()));
            return;
        }
    };
    if !KNOWN.load(Ordering::SeqCst) {
        match api.knows_family(NAME) {
            Ok(true) => KNOWN.store(true, Ordering::SeqCst),
            // Сервер ещё без обмена прокачкой MW2: молча, проверим следующим разом.
            Ok(false) | Err(NetError::Missing) => return,
            Err(e) => {
                crate::log(&format!("MW2: прокачку не сверить: {}", e.text()));
                return;
            }
        }
    }
    let srv = manifest.saves.into_iter().find(|s| !s.deleted && s.name.eq_ignore_ascii_case(NAME));
    let path = root.join(NAME);
    let local = match fs::read(&path) {
        Ok(data) if data.len() == FILE_SIZE => {
            let mtime = fs::metadata(&path).map(|m| openmw::mtime_of(&m)).unwrap_or(0);
            Some((mtime, openmw::sha_hex(&data), data))
        }
        Ok(data) => {
            crate::log(&format!("MW2: {NAME} размером {} байт, а не {FILE_SIZE}: не трогаю", data.len()));
            return;
        }
        Err(_) => None,
    };
    let state_file = state_path();
    let mut st = load_state(&state_file);
    let before = serde_json::to_string(&st).unwrap_or_default();
    let here_now = local.as_ref().map(|(m, sha, _)| (*m, sha.as_str()));
    let step = decide(here_now, srv.as_ref(), &st);
    // Имя для сервера: то, что там уже есть, иначе своё.
    let name = srv.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| NAME.to_string());

    match step {
        None => {}
        Some(Step::Same) => {
            if let (Some((mtime, sha, _)), Some(s)) = (&local, &srv) {
                st.server_sha = s.sha256.clone();
                st.content = sha.clone();
                st.name = name.clone();
                st.mtime = (*mtime).max(0) as u64 * 1000;
            }
        }
        Some(Step::Upload | Step::Both) => {
            let Some((mtime, sha, data)) = &local else { return };
            let current = step == Some(Step::Upload);
            let when = match openmw::trusted_mtime(*mtime, !st.content.is_empty()) {
                0 => *mtime,
                t => t,
            };
            if let Err(e) = api.upload(&name, when, &st.device_id, sha, current, data) {
                crate::log(&format!("MW2: прокачка не отправлена: {}", e.text()));
                return;
            }
            if current {
                st.server_sha = sha.clone();
                st.content = sha.clone();
                st.name = name.clone();
                st.mtime = (*mtime).max(0) as u64 * 1000;
                crate::log("MW2: прокачка отправлена на сервер");
            } else {
                crate::log("MW2: прокачка разошлась, здешняя ушла на сервер копией (свежее с сервера)");
            }
        }
        Some(Step::Download) => {}
    }
    if matches!(step, Some(Step::Download | Step::Both)) {
        if let Some(s) = &srv {
            download(&api, s, &root, &path, manifest.max_bytes, &mut st);
        }
    }
    if serde_json::to_string(&st).unwrap_or_default() != before || !state_file.is_file() {
        save_state(&state_file, &st);
    }
}

/// Серверную прокачку — на место через временный файл, время файла — время на сервере.
fn download(api: &Api, s: &ServerSave, root: &Path, path: &Path, max_bytes: u64, st: &mut State) {
    let data = match api.download(s.id, max_bytes) {
        Ok(d) => d,
        Err(e) => {
            crate::log(&format!("MW2: прокачка не скачана: {}", e.text()));
            return;
        }
    };
    if openmw::sha_hex(&data) != s.sha256 {
        crate::log("MW2: прокачка скачана с ошибкой (sha256 не сошёлся), не записываю");
        return;
    }
    if data.len() != FILE_SIZE {
        crate::log(&format!("MW2: {} с сервера размером {} байт, не записываю", s.name, data.len()));
        return;
    }
    // Пока качали, могли запустить игру: тогда не пишем, заберём после выхода.
    if running() {
        return;
    }
    match crate::sacred_swap::write_file(root, path, &data, s.mtime) {
        Ok((_, mtime)) => {
            st.server_sha = s.sha256.clone();
            st.content = s.sha256.clone();
            st.name = NAME.to_string();
            st.mtime = mtime.max(0) as u64 * 1000;
            crate::log("MW2: прокачка получена с сервера");
        }
        Err(e) => crate::log(&format!("MW2: {NAME} не записать: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(sha: &str, mtime: i64) -> ServerSave {
        serde_json::from_value(serde_json::json!({"id": 1, "name": NAME, "sha256": sha, "mtime": mtime})).unwrap()
    }

    fn mark(server_sha: &str, content: &str) -> State {
        State { server_sha: server_sha.into(), content: content.into(), ..State::default() }
    }

    #[test]
    fn decisions() {
        let t = 2_000_000_000;
        let m = mark("a", "a");
        // Совпало — только отметка.
        assert_eq!(decide(Some((t, "a")), Some(&srv("a", 1)), &m), Some(Step::Same));
        assert_eq!(decide(Some((t, "c")), Some(&srv("c", 1)), &State::default()), Some(Step::Same));
        // Только здесь, только на сервере.
        assert_eq!(decide(Some((t, "b")), Some(&srv("a", 1)), &m), Some(Step::Upload));
        assert_eq!(decide(Some((t, "a")), Some(&srv("c", 1)), &m), Some(Step::Download));
        // В обоих: свежее побеждает, проигравшее здесь — копией.
        assert_eq!(decide(Some((t, "b")), Some(&srv("c", 1_900_000_000)), &m), Some(Step::Upload));
        assert_eq!(decide(Some((t, "b")), Some(&srv("c", 2_100_000_000)), &m), Some(Step::Both));
        // Первый обмен: на сервере пусто — отдать; здесь пусто — забрать.
        assert_eq!(decide(Some((t, "b")), None, &State::default()), Some(Step::Upload));
        assert_eq!(decide(None, Some(&srv("c", 1)), &State::default()), Some(Step::Download));
        // Файла здесь нет, на сервере тот же, что при прошлом обмене: ничего (удаление не синкаем).
        assert_eq!(decide(None, Some(&srv("a", 1)), &m), None);
        assert_eq!(decide(Some((t, "a")), None, &m), None);
        assert_eq!(decide(None, None, &m), None);
    }

    #[test]
    fn state_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bb-mw2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mw2.json");
        let st = load_state(&path);
        assert!(st.device_id.starts_with("pc-"));
        let st = State { name: NAME.into(), mtime: 1_800_000_000_000, server_sha: "s".into(), content: "c".into(), ..st };
        save_state(&path, &st);
        assert_eq!(load_state(&path), st);
        let _ = fs::remove_dir_all(&dir);
    }
}
