//! Сохранения портов Xbox 360 из каталога Recomp (Forza Horizon в PinyonShift, Skate 3) между ПК и
//! Steam Deck через сервер (app/handheld_saves.py, kind=pc, family=recomp-<titleid>) — тем же
//! форматом и порядком, что плагин Деки (decky/main.py, «Сохранения портов Xbox 360»).
//!
//! Порт на ReXGlue держит сохранения как приставка: <папка пользователя>\<XUID>\<TitleID>\…
//! (+ Headers). Эта папка целиком ходит одним архивом «<TitleID>.zip»: пути внутри от папки
//! TitleID через «/», рядом bigbacklog.json с XUID и точным временем файлов (в zip время местное и
//! без пояса). Какие порты и где их папки — с сервера (`recomp_saves` в /api/agent/state,
//! app/recomp.py agent_saves): новый порт без новой сборки приложения.
//!
//! Решение — по отметке прошлого обмена (recomp_saves.json рядом с config.json): sha серверного
//! архива и отпечаток здешних файлов. Поменялось только на сервере — забрать, только здесь —
//! отдать, в обоих местах — свежее побеждает, проигравшее уходит на сервер копией. Первый обмен,
//! когда сохранение есть и тут, и на сервере, и они разные, ничего не заменяет (урок 3DS: пустая
//! новая игра затерла прогресс): здешнее уходит копией и ждет, какая сторона сдвинется первой.
//! Перед записью с сервера здешняя папка ложится архивом в <папка пользователя>\.bigbacklog-backup.
//! Пока порт запущен, его папку не трогаем.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::openmw::{self, Api, NetError, ServerSave};

/// Как часто сверяться с сервером, пока порты не запущены.
pub const EVERY: Duration = Duration::from_secs(5 * 60);
const TIMEOUT: Duration = Duration::from_secs(60);
const META: &str = "bigbacklog.json";
const BACKUPS: usize = 3;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Port {
    pub title_id: String,
    pub family: String,
    pub exes: Vec<String>,
    pub dirs: Vec<String>,
    pub skip: Vec<String>,
}

