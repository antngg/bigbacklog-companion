//! OpenMW: сохранения между ПК и Декой через сервер (app/handheld_saves.py,
//! kind=pc, family=openmw) — тем же порядком, что приставки Miyoo
//! (miyoo/bigbacklog/src/saves.rs).
//!
//! Игра пишет сохранения в «Документы\My Games\OpenMW\saves\<персонаж>\*.omwsave»;
//! имя на сервере — путь внутри saves через «/» («Vasyan/42.omwsave»). Папки
//! персонажей (и сама saves) бывают ссылками на облачный диск — обход идёт
//! сквозь ссылки.
//!
//! Решает агент. Он помнит отметку прошлого синка каждого файла (openmw.json
//! рядом с config.json: какое содержимое было общим) и по ней видит, где
//! менялось: только здесь — отдать, только на сервере — забрать, в обоих
//! местах — побеждает более свежий файл, а проигравший уходит на сервер копией.
//! Удалённое на сайте (отметка удаления) убирается в скрытую
//! saves\.bigbacklog_deleted\<персонаж>, если его не меняли уже после удаления.
//! Удалённое здесь (файл был при прошлом синке, на сервере с тех пор не менялся,
//! а теперь его нет) удаляется и на сервере — иначе синк скачал бы его обратно.
//! Только если папка персонажа на месте и в ней есть другие сохранения: папка
//! на облачном диске бывает недоступна, и пропавшая целиком папка — не удаление.
//!
//! Только при включённой отправке сохранений игры «omw» (флаг save_sync с
//! сервера, saves.rs): выключено — ни скачивания, ни отправки. Папка — своя из
//! окна «Сохранения игр» или по умолчанию.
//!
//! Когда: при старте агента, сразу после включения флага, после выхода из
//! openmw.exe и раз в EVERY, пока игра не запущена (один запрос манифеста —
//! так ПК подхватывает сохранения с Деки).
//! Пока openmw.exe жив, папку не трогаем: игра пишет в неё сама.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// Процесс игры: пока он жив, сохранения не трогаем.
pub const EXE: &str = "openmw.exe";
/// Morrowind в Steam: квадратная иконка игры в окне «Сохранения игр» (saves.rs).
pub const STEAM_APPID: &str = "22320";
/// Как часто сверяться с сервером, пока игра не запущена.
pub const EVERY: Duration = Duration::from_secs(5 * 60);
const KIND: &str = "pc";
const FAMILY: &str = "openmw";
const DEVICE_LABEL: &str = "ПК";
const EXT: &str = ".omwsave";
const TRASH: &str = ".bigbacklog_deleted";
/// Сохранение OpenMW — единицы МБ: своё время на медленный канал.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Синк идёт — следующий не запускается.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Занять синк; false — уже идёт.
pub fn try_start() -> bool {
    !BUSY.swap(true, Ordering::SeqCst)
}

pub fn finish() {
    BUSY.store(false, Ordering::SeqCst);
}

/// «Документы» пользователя: из реестра (папку переносят на другой диск или в
/// OneDrive), иначе USERPROFILE\Documents.
pub fn documents() -> Option<PathBuf> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders")
        .ok();
    let from_reg = key.and_then(|k| k.get_value::<String, _>("Personal").ok()).map(|p| expand_env(&p));
    from_reg
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Documents")))
}

/// %VAR% в пути из реестра (REG_EXPAND_SZ). Неизвестная переменная остаётся как есть.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Папка по умолчанию: «Документы\My Games\OpenMW\saves».
pub fn default_saves_dir() -> Option<PathBuf> {
    documents().map(|d| d.join("My Games").join("OpenMW").join("saves"))
}

/// Папка сохранений, если OpenMW на этом ПК есть: сама saves или хотя бы
/// «My Games\OpenMW» (игру запускали, сохранений ещё нет — заберём с сервера).
fn saves_dir() -> Option<PathBuf> {
    crate::saves::dir("omw").filter(|d| d.is_dir() || d.parent().is_some_and(Path::is_dir))
}

/// OpenMW на этом ПК есть: без него и с сервером не сверяемся.
pub fn present() -> bool {
    saves_dir().is_some()
}

/// Что было общим с сервером на прошлом синке: содержимое (sha256) и
/// размер/время файла здесь в тот момент — неизменный файл не перехешируем.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Mark {
    sha: String,
    size: u64,
    mtime: i64,
}

