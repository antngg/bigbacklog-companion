//! Прошитый Xbox 360 (полка «Freeboot» на сайте): что сейчас запущено на
//! приставке, по сети через xbdm — отладочный монитор, который DashLaunch
//! грузит плагином (порт 730, без пароля).
//!
//! Спрашиваем только `xbeinfo running` — путь запущенного .xex. Команду
//! `modules` не шлём никогда: при загруженном плагине ReLive (темы дашборда)
//! она вешала приставку (2026-09-30, дважды). `getfile` — только заголовок
//! контейнера GOD (десятки КБ), один раз на игру: там название.
//!
//! Опрос идёт своим потоком раз в POLL_EVERY; track_loop агента каждые 5 с
//! берёт последний свежий ответ (current), как будто это ещё один процесс.

use std::collections::HashMap;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::scan::{GameInfo, Running};

pub const SOURCE: &str = "x360fb";
pub const POLL_EVERY: Duration = Duration::from_secs(15);
/// Достижения из профиля (GPD) — не чаще этого, и только в дашборде: чтение
/// файлов через xbdm во время игры вешало приставку (2026-09-30). Сразу после
/// игры с неизвестным пакетом — вне очереди (узнать, что это была за игра).
const GPD_EVERY: Duration = Duration::from_secs(600);
/// GPD игры с тысячей достижений ~1 МБ; больше — не наш файл.
const GPD_MAX: u32 = 4 * 1024 * 1024;
/// Файлов за один запрос к сайту.
const GPD_BATCH: usize = 10;
/// Приставка не отвечает (выключена) — стучимся реже: включение и запуск
/// игры заметим максимум через минуту.
const POLL_WHEN_DOWN: Duration = Duration::from_secs(60);
/// Ответ старше этого — приставка выключена или ушла из сети: игры нет.
const FRESH_FOR: Duration = Duration::from_secs(40);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(6);
/// Заголовок STFS-контейнера GOD весит ~45 КБ; больше — не заголовок.
const HEADER_MAX: u32 = 512 * 1024;

/// Память потока опроса (живёт только в нём, без блокировок).
#[derive(Default)]
struct State {
    /// Путь контейнера → (Title ID, название из заголовка), чтобы не качать
    /// заголовок каждый опрос.
    headers: HashMap<String, (String, String)>,
    /// Последний сырой путь — в журнал агента, только при смене.
    last_path: Option<String>,
    /// Пакет (Package_<хэш>) → (Title ID, название), таблица на диске ПК
    /// (xbox360_titles.json рядом с конфигом). Узнать Title ID у приставки во
    /// время игры нельзя: чтение GAME:default.xex через xbdm вешает ее
    /// (2026-09-30), а имя пакета — не хэш пути контейнера. Нет в таблице —
    /// игру не шлем вовсе (лучше пропуск, чем игра «Package_…» на полке).
    packages: Option<HashMap<String, (String, String)>>,
    /// GPD, уже отправленные на сайт: имя → (размер, время изменения FILETIME).
    /// На диске рядом с конфигом — после перезапуска агента не слать все заново.
    gpd_seen: Option<HashMap<String, (u32, u64)>>,
    last_gpd_sync: Option<Instant>,
    /// Шла игра из неизвестного пакета: (пакет, unix-время, когда увидели).
    /// После выхода в дашборд — чей GPD изменился с этого момента, та и игра.
    pending_package: Option<(String, i64)>,
}

fn gpd_seen_path() -> std::path::PathBuf {
    crate::config_path().with_file_name("xbox360_gpd.json")
}