static PORTS: Mutex<Vec<Port>> = Mutex::new(Vec::new());
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn set_from_state(state: &Value) {
    let Some(arr) = state.get("recomp_saves").and_then(|v| v.as_array()) else { return };
    let strs = |v: &Value, key: &str| -> Vec<String> {
        v.get(key).and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let list = arr.iter().filter_map(|v| {
        let tid = v.get("title_id")?.as_str()?.to_uppercase();
        let family = v.get("family")?.as_str()?.to_string();
        (tid.len() == 8 && family.starts_with("recomp-")).then(|| Port {
            title_id: tid, family, exes: strs(v, "exes"), dirs: strs(v, "dirs"), skip: strs(v, "skip"),
        })
    }).collect();
    *PORTS.lock().unwrap() = list;
}

pub fn ports() -> Vec<Port> {
    PORTS.lock().unwrap().clone()
}

pub fn try_start() -> bool {
    !BUSY.swap(true, Ordering::SeqCst)
}

pub fn finish() {
    BUSY.store(false, Ordering::SeqCst);
}

/// Запущен ли какой-нибудь порт (его exe).
pub fn running(ports: &[Port]) -> bool {
    ports.iter().flat_map(|p| &p.exes).any(|e| crate::scan::exe_running(&format!("{e}.exe")))
}

/// Есть ли хоть один порт на этом ПК (папка пользователя).
pub fn present(ports: &[Port]) -> bool {
    ports.iter().any(|p| !roots(p).is_empty())
}

/// Папки пользователя порта на этом ПК: пути Windows из описания (пути Linux — Деке).
fn roots(port: &Port) -> Vec<PathBuf> {
    port.dirs.iter()
        .filter(|d| !d.starts_with('~') && !d.starts_with('/'))
        .flat_map(|d| crate::recomp_ach::expand(d))
        .filter(|p| p.is_dir())
        .collect()
}

fn skipped(port: &Port, rel: &str) -> bool {
    rel.split('/').any(|part| part.starts_with('.') || port.skip.iter().any(|s| s.eq_ignore_ascii_case(part)))
}

fn is_xuid(name: &str) -> bool {
    name.len() == 16 && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Файлы папки <XUID>\<TitleID>: путь через «/» → (размер, время).
fn files(port: &Port, folder: &Path) -> BTreeMap<String, (u64, i64)> {
    fn walk(port: &Port, base: &Path, dir: &Path, out: &mut BTreeMap<String, (u64, i64)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(port, base, &path, out);
            } else if let Ok(rel) = path.strip_prefix(base) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if !skipped(port, &rel) {
                    out.insert(rel, (meta.len(), openmw::mtime_of(&meta)));
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(port, folder, folder, &mut out);
    out
}

/// (папка пользователя, папка <XUID>\<TitleID>). Сохранений нет — самая свежая папка
/// пользователя (куда класть с сервера) и None; порта нет — None.
fn locate(port: &Port) -> Option<(PathBuf, Option<PathBuf>)> {
    let roots = roots(port);
    let mut best: Option<(i64, PathBuf, PathBuf)> = None;
    for root in &roots {
        let Ok(rd) = fs::read_dir(root) else { continue };
        for x in rd.flatten() {
            let xname = x.file_name().to_string_lossy().to_string();
            if !is_xuid(&xname) {
                continue;
            }
            let Ok(inner) = fs::read_dir(x.path()) else { continue };
            for t in inner.flatten() {
                if !t.file_name().to_string_lossy().eq_ignore_ascii_case(&port.title_id) {
                    continue;
                }
                let found = files(port, &t.path());
                let when = found.values().map(|(_, m)| *m).max().unwrap_or(0);
                if !found.is_empty() && best.as_ref().is_none_or(|(w, _, _)| when > *w) {
                    best = Some((when, root.clone(), t.path()));
                }
            }
        }
    }
    if let Some((_, root, folder)) = best {
        return Some((root, Some(folder)));
    }
    let newest = roots.into_iter().max_by_key(|r| fs::metadata(r).map(|m| openmw::mtime_of(&m)).unwrap_or(0))?;
    Some((newest, None))
}

fn stamp(port: &Port, folder: Option<&Path>) -> Option<String> {
    let folder = folder?;
    let found = files(port, folder);
    if found.is_empty() {
        return None;
    }
    let mut text: Vec<String> = found.iter().map(|(rel, (size, mtime))| format!("{rel}:{mtime}:{size}")).collect();
    if let Ok(meta) = fs::metadata(ach_file(port, folder)) {
        text.push(format!("{ACH_ENTRY}:{}:{}", openmw::mtime_of(&meta), meta.len()));
    }
    Some(openmw::sha_hex(text.join(";").as_bytes()))
}

// ---------- открытые достижения порта ----------
// Рантайм держит их рядом с сейвами, в <папка пользователя>\achievements\<TitleID>.toml, и читает при
// запуске. Едут в том же архиве именем ACH_ENTRY; при приеме открытое складывается со здешним (время
// раньшее), чтобы игра на другом устройстве не считала его закрытым.

const ACH_ENTRY: &str = "achievements.toml";
const ACH_HEADER: &str = "# Achievement unlock state - managed by ReXGlue runtime\n";

/// <папка пользователя>\achievements\<TitleID>.toml рядом с <XUID>\<TitleID>.
fn ach_file(port: &Port, folder: &Path) -> PathBuf {
    let root = folder.parent().and_then(Path::parent).unwrap_or(folder);
    root.join("achievements").join(format!("{}.toml", port.title_id))
}

/// Файл открытого: id → FILETIME как есть.
fn ach_raw(text: &str) -> BTreeMap<u32, u64> {
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
            if let Ok(ft) = rest.trim_start_matches([' ', '=']).trim().parse() {
                out.insert(id, ft);
            }
        }
    }
    out
}

fn ach_text(unlocked: &BTreeMap<u32, u64>) -> String {
    let mut out = ACH_HEADER.to_string();
    for (id, ft) in unlocked {
        out.push_str(&format!("\n[unlocked.{id}]\nfiletime = {ft}\n"));
    }
    out
}

/// Открытое с другого устройства — к здешнему файлу: ничего не закрывается, время раньшее.
fn ach_merge(path: &Path, theirs: &str) -> std::io::Result<()> {
    let mine = fs::read_to_string(path).map(|t| ach_raw(&t)).unwrap_or_default();
    let mut merged = mine.clone();
    for (id, ft) in ach_raw(theirs) {
        let keep = merged.get(&id).copied();
        if keep.is_none() || (ft != 0 && (keep == Some(0) || Some(ft) < keep)) {
            merged.insert(id, ft);
        }
    }
    if merged == mine {
        return Ok(());
    }
    let dir = path.parent().unwrap_or(path).to_path_buf();
    crate::sacred_swap::write_file(&dir, path, ach_text(&merged).as_bytes(), crate::unix_now()).map(|_| ())
}

fn newest(port: &Port, folder: &Path) -> i64 {
    files(port, folder).values().map(|(_, m)| *m).max().unwrap_or(0)
}

// ---------- zip: свой маленький, deflate через flate2 ----------

struct Entry {
    name: String,
    data: Vec<u8>,
}

fn dos_time(unix: i64) -> (u16, u16) {
    // Время в zip — для глаз; точное лежит в bigbacklog.json. UTC.
    let days = unix.max(315_532_800) / 86_400;
    let secs = unix.max(315_532_800) % 86_400;
    // Гражданская дата из числа дней (алгоритм Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let date = (((year - 1980).clamp(0, 127) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = (((secs / 3600) as u16) << 11) | ((((secs % 3600) / 60) as u16) << 5) | ((secs % 60) / 2) as u16;
    (time, date)
}

fn zip_write(entries: &[(String, Vec<u8>, i64)]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, mtime) in entries {
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data)?;
        let packed = enc.finish()?;
        let crc = crc32fast::hash(data);
        let (time, date) = dos_time(*mtime);
        let offset = out.len() as u32;
        let head = |sig: u32, central: bool| {
            let mut h = Vec::new();
            h.extend(sig.to_le_bytes());
            if central {
                h.extend(20u16.to_le_bytes());
            }
            h.extend(20u16.to_le_bytes());
            h.extend(0x0800u16.to_le_bytes()); // имена в UTF-8
            h.extend(8u16.to_le_bytes());
            h.extend(time.to_le_bytes());
            h.extend(date.to_le_bytes());
            h.extend(crc.to_le_bytes());
            h.extend((packed.len() as u32).to_le_bytes());
            h.extend((data.len() as u32).to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes());
            if central {
                h.extend([0u8; 6]); // комментарий, диск, внутренние атрибуты
                h.extend(0u32.to_le_bytes()); // внешние атрибуты
                h.extend(offset.to_le_bytes());
            }
            h.extend(name.as_bytes());
            h
        };
        out.extend(head(0x0403_4b50, false));
        out.extend(&packed);
        central.extend(head(0x0201_4b50, true));
    }
    let cd_offset = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    Ok(out)
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Файлы архива (stored и deflate — так пишет и Python на Деке). None — архив битый.
fn zip_read(b: &[u8]) -> Option<Vec<Entry>> {
    let eocd = (0..b.len().saturating_sub(21)).rev().find(|&i| u32_at(b, i) == Some(0x0605_4b50))?;
    let count = u16_at(b, eocd + 10)? as usize;
    let mut at = u32_at(b, eocd + 16)? as usize;
    let mut out = Vec::new();
    for _ in 0..count {
        if u32_at(b, at)? != 0x0201_4b50 {
            return None;
        }
        let method = u16_at(b, at + 10)?;
        let crc = u32_at(b, at + 16)?;
        let csize = u32_at(b, at + 20)? as usize;
        let usize_ = u32_at(b, at + 24)? as usize;
        let (n, e, c) = (u16_at(b, at + 28)? as usize, u16_at(b, at + 30)? as usize, u16_at(b, at + 32)? as usize);
        let local = u32_at(b, at + 42)? as usize;
        let name = String::from_utf8(b.get(at + 46..at + 46 + n)?.to_vec()).ok()?;
        at += 46 + n + e + c;
        if name.ends_with('/') {
            continue;
        }
        if u32_at(b, local)? != 0x0403_4b50 {
            return None;
        }
        let start = local + 30 + u16_at(b, local + 26)? as usize + u16_at(b, local + 28)? as usize;
        let raw = b.get(start..start + csize)?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut d = Vec::with_capacity(usize_);
                flate2::read::DeflateDecoder::new(raw).read_to_end(&mut d).ok()?;
                d
            }
            _ => return None,
        };
        if data.len() != usize_ || crc32fast::hash(&data) != crc {
            return None;
        }
        out.push(Entry { name, data });
    }
    Some(out)
}