/// openmw.json рядом с config.json.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    device_id: String,
    marks: HashMap<String, Mark>,
}

fn state_path() -> PathBuf {
    crate::config_path().with_file_name("openmw.json")
}

fn load_state(path: &Path) -> State {
    let mut st: State =
        fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if st.device_id.is_empty() {
        st.device_id = new_device_id();
    }
    st
}

fn save_state(path: &Path, st: &State) {
    let tmp = path.with_extension("json.tmp");
    let ok = serde_json::to_vec_pretty(st)
        .ok()
        .is_some_and(|b| fs::write(&tmp, b).and_then(|_| fs::rename(&tmp, path)).is_ok());
    if !ok {
        crate::log("OpenMW: не удалось записать openmw.json");
    }
}

/// id этого ПК для сервера: из MachineGuid Windows (не меняется при
/// переустановке агента), иначе из времени и номера процесса.
fn new_device_id() -> String {
    let guid = RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Cryptography")
        .and_then(|k| k.get_value::<String, _>("MachineGuid"))
        .ok()
        .map(|g| g.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_ascii_lowercase())
        .filter(|g| g.len() >= 12);
    let tail = guid.map(|g| g[..12].to_string()).unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let mixed = (nanos as u64) ^ ((std::process::id() as u64) << 32);
        format!("{:012x}", mixed & 0xffff_ffff_ffff)
    });
    format!("pc-{tail}")
}

#[derive(Debug, Clone)]
struct LocalSave {
    path: PathBuf,
    size: u64,
    mtime: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct ServerSave {
    id: i64,
    name: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    mtime: i64,
    #[serde(default)]
    deleted: bool,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    saves: Vec<ServerSave>,
    #[serde(default = "default_max")]
    max_bytes: u64,
}

fn default_max() -> u64 {
    16 * 1024 * 1024
}

fn mtime_of(meta: &fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Часть пути, которую примет сервер: не пустая, не скрытая, без разделителей
/// и без «:» (диск Windows).
fn plain_part(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && !s.contains(['/', '\\', ':']) && s.trim() == s
}

/// Имя с сервера → (персонаж, файл), только «<персонаж>/<файл>.omwsave».
fn split_name(name: &str) -> Option<(&str, &str)> {
    let (ch, file) = name.split_once('/')?;
    (plain_part(ch) && plain_part(file) && file.ends_with(EXT)).then_some((ch, file))
}

/// Все сохранения: <saves>/<персонаж>/<файл>.omwsave, ровно один уровень папок,
/// скрытые папки (и корзина) мимо. fs::metadata идёт сквозь ссылки на папки.
fn scan(root: &Path) -> HashMap<String, LocalSave> {
    let mut out = HashMap::new();
    let Ok(chars) = fs::read_dir(root) else { return out };
    for ch in chars.flatten() {
        let Ok(ch_name) = ch.file_name().into_string() else { continue };
        if !plain_part(&ch_name) || !fs::metadata(ch.path()).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(ch.path()) else { continue };
        for f in files.flatten() {
            let Ok(file) = f.file_name().into_string() else { continue };
            if !plain_part(&file) || !file.ends_with(EXT) {
                continue;
            }
            let Ok(meta) = fs::metadata(f.path()) else { continue };
            if !meta.is_file() {
                continue;
            }
            out.insert(format!("{ch_name}/{file}"), LocalSave { path: f.path(), size: meta.len(), mtime: mtime_of(&meta) });
        }
    }
    out
}

fn sha_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.hex())
}

/// Записать скачанное через временный файл: оборванная запись не должна
/// испортить сохранение. Папку персонажа создаём, если её ещё нет.
fn write_save(root: &Path, ch: &str, file: &str, data: &[u8]) -> std::io::Result<LocalSave> {
    let dir = root.join(ch);
    fs::create_dir_all(&dir)?;
    let path = dir.join(file);
    let tmp = dir.join(format!(".{file}.bbtmp"));
    fs::write(&tmp, data)?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let meta = fs::metadata(&path)?;
    Ok(LocalSave { size: meta.len(), mtime: mtime_of(&meta), path })
}

