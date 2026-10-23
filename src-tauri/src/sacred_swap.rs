//! Sacred Gold: сохранения между ПК, Декой и AYN Thor через сервер (app/handheld_saves.py,
//! kind=pc, family=sacred) — тем же порядком, что OpenMW (openmw.rs).
//!
//! Синкаются файлы прямо в папке сохранений игры («<папка игры>\save»): слоты GAMEnn.PAK, герои
//! heroNN.pax и параметры героя от мода геймпада GAMEnn.bb.json. Имя на сервере — имя файла как
//! есть («GAME01.PAK»), без подпапок. Регистр на устройствах разный (на Thor папка «save» и имена
//! бывают другого регистра), поэтому имена сравниваем без учёта регистра: свой файл отдаём под
//! именем, которое уже есть на сервере, а новый с сервера записываем под серверным именем.
//!
//! Решение — как у OpenMW: отметка прошлого синка каждого файла (sacred-swap.json рядом с
//! config.json) показывает, где менялось. Только здесь — отдать, только на сервере — забрать, в
//! обоих местах — свежее побеждает, проигравшее уходит на сервер копией. Удалённое на сайте
//! убирается в скрытую save\.bigbacklog_deleted, если его не меняли уже после удаления. Удалённое
//! здесь удаляется и на сервере — только если в папке есть другие наши файлы: пустая или
//! недоступная папка (переставили игру) — не удаление. В отличие от OpenMW синкаются все файлы, а не
//! последний: слоты и герои друг от друга не зависят.
//!
//! Номер слота — только ячейка меню: в GAME01.PAK на ПК и на Thor бывают РАЗНЫЕ герои. Поэтому слот,
//! изменённый в обоих местах, сперва сверяется по герою (номер прохождения из заголовка, как game_id
//! у сайта): тот же — обычный спор; разные — свой слот вместе с GAMEnn.bb.json переезжает в первую
//! свободную ячейку (split_heroes) и уходит на сервер новым, а серверный ложится на прежнее место.
//! heroNN.pax — шаблоны героев по имени, их это правило не касается.
//!
//! Флаг — тот же «sacred» (save_sync с сервера), что у отправки на вкладку «Прогресс»
//! (sacred_sync в main.rs): она остаётся как была и идёт следом за обменом тем же потоком.
//!
//! Когда: при старте агента, после выхода из игры, по «Отправить сейчас» и сразу после включения
//! флага, плюс раз в EVERY (так ПК подхватывает сохранения с Деки и Thor). Пока Sacred.exe жив,
//! папку не трогаем: игра пишет в неё сама.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use crate::openmw::{self, Api, Mark, NetError, ServerSave};

/// Как часто сверяться с сервером, пока игра не запущена.
pub const EVERY: Duration = Duration::from_secs(5 * 60);
const FAMILY: &str = "sacred";
const TRASH: &str = ".bigbacklog_deleted";
/// Слот ~2,6 МБ: своё время на медленный канал.
const TIMEOUT: Duration = Duration::from_secs(120);
/// Имя для проверки, знает ли сервер игру (Api::knows_family).
const PROBE: &str = "GAME00.PAK";

/// Сервер уже принимал сохранения Sacred: больше не проверяем до перезапуска агента.
static KNOWN: AtomicBool = AtomicBool::new(false);

/// GAMEnn.PAK, GAMEnn.bb.json (две и больше цифр) и heroNN.pax (одна и больше), регистр любой.
/// Ничего, кроме букв, цифр и точек, — значит, и без разделителей пути.
pub(crate) fn is_synced(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let digits = |s: &str, min: usize| s.len() >= min && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = lower.strip_prefix("game") {
        if let Some(num) = rest.strip_suffix(".pak").or_else(|| rest.strip_suffix(".bb.json")) {
            return digits(num, 2);
        }
        return false;
    }
    lower.strip_prefix("hero").and_then(|r| r.strip_suffix(".pax")).is_some_and(|num| digits(num, 1))
}

/// Папка сохранений: своя из окна «Сохранения игр» или «<папка игры>\save». Игра стоит, а папки
/// ещё нет (не сохранялись) — она же: создадим при первой записи с сервера.
fn saves_dir() -> Option<PathBuf> {
    crate::saves::dir("sacred")
        .filter(|d| d.is_dir())
        .or_else(|| crate::scan::steam_app_dir(crate::sacred::STEAM_APPID).map(|g| g.join("save")))
}