#[derive(Serialize, Deserialize, Default)]
struct Meta {
    #[serde(default)]
    title_id: String,
    #[serde(default)]
    xuid: String,
    #[serde(default)]
    mtimes: BTreeMap<String, i64>,
}

fn pack(port: &Port, folder: &Path) -> std::io::Result<Vec<u8>> {
    let found = files(port, folder);
    let meta = Meta {
        title_id: port.title_id.clone(),
        xuid: folder.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        mtimes: found.iter().map(|(rel, (_, m))| (rel.clone(), *m)).collect(),
    };
    let mut entries = vec![(META.to_string(), serde_json::to_vec(&meta).unwrap_or_default(), crate::unix_now())];
    for (rel, (_, mtime)) in &found {
        entries.push((rel.clone(), fs::read(folder.join(rel))?, *mtime));
    }
    let ach = ach_file(port, folder);
    if let Ok(data) = fs::read(&ach) {
        let mtime = fs::metadata(&ach).map(|m| openmw::mtime_of(&m)).unwrap_or(0);
        entries.push((ACH_ENTRY.to_string(), data, mtime));
    }
    zip_write(&entries)
}

/// Архив сервера и здешняя папка — одни и те же файлы (время не в счет).
fn same(port: &Port, entries: &[Entry], folder: &Path) -> bool {
    let local = files(port, folder);
    let theirs: Vec<&Entry> = entries.iter()
        .filter(|e| e.name != META && e.name != ACH_ENTRY && !skipped(port, &e.name)).collect();
    theirs.len() == local.len()
        && theirs.iter().all(|e| local.contains_key(&e.name) && fs::read(folder.join(&e.name)).is_ok_and(|d| d == e.data))
}