/// Удалённое на сайте — в saves\.bigbacklog_deleted\<персонаж> (игра его больше
/// не видит, но он не пропал). Папка персонажа может быть ссылкой на другой диск —
/// тогда копия и удаление вместо переноса.
fn trash(root: &Path, ch: &str, file: &str) -> std::io::Result<bool> {
    let path = root.join(ch).join(file);
    if !path.is_file() {
        return Ok(false);
    }
    let bin = root.join(TRASH).join(ch);
    fs::create_dir_all(&bin)?;
    let mut dest = bin.join(file);
    if dest.exists() {
        dest = bin.join(format!("{file}.{}", crate::unix_now()));
    }
    if fs::rename(&path, &dest).is_err() {
        fs::copy(&path, &dest)?;
        fs::remove_file(&path)?;
    }
    Ok(true)
}

/// Время файла для спора «кто свежее». Дате раньше 2000 года не верим: если
/// файл менялся с прошлого синка (отметка есть), значит, уже после него —
/// считаем «сейчас»; без отметки — самым старым.
const SANE_MTIME: i64 = 946_684_800;

fn trusted_mtime(mtime: i64, has_mark: bool) -> i64 {
    if mtime >= SANE_MTIME {
        mtime
    } else if has_mark {
        crate::unix_now()
    } else {
        0
    }
}

enum Step {
    Upload { current: bool },
    Download,
    Both,
    /// Удалено на сайте: убрать файл; keep_copy — он менялся здесь до
    /// удаления, сперва отдать его серверу копией.
    Trash { keep_copy: bool },
    /// Удалено здесь: отметить удаление на сервере.
    Remove,
}

enum NetError {
    /// Токен неверный, нет связи — дальше в этот раз не идём.
    Stop(String),
    /// Сервер без ручек сохранений ПК (старый) — молча.
    Missing,
    Other(String),
}

impl NetError {
    fn text(&self) -> &str {
        match self {
            NetError::Stop(m) | NetError::Other(m) => m,
            NetError::Missing => "сервер не знает сохранений ПК",
        }
    }
}

fn net_error(e: ureq::Error) -> NetError {
    match e {
        ureq::Error::Status(code, resp) => {
            let detail = resp
                .into_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("detail").and_then(|d| d.as_str()).map(String::from))
                .unwrap_or_else(|| format!("сервер ответил {code}"));
            match code {
                401 | 403 => NetError::Stop(detail),
                404 if detail == "Not Found" => NetError::Missing,
                _ => NetError::Other(detail),
            }
        }
        ureq::Error::Transport(_) => NetError::Stop("нет связи с сервером".into()),
    }
}

struct Api<'a> {
    server: &'a str,
    token: &'a str,
    agent: ureq::Agent,
}

impl Api<'_> {
    fn get(&self, path: &str) -> ureq::Request {
        self.agent
            .get(&format!("{}{path}", self.server))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("User-Agent", crate::USER_AGENT)
    }

    fn manifest(&self) -> Result<Manifest, NetError> {
        let resp = self.get("/api/agent/handheld/saves").query("kind", KIND).query("family", FAMILY).call();
        resp.map_err(net_error)?
            .into_json::<Manifest>()
            .map_err(|e| NetError::Other(format!("ответ не разобрать: {e}")))
    }

    #[allow(clippy::too_many_arguments)]
    fn upload(&self, name: &str, mtime: i64, device_id: &str, sha: &str, current: bool, data: &[u8])
        -> Result<(), NetError> {
        self.agent
            .post(&format!("{}/api/agent/handheld/saves/upload", self.server))
            .query("kind", KIND)
            .query("family", FAMILY)
            .query("name", name)
            .query("mtime", &mtime.to_string())
            .query("device_id", device_id)
            .query("sha256", sha)
            .query("current", if current { "true" } else { "false" })
            .query("device_label", DEVICE_LABEL)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("User-Agent", crate::USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(data)
            .map(|_| ())
            .map_err(net_error)
    }

    /// Файл удалили здесь. sha — версия при прошлом синке: если на сервере уже другая, он ответит 409
    /// и ничего не удалит. Ok(false) — там уже удалено.
    fn delete(&self, name: &str, sha: &str, device_id: &str) -> Result<bool, NetError> {
        let resp = self
            .agent
            .post(&format!("{}/api/agent/handheld/saves/delete", self.server))
            .query("kind", KIND)
            .query("family", FAMILY)
            .query("name", name)
            .query("sha256", sha)
            .query("device_id", device_id)
            .query("device_label", DEVICE_LABEL)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("User-Agent", crate::USER_AGENT)
            .call()
            .map_err(net_error)?;
        Ok(resp
            .into_json::<serde_json::Value>()
            .ok()
            .and_then(|v| v.get("deleted").and_then(|d| d.as_bool()))
            .unwrap_or(false))
    }

    fn download(&self, id: i64, max: u64) -> Result<Vec<u8>, NetError> {
        let resp = self.get(&format!("/api/agent/handheld/saves/file/{id}")).call().map_err(net_error)?;
        let mut data = Vec::new();
        resp.into_reader()
            .take(max + 1)
            .read_to_end(&mut data)
            .map_err(|_| NetError::Stop("нет связи с сервером".into()))?;
        if data.len() as u64 > max {
            return Err(NetError::Other("файл больше разрешённого".into()));
        }
        Ok(data)
    }
}