fn load_gpd_seen() -> HashMap<String, (u32, u64)> {
    std::fs::read_to_string(gpd_seen_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn filetime_unix(ft: u64) -> i64 {
    (ft / 10_000_000) as i64 - 11_644_473_600
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Строка dirlist → (имя, размер, время изменения FILETIME) для файла, None для папки.
fn dir_entry(line: &str) -> Option<(String, u32, u64)> {
    if line.ends_with(" directory") {
        return None;
    }
    let name = line.split_once("name=\"")?.1.split_once('"')?.0.to_string();
    let size = field(line, "sizelo")?;
    let change = (u64::from(field(line, "changehi")?) << 32) | u64::from(field(line, "changelo")?);
    Some((name, size, change))
}

/// Изменившиеся GPD игр из профиля — на сайт (upload), пачками. Отправленное
/// помечается только после ответа сайта: без связи попробуем в следующий раз.
fn gpd_sync(conn: &mut Conn, state: &mut State, upload: &dyn Fn(&Value) -> Option<Value>) {
    state.last_gpd_sync = Some(Instant::now());
    let entries: Vec<(String, u32, u64)> = match conn.multiline("dirlist name=\"DASHUSER:\\\"") {
        Ok(lines) => lines.iter().filter_map(|l| dir_entry(l)).collect(),
        Err(_) => return, // профиль не вошел — нечего читать
    };
    let seen = state.gpd_seen.get_or_insert_with(load_gpd_seen);
    let changed: Vec<(String, u32, u64)> = entries.into_iter()
        .filter(|(name, size, change)| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".gpd") && lower != "fffe07d1.gpd" && *size <= GPD_MAX
                && seen.get(name) != Some(&(*size, *change))
        })
        .collect();
    let pending = state.pending_package.clone();
    let mut games: HashMap<String, String> = HashMap::new();
    for batch in changed.chunks(GPD_BATCH) {
        let mut files = Vec::new();
        for (name, _, _) in batch {
            if let Some(data) = conn.get_file(&format!("DASHUSER:\\{name}")) {
                files.push(json!({"name": name, "data": base64(&data)}));
            }
        }
        if files.is_empty() {
            continue;
        }
        let Some(resp) = upload(&json!({"files": files})) else {
            crate::log("xbox360: достижения не ушли на сайт — попробую позже");
            return;
        };
        for r in resp.get("results").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(f), Some(g)) = (r.get("file").and_then(Value::as_str), r.get("game").and_then(Value::as_str)) {
                games.insert(f.to_string(), g.to_string());
            }
        }
        for (name, size, change) in batch {
            seen.insert(name.clone(), (*size, *change));
        }
        crate::log(&format!("xbox360: достижения отправлены, файлов {}", files.len()));
    }
    if let Ok(json) = serde_json::to_string(&*seen) {
        let _ = std::fs::write(gpd_seen_path(), json);
    }
    // Неизвестный пакет: чей GPD изменился с начала той игры — та и была.
    if let Some((pkg, since)) = pending {
        let fresh: Vec<&(String, u32, u64)> = changed.iter().filter(|(_, _, c)| filetime_unix(*c) >= since - 60).collect();
        if let [(name, _, _)] = fresh.as_slice() {
            let tid = name.trim_end_matches(".gpd").trim_end_matches(".GPD").to_ascii_uppercase();
            if is_hex8(&tid) {
                let label = games.get(name).cloned().unwrap_or_default();
                let packages = state.packages.get_or_insert_with(load_packages);
                packages.insert(pkg.clone(), (tid.clone(), label.clone()));
                if let Ok(json) = serde_json::to_string_pretty(&*packages) {
                    let _ = std::fs::write(packages_path(), json);
                }
                crate::log(&format!("xbox360: пакет {pkg} — это {tid} «{label}» (по профилю)"));
            }
            state.pending_package = None;
        } else if !fresh.is_empty() {
            crate::log(&format!("xbox360: пакет {pkg} — изменилось {} игр, не угадать", fresh.len()));
            state.pending_package = None;
        }
    }
}

fn packages_path() -> std::path::PathBuf {
    crate::config_path().with_file_name("xbox360_titles.json")
}