/// Sacred на этом ПК есть: без него и с сервером не сверяемся.
pub fn present() -> bool {
    saves_dir().is_some()
}

fn state_path() -> PathBuf {
    crate::config_path().with_file_name("sacred-swap.json")
}

#[derive(Debug, Clone)]
struct Local {
    /// Имя как на диске.
    name: String,
    path: PathBuf,
    size: u64,
    mtime: i64,
}

/// Наши файлы папки: {имя в нижнем регистре: файл}. Скрытые (корзина, временные) мимо; два имени,
/// различающихся только регистром (бывает на Linux), — берём более свежее.
fn scan(root: &Path) -> HashMap<String, Local> {
    let mut out: HashMap<String, Local> = HashMap::new();
    let Ok(read) = fs::read_dir(root) else { return out };
    for e in read.flatten() {
        let Ok(name) = e.file_name().into_string() else { continue };
        if !is_synced(&name) {
            continue;
        }
        let Ok(meta) = fs::metadata(e.path()) else { continue };
        if !meta.is_file() {
            continue;
        }
        let l = Local { name, path: e.path(), size: meta.len(), mtime: openmw::mtime_of(&meta) };
        let key = l.name.to_ascii_lowercase();
        if out.get(&key).is_none_or(|old| (l.mtime, &l.name) > (old.mtime, &old.name)) {
            out.insert(key, l);
        }
    }
    out
}