/// Обмен сохранениями OpenMW. Блокирующий — зовётся из своего потока.
/// Какие имена синкаем: у каждого персонажа только самое свежее сохранение (здесь или на сервере) —
/// сервер держит у персонажа одно (app/handheld_saves.py), старые файлы на устройствах не трогаем и не
/// шлем. Плюс отметки удаления с сайта: по ним убирается удаленное там.
fn latest_only(local: &HashMap<String, LocalSave>, server: &HashMap<String, ServerSave>) -> BTreeSet<String> {
    // Удаленное на сайте в выбор «самого свежего» не идет: его уберет (или вернет, если меняли после
    // удаления) обычная логика, а свежим станет следующее.
    let gone = |k: &String| server.get(k).is_some_and(|s| s.deleted);
    let mut newest: HashMap<&str, (i64, &String)> = HashMap::new();
    let items = local.iter().filter(|(k, _)| !gone(k)).map(|(k, l)| (k, l.mtime))
        .chain(server.iter().filter(|(_, s)| !s.deleted).map(|(k, s)| (k, s.mtime)));
    for (k, mtime) in items {
        if let Some((ch, _)) = split_name(k) {
            let e = newest.entry(ch).or_insert((mtime, k));
            if (mtime, k) > (e.0, e.1) {
                *e = (mtime, k);
            }
        }
    }
    let mut keys: BTreeSet<String> = newest.into_values().map(|(_, k)| k.clone()).collect();
    keys.extend(server.iter().filter(|(_, s)| s.deleted).map(|(k, _)| k.clone()));
    keys
}