fn backup(port: &Port, root: &Path, folder: &Path) -> std::io::Result<()> {
    let dir = root.join(".bigbacklog-backup");
    fs::create_dir_all(&dir)?;
    let prefix = format!("{}-", port.title_id);
    fs::write(dir.join(format!("{prefix}{}.zip", crate::unix_now())), pack(port, folder)?)?;
    let mut old: Vec<PathBuf> = fs::read_dir(&dir)?.flatten().map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix)))
        .collect();
    old.sort();
    let extra = old.len().saturating_sub(BACKUPS);
    for p in old.into_iter().take(extra) {
        let _ = fs::remove_file(p);
    }
    Ok(())
}

/// Архив сервера → папка <XUID>\<TitleID> (нет — под XUID из архива); здешнее — в резервную копию,
/// файлы, которых в архиве нет, убираются (кроме skip). Путь записанной папки.
fn unpack(port: &Port, entries: &[Entry], root: &Path, folder: Option<&Path>) -> Result<PathBuf, String> {
    let names: Vec<&Entry> = entries.iter().filter(|e| e.name != META && e.name != ACH_ENTRY).collect();
    let bad = |n: &str| n.starts_with('/') || n.contains('\\') || n.contains(':') || n.split('/').any(|p| p == ".." || p.is_empty());
    if names.is_empty() || names.iter().any(|e| bad(&e.name)) {
        return Err("архив без файлов или с чужими путями".into());
    }
    let meta: Meta = entries.iter().find(|e| e.name == META)
        .and_then(|e| serde_json::from_slice(&e.data).ok()).unwrap_or_default();
    let folder = match folder {
        Some(f) => {
            backup(port, root, f).map_err(|e| format!("резервная копия не записана: {e}"))?;
            f.to_path_buf()
        }
        None => {
            if !is_xuid(&meta.xuid) {
                return Err("в архиве нет XUID".into());
            }
            root.join(meta.xuid.to_uppercase()).join(&port.title_id)
        }
    };
    for rel in files(port, &folder).keys() {
        if !names.iter().any(|e| &e.name == rel) {
            let _ = fs::remove_file(folder.join(rel));
        }
    }
    for e in names {
        if skipped(port, &e.name) {
            continue;
        }
        let path = folder.join(e.name.replace('/', "\\"));
        let dir = path.parent().unwrap_or(&folder).to_path_buf();
        let mtime = meta.mtimes.get(&e.name).copied().unwrap_or(0);
        crate::sacred_swap::write_file(&dir, &path, &e.data, mtime).map_err(|err| format!("{}: {err}", e.name))?;
    }
    if let Some(e) = entries.iter().find(|e| e.name == ACH_ENTRY) {
        ach_merge(&ach_file(port, &folder), &String::from_utf8_lossy(&e.data))
            .map_err(|err| format!("{ACH_ENTRY}: {err}"))?;
    }
    Ok(folder)
}