/// Манифест сервера по именам без учёта регистра. Одно имя в разном регистре (прислали разные
/// устройства) — берём живое, из живых — более свежее.
fn by_lower(saves: Vec<ServerSave>) -> HashMap<String, ServerSave> {
    let mut out: HashMap<String, ServerSave> = HashMap::new();
    for s in saves.into_iter().filter(|s| is_synced(&s.name)) {
        let key = s.name.to_ascii_lowercase();
        if out.get(&key).is_none_or(|old| (!s.deleted, s.mtime) > (!old.deleted, old.mtime)) {
            out.insert(key, s);
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Содержимое совпало: обновить отметку.
    Same,
    /// Удалено и здесь, и на сайте: забыть отметку.
    Forget,
    Upload,
    Download,
    /// Забрать серверное, своё — копией на сервер.
    Both,
    /// Удалено на сайте: убрать файл; keep_copy — он менялся здесь до удаления, сперва копией.
    Trash { keep_copy: bool },
    /// Удалено здесь: отметить удаление на сервере.
    Remove,
}

/// Что делать с файлом — тот же порядок, что в openmw::sync. local — (время, sha) файла здесь;
/// here — в папке сейчас есть наши файлы (пропавшая целиком папка удалением не считается).
fn decide(local: Option<(i64, &str)>, srv: Option<&ServerSave>, mark: Option<&Mark>, here: bool) -> Option<Step> {
    let trusted = |mtime: i64| openmw::trusted_mtime(mtime, mark.is_some());
    if let Some(gone) = srv.filter(|s| s.deleted) {
        let Some((mtime, sha)) = local else { return Some(Step::Forget) };
        let unchanged = mark.is_some_and(|m| m.sha == sha);
        if !unchanged && trusted(mtime) > gone.mtime {
            return Some(Step::Upload);
        }
        return Some(Step::Trash { keep_copy: !unchanged });
    }
    match (local, srv) {
        (Some((mtime, sha)), Some(s)) => {
            if sha == s.sha256 {
                return Some(Step::Same);
            }
            let here_changed = mark.is_none_or(|m| m.sha != sha);
            let there_changed = mark.is_none_or(|m| m.sha != s.sha256);
            Some(match (here_changed, there_changed) {
                (true, false) => Step::Upload,
                (false, true) => Step::Download,
                // Меняли в обоих местах: свежее побеждает, второе — копией на сервер.
                _ if trusted(mtime) >= s.mtime => Step::Upload,
                _ => Step::Both,
            })
        }
        (Some(_), None) => Some(Step::Upload),
        // Был здесь при прошлом синке, на сервере с тех пор тот же — значит, удалили здесь.
        (None, Some(s)) if here && mark.is_some_and(|m| m.sha == s.sha256) => Some(Step::Remove),
        (None, Some(_)) => Some(Step::Download),
        (None, None) => None,
    }
}

/// Скачанное — на место через временный файл (оборванная запись не портит сохранение), время
/// файла — время сохранения на сервере: по нему мод геймпада сверяет GAMEnn.bb.json со слотом.
pub(crate) fn write_file(root: &Path, path: &Path, data: &[u8], mtime: i64) -> std::io::Result<(u64, i64)> {
    fs::create_dir_all(root)?;
    let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = root.join(format!(".{file}.bbtmp"));
    fs::write(&tmp, data)?;
    if mtime >= openmw::SANE_MTIME {
        let when = UNIX_EPOCH + Duration::from_secs(mtime as u64);
        let _ = fs::File::options().write(true).open(&tmp).and_then(|f| f.set_modified(when));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let meta = fs::metadata(path)?;
    Ok((meta.len(), openmw::mtime_of(&meta)))
}

/// Удалённое на сайте — в save\.bigbacklog_deleted (игра его больше не видит, но он не пропал).
fn trash(root: &Path, path: &Path) -> std::io::Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let bin = root.join(TRASH);
    fs::create_dir_all(&bin)?;
    let mut dest = bin.join(&file);
    if dest.exists() {
        dest = bin.join(format!("{file}.{}", crate::unix_now()));
    }
    if fs::rename(path, &dest).is_err() {
        fs::copy(path, &dest)?;
        fs::remove_file(path)?;
    }
    Ok(true)
}

/// Герой слота — номер прохождения из заголовка GAMEnn.PAK: два u32 LE по 0x0C (game_id у сайта,
/// app/sacred_save.py). Заголовок короче — не знаем.
fn hero_id(head: &[u8]) -> Option<(u32, u32)> {
    let b = head.get(0x0C..0x14)?;
    Some((u32::from_le_bytes(b[..4].try_into().ok()?), u32::from_le_bytes(b[4..].try_into().ok()?)))
}

fn hero_of_file(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut head = [0u8; 0x14];
    fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    hero_id(&head)
}

/// Содержимое своего файла: неизменный с прошлого синка — по отметке, иначе считаем.
fn local_sha(l: &Local, mark: Option<&Mark>) -> Option<String> {
    match mark {
        Some(m) if m.size == l.size && m.mtime == l.mtime => Some(m.sha.clone()),
        _ => openmw::sha_file(&l.path).ok(),
    }
}

/// Меняли и здесь, и на сервере (у decide — Upload по свежести или Both): содержимое разное, а
/// отметка не совпадает ни с одной стороной (или её нет — первый синк).
fn disputed(sha: &str, s: &ServerSave, mark: Option<&Mark>) -> bool {
    !s.deleted && sha != s.sha256 && mark.is_none_or(|m| m.sha != sha && m.sha != s.sha256)
}

/// «GAME01.PAK» → «GAME04.PAK» (регистр свой, номер двумя цифрами, как у игры).
fn renumber(name: &str, n: u32) -> String {
    let rest = &name[4..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    format!("{}{n:02}{}", &name[..4], &rest[end..])
}

/// Первая ячейка, где нет ни GAMEnn.PAK, ни GAMEnn.bb.json — ни здесь, ни на сервере (в том числе
/// отметкой удаления), без учёта регистра.
fn free_slot(root: &Path, local: &HashMap<String, Local>, server: &HashMap<String, ServerSave>) -> Option<u32> {
    (1..=99).find(|n| {
        [format!("game{n:02}.pak"), format!("game{n:02}.bb.json")]
            .iter()
            .all(|k| !local.contains_key(k) && !server.contains_key(k) && !root.join(k).exists())
    })
}

/// Свой слот key (game01.pak) вместе с его GAMEnn.bb.json — в ячейку n. Отметки старой и новой
/// ячейки забываем: свой файл дальше идёт как новый, серверный ложится на прежнее место как новый.
/// → (было, стало) — имена слота без «.PAK» для журнала.
fn move_slot(
    root: &Path,
    local: &mut HashMap<String, Local>,
    marks: &mut HashMap<String, Mark>,
    shas: &mut HashMap<String, String>,
    key: &str,
    n: u32,
) -> std::io::Result<(String, String)> {
    let stem = key.strip_suffix(".pak").unwrap_or(key);
    let pairs: Vec<(String, String)> =
        [".pak", ".bb.json"].iter().map(|ext| (format!("{stem}{ext}"), format!("game{n:02}{ext}"))).collect();
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (old, _) in &pairs {
        let Some(l) = local.get(old) else { continue };
        let dest = root.join(renumber(&l.name, n));
        if let Err(e) = fs::rename(&l.path, &dest) {
            for (from, to) in moved.iter().rev() {
                let _ = fs::rename(to, from);
            }
            return Err(e);
        }
        moved.push((l.path.clone(), dest));
    }
    let mut names = (String::new(), String::new());
    for (old, new) in &pairs {
        marks.remove(old);
        marks.remove(new);
        let Some(mut l) = local.remove(old) else { continue };
        if let Some(sha) = shas.remove(old) {
            shas.insert(new.clone(), sha);
        }
        let renamed = renumber(&l.name, n);
        if old.ends_with(".pak") {
            let cut = |s: &str| s[..s.len() - 4].to_string();
            names = (cut(&l.name), cut(&renamed));
        }
        l.path = root.join(&renamed);
        l.name = renamed;
        local.insert(new.clone(), l);
    }
    Ok(names)
}

/// Номер ячейки — не герой: в одну ячейку меню на разных устройствах пишут разных героев. Слоты
/// GAMEnn.PAK, изменённые и здесь, и на сервере, сверяем по герою (серверный для этого качаем):
/// тот же — обычный спор, свежее побеждает; разные — свой слот с его GAMEnn.bb.json переезжает в
/// свободную ячейку и дальше уходит на сервер как новый, а серверный ложится на прежнее место.
/// Скачанное — в fetched, второй раз не качаем. → ключи, которые в этот раз не трогать (не
/// скачалось, некуда переехать), или Err(Stop) — дальше не идём.
fn split_heroes(
    root: &Path,
    local: &mut HashMap<String, Local>,
    server: &HashMap<String, ServerSave>,
    marks: &mut HashMap<String, Mark>,
    shas: &mut HashMap<String, String>,
    fetched: &mut HashMap<String, Vec<u8>>,
    mut fetch: impl FnMut(&ServerSave) -> Result<Vec<u8>, NetError>,
) -> Result<BTreeSet<String>, NetError> {
    let mut hold = BTreeSet::new();
    let slots: BTreeSet<String> =
        local.keys().filter(|k| k.ends_with(".pak") && server.contains_key(*k)).cloned().collect();
    for k in slots {
        let (Some(l), Some(s), Some(sha)) = (local.get(&k), server.get(&k), shas.get(&k)) else { continue };
        if !disputed(sha, s, marks.get(&k)) {
            continue;
        }
        let bb = format!("{}.bb.json", k.trim_end_matches(".pak"));
        let data = match fetch(s) {
            Ok(d) if openmw::sha_hex(&d) == s.sha256 => d,
            Ok(_) => {
                crate::log(&format!("Sacred: {} скачано с ошибкой (sha256 не сошёлся), слот не трогаю", s.name));
                hold.extend([k.clone(), bb]);
                continue;
            }
            Err(NetError::Stop(m)) => return Err(NetError::Stop(m)),
            Err(e) => {
                crate::log(&format!("Sacred: {} не скачано: {}", s.name, e.text()));
                hold.extend([k.clone(), bb]);
                continue;
            }
        };
        let (mine, theirs) = (hero_of_file(&l.path), hero_id(&data));
        fetched.insert(k.clone(), data);
        if mine.is_none() || theirs.is_none() || mine == theirs {
            continue;
        }
        let Some(n) = free_slot(root, local, server) else {
            crate::log(&format!("Sacred: {} — другой герой, а свободной ячейки нет, слот не трогаю", s.name));
            hold.extend([k.clone(), bb]);
            continue;
        };
        match move_slot(root, local, marks, shas, &k, n) {
            Ok((from, to)) => crate::log(&format!("Sacred: {from} → {to}: другой герой")),
            Err(e) => {
                crate::log(&format!("Sacred: {} не переложить в свободную ячейку: {e}", s.name));
                hold.extend([k.clone(), bb]);
            }
        }
    }
    Ok(hold)
}

/// Обмен сохранениями Sacred. Блокирующий — зовётся из своего потока.
pub fn sync(server: &str, token: &str) {
    if !crate::saves::enabled("sacred") {
        return;
    }
    let Some(root) = saves_dir() else { return };
    let api = Api { server, token, family: FAMILY, agent: crate::tls::agent().timeout(TIMEOUT).build() };
    let manifest = match api.manifest() {
        Ok(m) => m,
        Err(NetError::Missing) => return,
        Err(e) => {
            crate::log(&format!("Sacred: сохранения не сверить: {}", e.text()));
            return;
        }
    };
    if !KNOWN.load(Ordering::SeqCst) {
        match api.knows_family(PROBE) {
            Ok(true) => KNOWN.store(true, Ordering::SeqCst),
            // Сервер ещё без обмена сохранениями Sacred — молча, проверим следующим разом.
            Ok(false) | Err(NetError::Missing) => return,
            Err(e) => {
                crate::log(&format!("Sacred: сохранения не сверить: {}", e.text()));
                return;
            }
        }
    }
    let path = state_path();
    let mut st = openmw::load_state(&path);
    let before = serde_json::to_string(&st.marks).unwrap_or_default();
    let device_id = st.device_id.clone();
    let marks = &mut st.marks;
    let max_bytes = manifest.max_bytes;
    let server = by_lower(manifest.saves);
    let mut local = scan(&root);
    let here = !local.is_empty();
    // Отметки файлов, которых нет ни здесь, ни на сервере, больше ни к чему: вернись такой файл на
    // сервер, старая отметка засчитала бы его «удалённым здесь».
    marks.retain(|k, _| local.contains_key(k) || server.contains_key(k));
    // Содержимое здесь: неизменный с прошлого синка файл — по отметке. Не прочитался — мимо.
    let mut shas: HashMap<String, String> =
        local.iter().filter_map(|(k, l)| local_sha(l, marks.get(k)).map(|sha| (k.clone(), sha))).collect();
    // Слот, где в одной ячейке разные герои, — сначала в свободную ячейку.
    let mut fetched: HashMap<String, Vec<u8>> = HashMap::new();
    let hold = match split_heroes(&root, &mut local, &server, marks, &mut shas, &mut fetched, |s| {
        api.download(s.id, max_bytes)
    }) {
        Ok(hold) => hold,
        Err(e) => {
            crate::log(&format!("Sacred: сохранения не сверить: {}", e.text()));
            if serde_json::to_string(&st.marks).unwrap_or_default() != before {
                openmw::save_state(&path, &st);
            }
            return;
        }
    };
    let keys: BTreeSet<String> = local.keys().chain(server.keys()).filter(|k| !hold.contains(*k)).cloned().collect();

    for k in &keys {
        let l = local.get(k);
        let s = server.get(k);
        let mark = marks.get(k).cloned();
        // Имя для сервера: то, что там уже есть, иначе своё.
        let name = s.map(|s| s.name.clone()).or_else(|| l.map(|l| l.name.clone())).unwrap_or_default();

        let local_sha = match l {
            Some(_) => match shas.get(k) {
                Some(sha) => Some(sha.clone()),
                None => continue,
            },
            None => None,
        };
        let here_now = l.zip(local_sha.as_deref()).map(|(l, sha)| (l.mtime, sha));
        let Some(step) = decide(here_now, s, mark.as_ref(), here) else { continue };

        match step {
            Step::Same => {
                if let (Some(l), Some(sha)) = (l, &local_sha) {
                    marks.insert(k.clone(), Mark { sha: sha.clone(), size: l.size, mtime: l.mtime });
                }
                continue;
            }
            Step::Forget => {
                marks.remove(k);
                continue;
            }
            _ => {}
        }

        if let (Step::Upload | Step::Both | Step::Trash { keep_copy: true }, Some(l), Some(sha)) = (&step, l, &local_sha) {
            if l.size > max_bytes {
                crate::log(&format!("Sacred: {name} больше {} МБ, не отправляю", max_bytes / (1024 * 1024)));
                continue;
            }
            let current = matches!(step, Step::Upload);
            let Ok(data) = fs::read(&l.path) else { continue };
            let when = match openmw::trusted_mtime(l.mtime, mark.is_some()) {
                0 => l.mtime,
                t => t,
            };
            if let Err(e) = api.upload(&name, when, &device_id, sha, current, &data) {
                crate::log(&format!("Sacred: {name} не отправлено: {}", e.text()));
                if matches!(e, NetError::Stop(_)) {
                    break;
                }
                continue;
            }
            if current {
                marks.insert(k.clone(), Mark { sha: sha.clone(), size: l.size, mtime: l.mtime });
                crate::log(&format!("Sacred: {name} отправлено на сервер"));
            } else {
                crate::log(&format!("Sacred: {name} отправлено на сервер копией (свежее — с сервера)"));
            }
        }
        if let Step::Remove = step {
            let sha = mark.as_ref().map(|m| m.sha.clone()).unwrap_or_default();
            match api.delete(&name, &sha, &device_id) {
                Ok(_) => {
                    marks.remove(k);
                    crate::log(&format!("Sacred: {name} удалено на ПК — удалено и на сервере"));
                }
                Err(e) => {
                    crate::log(&format!("Sacred: {name} не удалить на сервере: {}", e.text()));
                    if matches!(e, NetError::Stop(_)) {
                        break;
                    }
                }
            }
            continue;
        }
        if let Step::Trash { .. } = step {
            if let Some(l) = l {
                match trash(&root, &l.path) {
                    Ok(true) => crate::log(&format!("Sacred: {} удалено на сайте — убрано в {TRASH}", l.name)),
                    Ok(false) => {}
                    Err(e) => crate::log(&format!("Sacred: {} не убрать: {e}", l.name)),
                }
            }
            marks.remove(k);
            continue;
        }
        if let (Step::Download | Step::Both, Some(s)) = (&step, s) {
            let data = match fetched.remove(k).map_or_else(|| api.download(s.id, max_bytes), Ok) {
                Ok(d) => d,
                Err(e) => {
                    crate::log(&format!("Sacred: {name} не скачано: {}", e.text()));
                    if matches!(e, NetError::Stop(_)) {
                        break;
                    }
                    continue;
                }
            };
            if openmw::sha_hex(&data) != s.sha256 {
                crate::log(&format!("Sacred: {name} скачано с ошибкой (sha256 не сошёлся), не записываю"));
                continue;
            }
            // Свой файл — на его же место (регистр имени здесь свой), нового нет — под серверным именем.
            let target = l.map(|l| l.path.clone()).unwrap_or_else(|| root.join(&s.name));
            match write_file(&root, &target, &data, s.mtime) {
                Ok((size, mtime)) => {
                    marks.insert(k.clone(), Mark { sha: s.sha256.clone(), size, mtime });
                    crate::log(&format!("Sacred: {name} получено с сервера"));
                }
                Err(e) => crate::log(&format!("Sacred: {name} не записать: {e}")),
            }
        }
    }
    if serde_json::to_string(&st.marks).unwrap_or_default() != before || !path.is_file() {
        openmw::save_state(&path, &st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(name: &str, sha: &str, mtime: i64, deleted: bool) -> ServerSave {
        serde_json::from_value(serde_json::json!({"id": 1, "name": name, "sha256": sha, "mtime": mtime, "deleted": deleted}))
            .unwrap()
    }

    fn mark(sha: &str) -> Mark {
        Mark { sha: sha.into(), size: 3, mtime: 2_000_000_000 }
    }

    #[test]
    fn names() {
        for ok in ["GAME01.PAK", "game123.pak", "GAME00.bb.json", "game02.BB.JSON", "hero06.pax", "HERO7.PAX"] {
            assert!(is_synced(ok), "{ok}");
        }
        for bad in [
            "GAME1.PAK", "GAME.PAK", "GAMEab.PAK", "GAME01.PAK.bak", ".GAME01.PAK.bbtmp", "hero.pax", "hero06.pak",
            "Settings.cfg", "../GAME01.PAK", "save/GAME01.PAK", "GAME01.json",
        ] {
            assert!(!is_synced(bad), "{bad}");
        }
    }

    #[test]
    fn server_names_ignore_case() {
        let m = by_lower(vec![
            srv("game01.pak", "a", 10, false),
            srv("GAME01.PAK", "b", 20, false),
            srv("HERO06.pax", "c", 30, true),
            srv("hero06.pax", "d", 5, false),
            srv("../GAME02.PAK", "e", 1, false),
        ]);
        assert_eq!(m.len(), 2);
        // Одно имя в разном регистре: свежее, а живое — раньше отметки удаления.
        assert_eq!(m["game01.pak"].name, "GAME01.PAK");
        assert_eq!(m["hero06.pax"].sha256, "d");
    }

    #[test]
    fn decisions() {
        let t = 2_000_000_000;
        let s = |sha: &str, m: i64| srv("GAME01.PAK", sha, m, false);
        let m = mark("a");
        assert_eq!(decide(Some((t, "a")), Some(&s("a", 1)), Some(&m), true), Some(Step::Same));
        assert_eq!(decide(Some((t, "b")), Some(&s("a", 1)), Some(&m), true), Some(Step::Upload));
        assert_eq!(decide(Some((t, "a")), Some(&s("c", 1)), Some(&m), true), Some(Step::Download));
        // Меняли в обоих местах: свежее побеждает.
        assert_eq!(decide(Some((t, "b")), Some(&s("c", 1_000_000_000)), Some(&m), true), Some(Step::Upload));
        assert_eq!(decide(Some((t, "b")), Some(&s("c", 2_100_000_000)), Some(&m), true), Some(Step::Both));
        assert_eq!(decide(Some((t, "b")), None, Some(&m), true), Some(Step::Upload));
        assert_eq!(decide(None, Some(&s("c", 1)), None, true), Some(Step::Download));
        // Удалено на сайте.
        let gone = srv("GAME01.PAK", "", 2_050_000_000, true);
        assert_eq!(decide(Some((t, "a")), Some(&gone), Some(&m), true), Some(Step::Trash { keep_copy: false }));
        assert_eq!(decide(Some((t, "b")), Some(&gone), Some(&m), true), Some(Step::Trash { keep_copy: true }));
        assert_eq!(decide(Some((2_100_000_000, "b")), Some(&gone), Some(&m), true), Some(Step::Upload));
        assert_eq!(decide(None, Some(&gone), Some(&m), true), Some(Step::Forget));
        // Удалено здесь: был при прошлом синке, на сайте тот же — удалить и там. Папка пуста
        // целиком или на сайте новее — забрать.
        assert_eq!(decide(None, Some(&s("a", 1)), Some(&m), true), Some(Step::Remove));
        assert_eq!(decide(None, Some(&s("a", 1)), Some(&m), false), Some(Step::Download));
        assert_eq!(decide(None, Some(&s("c", 1)), Some(&m), true), Some(Step::Download));
        assert_eq!(decide(None, None, Some(&m), true), None);
    }

    /// Слот 0x20 байт: герой (a, b) по 0x0C, tag — чтобы содержимое различалось.
    fn pak(a: u32, b: u32, tag: u8) -> Vec<u8> {
        let mut d = vec![tag; 0x20];
        d[0x0C..0x10].copy_from_slice(&a.to_le_bytes());
        d[0x10..0x14].copy_from_slice(&b.to_le_bytes());
        d
    }

    fn live(name: &str, data: &[u8], mtime: i64) -> ServerSave {
        srv(name, &openmw::sha_hex(data), mtime, false)
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bb-sacred-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn slot_helpers() {
        assert_eq!(hero_id(&pak(40543, 52446, 0)), Some((40543, 52446)));
        assert_eq!(hero_id(&[0u8; 0x13]), None);
        assert_eq!(renumber("GAME01.PAK", 4), "GAME04.PAK");
        assert_eq!(renumber("game01.bb.json", 12), "game12.bb.json");
        let s = live("GAME01.PAK", b"srv", 1);
        assert!(disputed("loc", &s, None));
        assert!(disputed("loc", &s, Some(&mark("old"))));
        assert!(!disputed("loc", &s, Some(&mark("loc"))));
        assert!(!disputed("loc", &s, Some(&mark(&s.sha256))));
    }

    #[test]
    fn other_hero_moves_to_free_slot() {
        let dir = temp("heroes");
        let mine = pak(40543, 52446, 1); // Seraphim с ПК
        let theirs = pak(71080, 5844, 2); // новый герой с Thor в той же ячейке
        fs::write(dir.join("GAME01.PAK"), &mine).unwrap();
        fs::write(dir.join("GAME01.bb.json"), b"{\"pc\":1}").unwrap();
        fs::write(dir.join("game02.pak"), pak(1, 1, 3)).unwrap();
        let mut local = scan(&dir);
        // Ячейки 02 (здесь), 03 (на сервере), 04 (на сервере только .bb.json, удалённый) заняты.
        let server = by_lower(vec![
            live("GAME01.PAK", &theirs, 2_100_000_000),
            live("GAME01.bb.json", b"{\"thor\":1}", 2_100_000_000),
            live("GAME03.PAK", &pak(3, 3, 3), 1),
            srv("game04.BB.JSON", "", 1, true),
        ]);
        let mut marks: HashMap<String, Mark> =
            [("game01.pak".to_string(), mark("old")), ("game01.bb.json".to_string(), mark("old"))].into();
        let mut shas: HashMap<String, String> =
            local.iter().map(|(k, l)| (k.clone(), openmw::sha_file(&l.path).unwrap())).collect();
        let mut fetched = HashMap::new();
        let mut asked = Vec::new();
        let hold = split_heroes(&dir, &mut local, &server, &mut marks, &mut shas, &mut fetched, |s| {
            asked.push(s.name.clone());
            Ok(theirs.clone())
        })
        .ok()
        .unwrap();
        assert!(hold.is_empty());
        assert_eq!(asked, ["GAME01.PAK"]);
        // Свой слот с параметрами героя — в GAME05, регистр свой; отметки старой ячейки забыты.
        assert!(!dir.join("GAME01.PAK").exists() && !dir.join("GAME01.bb.json").exists());
        assert_eq!(fs::read(dir.join("GAME05.PAK")).unwrap(), mine);
        assert_eq!(fs::read(dir.join("GAME05.bb.json")).unwrap(), b"{\"pc\":1}");
        assert_eq!(local["game05.pak"].name, "GAME05.PAK");
        assert!(!local.contains_key("game01.pak") && !local.contains_key("game01.bb.json"));
        assert!(marks.is_empty());
        assert_eq!(shas["game05.pak"], openmw::sha_hex(&mine));
        assert_eq!(fetched["game01.pak"], theirs);
        // Дальше обычный ход: GAME05 — на сервер как новый, серверный GAME01 и его .bb.json — сюда.
        let t = local["game05.pak"].mtime;
        assert_eq!(decide(Some((t, shas["game05.pak"].as_str())), server.get("game05.pak"), None, true), Some(Step::Upload));
        assert_eq!(decide(None, server.get("game01.pak"), marks.get("game01.pak"), true), Some(Step::Download));
        assert_eq!(decide(None, server.get("game01.bb.json"), None, true), Some(Step::Download));

        // Тот же герой — как было: никто не переезжает, спор решает свежесть.
        let newer = pak(71080, 5844, 9);
        fs::write(dir.join("GAME01.PAK"), &newer).unwrap();
        let mut local = scan(&dir);
        let mut shas: HashMap<String, String> =
            local.iter().map(|(k, l)| (k.clone(), openmw::sha_file(&l.path).unwrap())).collect();
        let mut fetched = HashMap::new();
        let hold =
            split_heroes(&dir, &mut local, &server, &mut marks, &mut shas, &mut fetched, |_| Ok(theirs.clone()))
                .ok()
                .unwrap();
        assert!(hold.is_empty() && local.contains_key("game01.pak") && fetched.contains_key("game01.pak"));
        assert!(!dir.join("GAME06.PAK").exists());
        let t = local["game01.pak"].mtime;
        assert_eq!(decide(Some((t, shas["game01.pak"].as_str())), server.get("game01.pak"), None, true), Some(Step::Both));

        // Не скачалось — слот с его .bb.json в этот раз не трогаем; без спора не качаем вовсе.
        let mut fetched = HashMap::new();
        let hold = split_heroes(&dir, &mut local, &server, &mut marks, &mut shas, &mut fetched, |_| {
            Err(NetError::Other("нет".into()))
        })
        .ok()
        .unwrap();
        assert_eq!(hold.into_iter().collect::<Vec<_>>(), ["game01.bb.json", "game01.pak"]);
        marks.insert("game01.pak".into(), Mark { sha: shas["game01.pak"].clone(), size: 0, mtime: 0 });
        let hold = split_heroes(&dir, &mut local, &server, &mut marks, &mut shas, &mut fetched, |_| {
            panic!("лишнее скачивание")
        })
        .ok()
        .unwrap();
        assert!(hold.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_folder() {
        let dir = std::env::temp_dir().join(format!("bb-sacred-swap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(TRASH)).unwrap();
        for name in ["GAME01.PAK", "game01.bb.json", "hero06.pax", "Settings.cfg", ".GAME02.PAK.bbtmp"] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        fs::write(dir.join(TRASH).join("GAME03.PAK"), b"x").unwrap();
        let found = scan(&dir);
        let mut keys: Vec<&str> = found.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["game01.bb.json", "game01.pak", "hero06.pax"]);
        assert_eq!(found["game01.pak"].name, "GAME01.PAK");

        // Запись с сервера: новое имя — как на сервере, время — серверное.
        let target = dir.join("GAME02.PAK");
        let (size, mtime) = write_file(&dir, &target, b"slot", 1_800_000_000).unwrap();
        assert_eq!((size, mtime), (4, 1_800_000_000));
        assert_eq!(fs::read(&target).unwrap(), b"slot");
        assert!(!dir.join(".GAME02.PAK.bbtmp").exists());

        // Удалённое на сайте — в корзину, второй раз — рядом с отметкой времени.
        assert!(trash(&dir, &dir.join("GAME01.PAK")).unwrap());
        assert!(!dir.join("GAME01.PAK").exists());
        assert!(dir.join(TRASH).join("GAME01.PAK").is_file());
        assert!(!trash(&dir, &dir.join("GAME01.PAK")).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }
}