pub fn sync(server: &str, token: &str) {
    if !crate::saves::enabled("omw") {
        return;
    }
    let Some(root) = saves_dir() else { return };
    let api = Api { server, token, agent: crate::tls::agent().timeout(TIMEOUT).build() };
    let manifest = match api.manifest() {
        Ok(m) => m,
        Err(NetError::Missing) => return,
        Err(e) => {
            crate::log(&format!("OpenMW: сохранения не сверить: {}", e.text()));
            return;
        }
    };
    let path = state_path();
    let mut st = load_state(&path);
    let before = serde_json::to_string(&st.marks).unwrap_or_default();
    let device_id = st.device_id.clone();
    let marks = &mut st.marks;
    let max_bytes = manifest.max_bytes;
    let server: HashMap<String, ServerSave> = manifest
        .saves
        .into_iter()
        .filter(|s| split_name(&s.name).is_some())
        .map(|s| (s.name.clone(), s))
        .collect();
    let local = scan(&root);
    // Папки персонажей, где сейчас есть сохранения: удаление засчитываем только в них.
    let chars_here: HashSet<String> =
        local.keys().filter_map(|k| k.split_once('/').map(|(ch, _)| ch.to_string())).collect();
    let keys: BTreeSet<String> = latest_only(&local, &server);

    for k in &keys {
        let Some((ch, file)) = split_name(k) else { continue };
        let l = local.get(k);
        let s = server.get(k);
        let mark = marks.get(k).cloned();

        // Содержимое здесь: неизменный с прошлого синка файл — по отметке.
        let local_sha = match l {
            Some(l) => match &mark {
                Some(m) if m.size == l.size && m.mtime == l.mtime => Some(m.sha.clone()),
                _ => match sha_file(&l.path) {
                    Ok(sha) => Some(sha),
                    Err(_) => continue,
                },
            },
            None => None,
        };

        let step = if let Some(gone) = s.filter(|s| s.deleted) {
            match (l, &local_sha) {
                (Some(l), Some(sha)) => {
                    let unchanged = mark.as_ref().is_some_and(|m| m.sha == *sha);
                    if !unchanged && trusted_mtime(l.mtime, mark.is_some()) > gone.mtime {
                        Step::Upload { current: true }
                    } else {
                        Step::Trash { keep_copy: !unchanged }
                    }
                }
                _ => {
                    marks.remove(k);
                    continue;
                }
            }
        } else {
            match (l, s, &local_sha) {
                (Some(l), Some(s), Some(sha)) => {
                    if *sha == s.sha256 {
                        marks.insert(k.clone(), Mark { sha: sha.clone(), size: l.size, mtime: l.mtime });
                        continue;
                    }
                    let here_changed = mark.as_ref().is_none_or(|m| m.sha != *sha);
                    let there_changed = mark.as_ref().is_none_or(|m| m.sha != s.sha256);
                    match (here_changed, there_changed) {
                        (true, false) => Step::Upload { current: true },
                        (false, true) => Step::Download,
                        // Меняли в обоих местах: свежее побеждает, второе — копией на сервер.
                        _ if trusted_mtime(l.mtime, mark.is_some()) >= s.mtime => Step::Upload { current: true },
                        _ => Step::Both,
                    }
                }
                (Some(_), None, Some(_)) => Step::Upload { current: true },
                // Был здесь при прошлом синке, на сервере с тех пор тот же — значит, удалили здесь.
                (None, Some(s), _)
                    if mark.as_ref().is_some_and(|m| m.sha == s.sha256) && chars_here.contains(ch) =>
                {
                    Step::Remove
                }
                (None, Some(_), _) => Step::Download,
                _ => continue,
            }
        };

        if let (Step::Upload { .. } | Step::Both | Step::Trash { keep_copy: true }, Some(l), Some(sha)) = (&step, l, &local_sha) {
            if l.size > max_bytes {
                crate::log(&format!("OpenMW: {k} больше {} МБ, не отправляю", max_bytes / (1024 * 1024)));
                continue;
            }
            let current = matches!(step, Step::Upload { current: true });
            let Ok(data) = fs::read(&l.path) else { continue };
            let when = match trusted_mtime(l.mtime, mark.is_some()) {
                0 => l.mtime,
                t => t,
            };
            if let Err(e) = api.upload(k, when, &device_id, sha, current, &data) {
                crate::log(&format!("OpenMW: {k} не отправлено: {}", e.text()));
                if matches!(e, NetError::Stop(_)) {
                    break;
                }
                continue;
            }
            if current {
                marks.insert(k.clone(), Mark { sha: sha.clone(), size: l.size, mtime: l.mtime });
                crate::log(&format!("OpenMW: {k} отправлено на сервер"));
            } else {
                crate::log(&format!("OpenMW: {k} отправлено на сервер копией (свежее — с сервера)"));
            }
        }
        if let Step::Remove = step {
            let sha = mark.as_ref().map(|m| m.sha.clone()).unwrap_or_default();
            match api.delete(k, &sha, &device_id) {
                Ok(_) => {
                    marks.remove(k);
                    crate::log(&format!("OpenMW: {k} удалено на ПК — удалено и на сервере"));
                }
                Err(e) => {
                    crate::log(&format!("OpenMW: {k} не удалить на сервере: {}", e.text()));
                    if matches!(e, NetError::Stop(_)) {
                        break;
                    }
                }
            }
            continue;
        }
        if let Step::Trash { .. } = step {
            match trash(&root, ch, file) {
                Ok(true) => crate::log(&format!("OpenMW: {k} удалено на сайте — убрано в {TRASH}")),
                Ok(false) => {}
                Err(e) => crate::log(&format!("OpenMW: {k} не убрать: {e}")),
            }
            marks.remove(k);
            continue;
        }
        if let (Step::Download | Step::Both, Some(s)) = (&step, s) {
            let data = match api.download(s.id, max_bytes) {
                Ok(d) => d,
                Err(e) => {
                    crate::log(&format!("OpenMW: {k} не скачано: {}", e.text()));
                    if matches!(e, NetError::Stop(_)) {
                        break;
                    }
                    continue;
                }
            };
            if sha_hex(&data) != s.sha256 {
                crate::log(&format!("OpenMW: {k} скачано с ошибкой (sha256 не сошёлся), не записываю"));
                continue;
            }
            match write_save(&root, ch, file, &data) {
                Ok(w) => {
                    marks.insert(k.clone(), Mark { sha: s.sha256.clone(), size: w.size, mtime: w.mtime });
                    crate::log(&format!("OpenMW: {k} получено с сервера"));
                }
                Err(e) => crate::log(&format!("OpenMW: {k} не записать: {e}")),
            }
        }
    }
    if serde_json::to_string(&st.marks).unwrap_or_default() != before || !path.is_file() {
        save_state(&path, &st);
    }
}