fn load_packages() -> HashMap<String, (String, String)> {
    std::fs::read_to_string(packages_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Последний ответ приставки: (что запущено — None, если дашборд или
/// оболочка; когда). Сеть ждёт без этой блокировки — track_loop не стоит.
static LAST: Mutex<Option<(Option<Running>, Instant)>> = Mutex::new(None);

/// Приложение Big Backlog в Aurora сверило достижения — сервер просит GPD
/// сейчас, не дожидаясь GPD_EVERY. Время сверки приходит в /api/agent/state
/// (x360_gpd_sync); отвечаем на каждое время один раз.
static GPD_NOW: AtomicBool = AtomicBool::new(false);
static GPD_NUDGE_SEEN: AtomicI64 = AtomicI64::new(0);

pub fn nudge_from_state(state: &Value) {
    let Some(at) = state.get("x360_gpd_sync").and_then(|v| v.as_i64()) else { return };
    if GPD_NUDGE_SEEN.swap(at, Ordering::SeqCst) != at {
        crate::log("xbox360: приложение Aurora просит синк GPD");
        GPD_NOW.store(true, Ordering::SeqCst);
    }
}

/// Запущенная игра для track_loop: свежий ответ приставки или ничего.
pub fn current() -> Option<Running> {
    let guard = LAST.lock().unwrap();
    let (running, at) = guard.as_ref()?;
    if at.elapsed() > FRESH_FOR {
        return None;
    }
    running.clone()
}

struct Conn {
    reader: BufReader<TcpStream>,
    stream: TcpStream,
}

impl Conn {
    fn open(addr: &str) -> std::io::Result<Conn> {
        let sock: SocketAddr = (addr, 730u16)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "нет адреса"))?;
        let stream = TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut conn = Conn { reader: BufReader::new(stream.try_clone()?), stream };
        let hello = conn.line()?;
        if !hello.starts_with("201") {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, hello));
        }
        Ok(conn)
    }

    fn line(&mut self) -> std::io::Result<String> {
        let mut s = String::new();
        self.reader.read_line(&mut s)?;
        Ok(s.trim_end().to_string())
    }

    fn send(&mut self, cmd: &str) -> std::io::Result<String> {
        self.stream.write_all(format!("{cmd}\r\n").as_bytes())?;
        self.line()
    }

    /// Многострочный ответ (202-) до строки «.».
    fn multiline(&mut self, cmd: &str) -> std::io::Result<Vec<String>> {
        let head = self.send(cmd)?;
        if !head.starts_with("202") {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, head));
        }
        let mut out = Vec::new();
        loop {
            let l = self.line()?;
            if l == "." {
                return Ok(out);
            }
            out.push(l);
        }
    }

    fn file_size(&mut self, path: &str) -> Option<u32> {
        let resp = self.send(&format!("getfileattributes name=\"{path}\"")).ok()?;
        // Бывает однострочным (200- …) и многострочным (202- … .)
        let body = if resp.starts_with("202") {
            let mut all = String::new();
            loop {
                let l = self.line().ok()?;
                if l == "." {
                    break;
                }
                all.push_str(&l);
                all.push(' ');
            }
            all
        } else if resp.starts_with("200") {
            resp
        } else {
            return None;
        };
        if field(&body, "sizehi").map(|v| v != 0).unwrap_or(false) {
            return None;
        }
        field(&body, "sizelo")
    }

    fn get_file(&mut self, path: &str) -> Option<Vec<u8>> {
        let resp = self.send(&format!("getfile name=\"{path}\"")).ok()?;
        if !resp.starts_with("203") {
            return None;
        }
        let mut len = [0u8; 4];
        self.reader.read_exact(&mut len).ok()?;
        let size = u32::from_le_bytes(len);
        let mut buf = vec![0u8; size as usize];
        self.reader.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn bye(mut self) {
        let _ = self.stream.write_all(b"bye\r\n");
    }
}

/// «sizelo=0x1a» → 26
fn field(s: &str, name: &str) -> Option<u32> {
    let at = s.find(&format!("{name}="))? + name.len() + 1;
    let v: String = s[at..].chars().take_while(|c| !c.is_whitespace()).collect();
    let v = v.trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(v, 16).ok()
}