// ---------- обмен ----------

#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
struct Mark {
    #[serde(skip_serializing_if = "String::is_empty")]
    server_sha: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    stamp: String,
    /// Первый обмен разошелся: [sha сервера, отпечаток здесь] на тот момент.
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict: Option<(String, String)>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    device_id: String,
    marks: BTreeMap<String, Mark>,
}

fn state_path() -> PathBuf {
    crate::config_path().with_file_name("recomp_saves.json")
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
    let ok = serde_json::to_vec_pretty(st).ok()
        .is_some_and(|b| fs::write(&tmp, b).and_then(|_| fs::rename(&tmp, path)).is_ok());
    if !ok {
        crate::log("Recomp: не удалось записать recomp_saves.json");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Первый обмен, сохранение и тут, и на сервере: сравнить содержимое.
    First,
    Upload,
    /// Здешнее — копией на сервер, серверное — сюда.
    Both,
    Download,
}

/// Что делать: local — (время, отпечаток) здесь, srv — серверная строка, mark — прошлый обмен.
fn decide(local: Option<(i64, &str)>, srv: Option<&ServerSave>, mark: &Mark) -> Option<Step> {
    let (server_sha, stamp) = match &mark.conflict {
        Some((s, l)) => (s.as_str(), l.as_str()),
        None => (mark.server_sha.as_str(), mark.stamp.as_str()),
    };
    let there = srv.is_some_and(|s| s.sha256 != server_sha);
    let here = local.is_some_and(|(_, st)| st != stamp);
    let unmarked = server_sha.is_empty() && stamp.is_empty();
    match (here, there) {
        (true, true) if unmarked => Some(Step::First),
        (true, true) => Some(if local?.0 > srv?.mtime { Step::Upload } else { Step::Both }),
        (true, false) => Some(Step::Upload),
        (false, true) => Some(Step::Download),
        (false, false) => None,
    }
}

/// Обмен всеми портами. Блокирующий — зовется из своего потока.
pub fn sync(server: &str, token: &str) {
    let ports = ports();
    let state_file = state_path();
    let mut st = load_state(&state_file);
    let before = serde_json::to_string(&st).unwrap_or_default();
    for port in &ports {
        if let Err(e) = sync_port(server, token, port, &mut st) {
            crate::log(&format!("Recomp {}: сохранение не обменялось: {e}", port.title_id));
            if matches!(e, SyncError::Stop) {
                break;
            }
        }
    }
    if serde_json::to_string(&st).unwrap_or_default() != before || !state_file.is_file() {
        save_state(&state_file, &st);
    }
}

#[derive(Debug)]
enum SyncError {
    Stop,
    Other(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Stop => write!(f, "нет связи с сервером"),
            SyncError::Other(m) => write!(f, "{m}"),
        }
    }
}

fn net(e: NetError) -> SyncError {
    match e {
        NetError::Stop(_) => SyncError::Stop,
        other => SyncError::Other(other.text().to_string()),
    }
}