// ---------- SHA-256 (FIPS 180-4): своё, чтобы не тянуть зависимость ----------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    len: usize,
    total: u64,
}

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            buf: [0; 64],
            len: 0,
            total: 0,
        }
    }

    fn block(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b_, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b_) ^ (a & c) ^ (b_ & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b_;
            b_ = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in self.h.iter_mut().zip([a, b_, c, d, e, f, g, h]) {
            *x = x.wrapping_add(y);
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        while !data.is_empty() {
            let n = (64 - self.len).min(data.len());
            self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            data = &data[n..];
            if self.len == 64 {
                let b = self.buf;
                self.block(&b);
                self.len = 0;
            }
        }
    }

    fn hex(mut self) -> String {
        let bits = self.total.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        while (self.len + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

pub(crate) fn sha_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_only_per_character() {
        let l = |m: i64| LocalSave { path: PathBuf::new(), size: 1, mtime: m };
        let local: HashMap<String, LocalSave> = [
            ("Anton/a.omwsave".to_string(), l(10)),
            ("Anton/b.omwsave".to_string(), l(30)),
            ("Vasya/q.omwsave".to_string(), l(5)),
        ].into_iter().collect();
        let srv = |name: &str, m: i64, deleted: bool| -> ServerSave {
            serde_json::from_value(serde_json::json!({"id": 1, "name": name, "sha256": "x", "mtime": m, "deleted": deleted}))
                .unwrap()
        };
        let server: HashMap<String, ServerSave> = [
            ("Vasya/n.omwsave".to_string(), srv("Vasya/n.omwsave", 50, false)),
            ("Anton/old.omwsave".to_string(), srv("Anton/old.omwsave", 1, true)),
        ].into_iter().collect();
        let keys: Vec<String> = latest_only(&local, &server).into_iter().collect();
        // Антон: свежее здесь (b), Вася: свежее на сервере (n), плюс отметка удаления.
        assert_eq!(keys, vec!["Anton/b.omwsave", "Anton/old.omwsave", "Vasya/n.omwsave"]);
        // Самое свежее удалили на сайте — свежим становится следующее.
        let server2: HashMap<String, ServerSave> =
            [("Anton/b.omwsave".to_string(), srv("Anton/b.omwsave", 99, true))].into_iter().collect();
        let keys2: Vec<String> = latest_only(&local, &server2).into_iter().collect();
        assert_eq!(keys2, vec!["Anton/a.omwsave", "Anton/b.omwsave", "Vasya/q.omwsave"]);
    }

    #[test]
    fn sha256_matches_standard() {
        assert_eq!(sha_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(sha_hex(long), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(h.hex(), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn names() {
        assert_eq!(split_name("Vasyan/42.omwsave"), Some(("Vasyan", "42.omwsave")));
        assert_eq!(split_name("Vasyan/Autosave - 1.omwsave"), Some(("Vasyan", "Autosave - 1.omwsave")));
        assert_eq!(split_name("42.omwsave"), None);
        assert_eq!(split_name("a/b/c.omwsave"), None);
        assert_eq!(split_name("../x.omwsave"), None);
        assert_eq!(split_name(".bigbacklog_deleted/x.omwsave"), None);
        assert_eq!(split_name("C:/x.omwsave"), None);
        assert_eq!(split_name("Vasyan/x.txt"), None);
        assert_eq!(expand_env("%NO_SUCH_VAR_BB%\\Documents"), "%NO_SUCH_VAR_BB%\\Documents");
        assert_eq!(expand_env("D:\\Docs"), "D:\\Docs");
    }
}