fn is_hex8(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Что за путь запущен. Дашборд и оболочки — не игра (None). Игра: Title ID,
/// если он виден в пути (контейнеры в Content\…\<TitleID>\<тип>\…), путь
/// контейнера (для заголовка с названием) и имя папки на крайний случай.
#[derive(Debug, PartialEq)]
pub struct Launch {
    pub title_id: Option<String>,
    pub container: Option<String>,
    pub folder_name: String,
    /// GOD/XBLA на приставке запускаются из смонтированного пакета:
    /// «Device/Package_<хэш>/default.xex». Title ID в таком пути нет.
    pub package: Option<String>,
}

pub fn classify(path: &str) -> Option<Launch> {
    let lower = path.to_ascii_lowercase();
    // igbacklog\ — наш плагин и его загрузчик/пробы, не игры.
    const SHELLS: [&str; 10] = [
        "\\device\\flash\\", "aurora.xex", "\\freestyle", "fsd", "xexmenu", "dashlaunch",
        "launch.xex", "relive", "\\dash.xex", "\\bigbacklog\\",
    ];
    if lower.is_empty() || SHELLS.iter().any(|s| lower.contains(s)) || lower.ends_with("dash.xex") {
        return None;
    }
    let parts: Vec<&str> = path.split('\\').filter(|p| !p.is_empty()).collect();
    if let Some(pkg) = parts.iter().find(|p| p.to_ascii_lowercase().starts_with("package_")) {
        return Some(Launch { title_id: None, container: None, folder_name: String::new(), package: Some(pkg.to_string()) });
    }
    // …\<TitleID>\<тип контейнера 8 hex>\<хэш контейнера>[\…]
    for i in 0..parts.len().saturating_sub(2) {
        if is_hex8(parts[i]) && is_hex8(parts[i + 1]) && parts[i + 1].starts_with("000") {
            let container = parts[..=i + 2].join("\\");
            return Some(Launch {
                title_id: Some(parts[i].to_ascii_uppercase()),
                container: Some(format!("\\{container}")),
                folder_name: parts[i].to_ascii_uppercase(),
                package: None,
            });
        }
    }
    // Распакованная игра: …\<папка игры>\default.xex (или .xbe первого Xbox).
    let folder = parts.iter().rev().nth(1).copied().unwrap_or("");
    if folder.is_empty() {
        return None;
    }
    Some(Launch { title_id: None, container: None, folder_name: folder.to_string(), package: None })
}

/// Title ID и название из заголовка STFS (CON/LIVE/PIRS).
pub fn parse_stfs(head: &[u8]) -> Option<(String, String)> {
    if head.len() < 0x491 || !matches!(&head[..4], b"CON " | b"LIVE" | b"PIRS") {
        return None;
    }
    let tid = head[0x360..0x364].iter().map(|b| format!("{b:02X}")).collect::<String>();
    let units: Vec<u16> = head[0x411..0x491].chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    let name = String::from_utf16_lossy(&units[..end]).trim().to_string();
    Some((tid, name))
}

/// Один опрос приставки. Ошибка сети — Err (приставка выключена/не в сети).
fn poll_once(addr: &str, state: &mut State, upload: Option<&dyn Fn(&Value) -> Option<Value>>) -> std::io::Result<Option<Running>> {
    let mut conn = Conn::open(addr)?;
    let lines = conn.multiline("xbeinfo running")?;
    let path = lines.iter()
        .find_map(|l| l.split_once("name=\"").map(|(_, rest)| rest.trim_end_matches('"').to_string()))
        .unwrap_or_default();
    let path_changed = state.last_path.as_deref() != Some(path.as_str());
    if path_changed {
        crate::log(&format!("xbox360: запущено {path}"));
        state.last_path = Some(path.clone());
    }
    let Some(launch) = classify(&path) else {
        // Дашборд или оболочка — единственное время, когда можно читать
        // профиль (в игре файловые операции xbdm вешали приставку).
        if let Some(upload) = upload {
            let due = state.last_gpd_sync.map(|t| t.elapsed() >= GPD_EVERY).unwrap_or(true);
            if GPD_NOW.swap(false, Ordering::SeqCst) || due || state.pending_package.is_some() {
                gpd_sync(&mut conn, state, upload);
            }
        }
        conn.bye();
        return Ok(None);
    };
    let mut title_id = launch.title_id.clone();
    let mut name = launch.folder_name.clone();
    if let Some(pkg) = launch.package.as_deref() {
        let packages = state.packages.get_or_insert_with(load_packages);
        let Some((tid, n)) = packages.get(pkg) else {
            if state.pending_package.as_ref().map(|(p, _)| p.as_str()) != Some(pkg) {
                state.pending_package = Some((pkg.to_string(), unix_now()));
            }
            if path_changed {
                crate::log(&format!("xbox360: пакет {pkg} не в таблице xbox360_titles.json — не шлем"));
            }
            conn.bye();
            return Ok(None);
        };
        title_id = Some(tid.clone());
        name = n.clone();
    }
    if let Some(container) = launch.container.as_deref() {
        if let Some((tid, n)) = state.headers.get(container) {
            title_id = Some(tid.clone());
            name = n.clone();
        } else if let Some(size) = conn.file_size(container) {
            if size <= HEADER_MAX {
                if let Some((tid, n)) = conn.get_file(container).as_deref().and_then(parse_stfs) {
                    crate::log(&format!("xbox360: заголовок {container}: {tid} «{n}»"));
                    state.headers.insert(container.to_string(), (tid.clone(), n.clone()));
                    title_id = Some(tid);
                    name = n;
                }
            }
        }
    }
    conn.bye();
    Ok(Some(Running {
        info: GameInfo { source: SOURCE.into(), id: title_id.unwrap_or_default(), name },
        title: None,
        has_window: true,
    }))
}

/// Один опрос с чистой памятью — для `--scan`.
pub fn probe(addr: &str) -> std::io::Result<Option<Running>> {
    poll_once(addr, &mut State::default(), None)
}

/// Поток опроса: адрес приставки берётся из конфига каждый круг (пустой —
/// слежение за Xbox 360 выключено).
pub fn poll_loop(addr_of: impl Fn() -> Option<String>, upload: impl Fn(&Value) -> Option<Value>) {
    let mut down_logged = false;
    let mut state = State::default();
    loop {
        let mut wait = POLL_EVERY;
        if let Some(addr) = addr_of() {
            match poll_once(&addr, &mut state, Some(&upload)) {
                Ok(running) => {
                    *LAST.lock().unwrap() = Some((running, Instant::now()));
                    if down_logged {
                        crate::log(&format!("xbox360: {addr} снова на связи"));
                    }
                    down_logged = false;
                }
                Err(e) => {
                    *LAST.lock().unwrap() = None;
                    wait = POLL_WHEN_DOWN;
                    if !down_logged {
                        crate::log(&format!("xbox360: {addr} не отвечает ({e})"));
                        down_logged = true;
                    }
                }
            }
        }
        std::thread::sleep(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_are_not_games() {
        assert_eq!(classify("\\Device\\Flash\\dash.xex"), None);
        assert_eq!(classify("\\Device\\Harddisk0\\Partition1\\Aurora\\Aurora.xex"), None);
        assert_eq!(classify("\\Device\\Harddisk0\\Partition1\\Themes\\Blades\\dash.xex"), None);
        assert_eq!(classify(""), None);
        assert_eq!(classify("\\Device\\Harddisk0\\Partition1\\BigBacklog\\loader\\default.xex"), None);
    }

    #[test]
    fn god_container_gives_title_id() {
        let l = classify("\\Device\\Mass0\\Content\\0000000000000000\\545407DF\\00007000\\19DA12B145C092562B74").unwrap();
        assert_eq!(l.title_id.as_deref(), Some("545407DF"));
        assert_eq!(l.container.as_deref(),
                   Some("\\Device\\Mass0\\Content\\0000000000000000\\545407DF\\00007000\\19DA12B145C092562B74"));
        let l = classify("\\Device\\Harddisk0\\Partition1\\Content\\0000000000000000\\4d530aa4\\00007000\\CFF296460D6AAA50BE9D\\default.xex").unwrap();
        assert_eq!(l.title_id.as_deref(), Some("4D530AA4"));
    }

    #[test]
    fn package_path() {
        let l = classify("\\Device\\Package_F6235467840958995C6065333815D366\\default.xex").unwrap();
        assert_eq!(l.package.as_deref(), Some("Package_F6235467840958995C6065333815D366"));
        assert_eq!(l.title_id, None);
    }

    #[test]
    fn extracted_game_uses_folder() {
        let l = classify("\\Device\\Mass0\\x360\\Battlefield BC\\default.xex").unwrap();
        assert_eq!((l.title_id, l.folder_name.as_str()), (None, "Battlefield BC"));
    }

    #[test]
    fn stfs_header() {
        let mut h = vec![0u8; 0x600];
        h[..4].copy_from_slice(b"LIVE");
        h[0x360..0x364].copy_from_slice(&[0x54, 0x54, 0x07, 0xDF]);
        for (i, u) in "Table Tennis".encode_utf16().enumerate() {
            h[0x411 + i * 2..0x413 + i * 2].copy_from_slice(&u.to_be_bytes());
        }
        assert_eq!(parse_stfs(&h), Some(("545407DF".into(), "Table Tennis".into())));
    }

    #[test]
    fn base64_matches_std() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff, 0xfe, 0x00, 0x01]), "//4AAQ==");
    }

    #[test]
    fn dirlist_entry() {
        let l = "name=\"494707D4.gpd\" sizehi=0x0 sizelo=0x73de createhi=0x01d750c5 createlo=0x9b991800 changehi=0x01dd5119 changelo=0xe93b6900";
        let (name, size, change) = dir_entry(l).unwrap();
        assert_eq!((name.as_str(), size), ("494707D4.gpd", 0x73de));
        assert_eq!(filetime_unix(change), 1_790_799_962);
        assert_eq!(dir_entry("name=\"Content\" sizehi=0x0 sizelo=0x0 changehi=0x0 changelo=0x0 directory"), None);
    }

    #[test]
    fn size_field() {
        assert_eq!(field("200- sizehi=0x0 sizelo=0xb000 createhi=0x1", "sizelo"), Some(0xb000));
    }
}