fn sync_port(server: &str, token: &str, port: &Port, st: &mut State) -> Result<(), SyncError> {
    let Some((root, folder)) = locate(port) else { return Ok(()) };
    let api = Api { server, token, family: &port.family, agent: crate::tls::agent().timeout(TIMEOUT).build() };
    let manifest = match api.manifest() {
        Ok(m) => m,
        Err(NetError::Missing) => return Ok(()),
        Err(e) => return Err(net(e)),
    };
    let name = format!("{}.zip", port.title_id);
    let srv = manifest.saves.into_iter().find(|s| !s.deleted && s.name.eq_ignore_ascii_case(&name));
    let stamp = stamp(port, folder.as_deref());
    let when = folder.as_deref().map(|f| newest(port, f)).unwrap_or(0);
    let mark = st.marks.get(&port.family).cloned().unwrap_or_default();
    let step = decide(stamp.as_deref().map(|s| (when, s)), srv.as_ref(), &mark);
    let upload = |current: bool| -> Result<String, SyncError> {
        let folder = folder.as_deref().ok_or_else(|| SyncError::Other("нет папки".into()))?;
        let data = pack(port, folder).map_err(|e| SyncError::Other(format!("архив не собрать: {e}")))?;
        let sha = openmw::sha_hex(&data);
        api.upload(&name, when, &st.device_id, &sha, current, &data).map_err(net)?;
        Ok(sha)
    };
    let fetch = |s: &ServerSave| -> Result<Vec<Entry>, SyncError> {
        let data = api.download(s.id, manifest.max_bytes).map_err(net)?;
        if openmw::sha_hex(&data) != s.sha256 {
            return Err(SyncError::Other("архив скачан с ошибкой (sha256 не сошелся)".into()));
        }
        zip_read(&data).ok_or_else(|| SyncError::Other("архив с сервера не разобрать".into()))
    };
    match step {
        None => {}
        Some(Step::First) => {
            let (s, f, stamp) = (srv.as_ref().unwrap(), folder.as_deref().unwrap(), stamp.clone().unwrap_or_default());
            if same(port, &fetch(s)?, f) {
                st.marks.insert(port.family.clone(), Mark { server_sha: s.sha256.clone(), stamp, conflict: None });
            } else {
                upload(false)?;
                st.marks.insert(port.family.clone(), Mark { conflict: Some((s.sha256.clone(), stamp)), ..Mark::default() });
                crate::log(&format!("Recomp {}: сохранения разные тут и на сервере, здешнее ушло копией", port.title_id));
            }
        }
        Some(Step::Upload) => {
            let sha = upload(true)?;
            st.marks.insert(port.family.clone(), Mark { server_sha: sha, stamp: stamp.unwrap_or_default(), conflict: None });
            crate::log(&format!("Recomp {}: сохранение отправлено на сервер", port.title_id));
        }
        Some(Step::Both | Step::Download) => {
            if step == Some(Step::Both) {
                upload(false)?;
            }
            let s = srv.as_ref().unwrap();
            let entries = fetch(s)?;
            // Пока качали, могли запустить игру: тогда не пишем, заберем после выхода.
            if running(std::slice::from_ref(port)) {
                return Ok(());
            }
            let written = unpack(port, &entries, &root, folder.as_deref()).map_err(SyncError::Other)?;
            let stamp = self::stamp(port, Some(&written)).unwrap_or_default();
            st.marks.insert(port.family.clone(), Mark { server_sha: s.sha256.clone(), stamp, conflict: None });
            crate::log(&format!("Recomp {}: сохранение получено с сервера", port.title_id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(sha: &str, mtime: i64) -> ServerSave {
        serde_json::from_value(serde_json::json!({"id": 1, "name": "X.zip", "sha256": sha, "mtime": mtime})).unwrap()
    }

    fn mark(server_sha: &str, stamp: &str) -> Mark {
        Mark { server_sha: server_sha.into(), stamp: stamp.into(), conflict: None }
    }

    #[test]
    fn decisions() {
        let t = 2_000_000_000;
        let m = mark("a", "l");
        assert_eq!(decide(Some((t, "l")), Some(&srv("a", 1)), &m), None);
        assert_eq!(decide(Some((t, "l2")), Some(&srv("a", 1)), &m), Some(Step::Upload));
        assert_eq!(decide(Some((t, "l")), Some(&srv("b", 1)), &m), Some(Step::Download));
        assert_eq!(decide(Some((t, "l2")), Some(&srv("b", 1)), &m), Some(Step::Upload));
        assert_eq!(decide(Some((t, "l2")), Some(&srv("b", t + 5)), &m), Some(Step::Both));
        // Первый обмен.
        let none = Mark::default();
        assert_eq!(decide(Some((t, "l")), None, &none), Some(Step::Upload));
        assert_eq!(decide(None, Some(&srv("b", 1)), &none), Some(Step::Download));
        assert_eq!(decide(Some((t, "l")), Some(&srv("b", 1)), &none), Some(Step::First));
        // Разошлись на первом обмене: ждем, что сдвинется.
        let c = Mark { conflict: Some(("b".into(), "l".into())), ..Mark::default() };
        assert_eq!(decide(Some((t, "l")), Some(&srv("b", 1)), &c), None);
        assert_eq!(decide(Some((t, "l")), Some(&srv("c", 1)), &c), Some(Step::Download));
        assert_eq!(decide(Some((t, "l3")), Some(&srv("b", 1)), &c), Some(Step::Upload));
    }

    #[test]
    fn zip_roundtrip_and_unpack() {
        let dir = std::env::temp_dir().join(format!("bb-recomp-saves-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let port = Port { title_id: "4D5309C9".into(), family: "recomp-4d5309c9".into(), skip: vec!["marketplace".into()],
                          ..Port::default() };
        let src = dir.join("a").join("B13EBABEBABEBABE").join("4D5309C9");
        fs::create_dir_all(src.join("00000001").join("ForzaProfile")).unwrap();
        fs::create_dir_all(src.join("00000001").join("marketplace")).unwrap();
        fs::write(src.join("00000001").join("ForzaProfile").join("ForzaProfile"), vec![7u8; 50_000]).unwrap();
        fs::write(src.join("00000001").join("marketplace").join("tmpfile"), b"cache").unwrap();
        let data = pack(&port, &src).unwrap();
        let entries = zip_read(&data).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![META, "00000001/ForzaProfile/ForzaProfile"]);
        assert!(same(&port, &entries, &src));
        // На другой машине папки еще нет: ложится под XUID из архива, время файла — из bigbacklog.json.
        let root = dir.join("b");
        fs::create_dir_all(&root).unwrap();
        let written = unpack(&port, &entries, &root, None).unwrap();
        assert_eq!(written, root.join("B13EBABEBABEBABE").join("4D5309C9"));
        assert_eq!(fs::read(written.join("00000001").join("ForzaProfile").join("ForzaProfile")).unwrap(), vec![7u8; 50_000]);
        assert_eq!(stamp(&port, Some(&written)), stamp(&port, Some(&src)));
        // Открытые достижения едут в архиве и складываются со здешними: время раньшее, ничего не закрывается.
        let ach_src = dir.join("a").join("achievements");
        fs::create_dir_all(&ach_src).unwrap();
        fs::write(ach_src.join("4D5309C9.toml"), "[unlocked.155]\nfiletime = 200\n\n[unlocked.157]\nfiletime = 500\n").unwrap();
        let entries = zip_read(&pack(&port, &src).unwrap()).unwrap();
        assert!(entries.iter().any(|e| e.name == ACH_ENTRY));
        assert!(same(&port, &entries, &src));
        fs::create_dir_all(root.join("achievements")).unwrap();
        fs::write(root.join("achievements").join("4D5309C9.toml"), "[unlocked.155]\nfiletime = 300\n\n[unlocked.160]\nfiletime = 900\n").unwrap();
        unpack(&port, &entries, &root, Some(&written)).unwrap();
        let merged = ach_raw(&fs::read_to_string(root.join("achievements").join("4D5309C9.toml")).unwrap());
        assert_eq!(merged, BTreeMap::from([(155, 200), (157, 500), (160, 900)]));
        assert!(!written.join(ACH_ENTRY).exists());
        // Чужие пути не пишем.
        let evil = zip_write(&[("../x".into(), b"1".to_vec(), 0)]).unwrap();
        assert!(unpack(&port, &zip_read(&evil).unwrap(), &root, None).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
