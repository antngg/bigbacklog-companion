//! Слежение за процессами: какие игры сейчас запущены.
//!
//! Каталог установленных игр собирается из того, что лаунчеры сами пишут на
//! диск и в реестр — угадывать по имени exe не нужно:
//! - Steam: `libraryfolders.vdf` + `appmanifest_<appid>.acf` → папка игры → appid;
//! - GOG: `HKLM\SOFTWARE\GOG.com\Games\<id>` (gameID, gameName, path);
//! - Battle.net: записи «Удаление программ» от Blizzard (InstallLocation и
//!   DisplayIcon — у WoW Classic и retail одна папка, но разные подпапки exe).
//!
//! Процесс — игра, если его exe лежит внутри папки из каталога (побеждает
//! самая длинная совпавшая папка) и это не лаунчер/редактор/служба. Эмуляторы
//! узнаются по имени exe, а какая в них игра — по заголовку окна (разбирает
//! сервер). Проверяются они РАНЬШЕ каталога: RetroArch бывает Steam-приложением.
//! Порты Recomp (Ship of Harkinian, Zelda64Recompiled…) узнаются по имени exe из
//! списка сервера (`recomp_exes` в /api/agent/state) и ПОСЛЕ каталога: порт,
//! положенный в папку Steam-игры, остается Steam-игрой.
//!
//! Сторонние игры, которые человек добавил в Steam (`userdata\<id>\config\shortcuts.vdf`),
//! — тоже каталог: папка exe → игра под ее именем в Steam. На сайте
//! это строка полки Steam со значком ярлыка.
//!
//! Свои папки с играми (окно «Папки с играми») проверяются
//! последними: DRM-free копии с GOG без Galaxy, abandonware, всё вне магазинов.
//! Игра — подпапка первого уровня (exe на любой глубине внутри нее), а если exe
//! лежит прямо в добавленной папке, то сама папка. На сайт уходит только имя.
//!
//! Доступ к чужим процессам — только PROCESS_QUERY_LIMITED_INFORMATION (путь к
//! exe): ни памяти, ни командной строки игр агент не читает — безопасно для
//! античитов.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY};
use winreg::RegKey;

#[derive(Clone, Serialize, Debug, PartialEq, Eq, Hash, Default)]
pub struct GameInfo {
    /// steam | gog | battlenet | emu | recomp | folder | steamshortcut
    pub source: String,
    /// appid / GOG gameID / ключ записи Blizzard / платформа эмулятора (ps2, psp, …) / exe порта Recomp /
    /// имя папки игры
    pub id: String,
    pub name: String,
    /// Только Recomp: какие порты каталога подошли по признаку папки (слаги) и
    /// короткий хэш папки exe, к которой сервер привязывает выбор человека.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

struct Root {
    dir: String, // нижний регистр, с завершающим «\»
    info: GameInfo,
}

#[derive(Default)]
pub struct Catalog {
    roots: Vec<Root>,
    /// Установленные игры GOG и Battle.net — сервер заводит по ним строки в
    /// библиотеке, даже если игру ещё не запускали (Warcraft III Reforged
    /// появится на полке Battle.net без единой сессии).
    pub installed: Vec<GameInfo>,
}

/// Эмуляторы: имя exe (нижний регистр, без .exe) → платформа ретро-полки.
const EMULATORS: &[(&str, &str, &str)] = &[
    ("pcsx2-qt", "ps2", "PCSX2"),
    ("pcsx2", "ps2", "PCSX2"),
    ("pcsx2x64", "ps2", "PCSX2"),
    ("pcsx2-avx2", "ps2", "PCSX2"),
    ("duckstation-qt-x64-releaseltcg", "ps1", "DuckStation"),
    ("duckstation-qt", "ps1", "DuckStation"),
    ("duckstation", "ps1", "DuckStation"),
    ("ppssppwindows64", "psp", "PPSSPP"),
    ("ppssppwindows", "psp", "PPSSPP"),
    ("rpcs3", "ps3", "RPCS3"),
    ("xenia_canary", "xbox360", "Xenia"),
    ("xenia", "xbox360", "Xenia"),
    ("flycast", "dc", "Flycast"),
    ("redream", "dc", "Redream"),
    ("melonds", "ds", "melonDS"),
    ("desmume", "ds", "DeSmuME"),
    ("mgba", "gba", "mGBA"),
    ("dolphin", "gc", "Dolphin"),
    ("ryujinx", "switch", "Ryujinx"),
    ("cemu", "wiiu", "Cemu"),
    ("project64", "n64", "Project64"),
    ("simple64-gui", "n64", "simple64"),
    ("simple64", "n64", "simple64"),
    ("retroarch", "retroarch", "RetroArch"),
];

/// Порт Recomp из списка сервера (app/recomp.py::agent_exes).
#[derive(Clone)]
struct RecompPort {
    slug: String,
    /// Название для «Сейчас»: игра, а у общего exe имя первого порта.
    name: String,
    game: String,
    /// Признак папки: файл рядом с exe (в его папке или трех выше) или
    /// «path:слово» в пути к exe. Пусто — порт узнается по одному имени.
    marker: Vec<String>,
}

/// Порты Recomp: имя exe (нижний регистр, без .exe) → порты с этим exe. Их
/// бывает несколько (ps2EntryRunner.exe, общий раннер автора), тогда решает
/// признак папки, а что не решил он, решает сервер (app/recomp_pick.py).
/// Список приходит с сервера, так что новый порт в каталоге сайта агент узнает
/// без новой сборки; до первого ответа сервера список пуст.
static RECOMP: Mutex<Option<HashMap<String, Vec<RecompPort>>>> = Mutex::new(None);
/// Что признаки сказали про папку exe (путь → (название, подошедшие слаги)):
/// файлы рядом с игрой не проверяются заново каждые 5 секунд.
static RECOMP_HITS: Mutex<Option<HashMap<String, (String, Vec<String>)>>> = Mutex::new(None);

pub fn set_recomp_from_state(state: &serde_json::Value) {
    let Some(arr) = state.get("recomp_exes").and_then(|v| v.as_array()) else { return };
    let mut map: HashMap<String, Vec<RecompPort>> = HashMap::new();
    for v in arr {
        let Some(exe) = v.get("exe").and_then(|x| x.as_str()).map(str::to_lowercase) else { continue };
        if exe.is_empty() {
            continue;
        }
        let name = v.get("name").and_then(|x| x.as_str()).unwrap_or(&exe).to_string();
        let text = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let marker = v
            .get("marker")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).filter(|m| !m.is_empty()).collect())
            .unwrap_or_default();
        let game = Some(text("game")).filter(|g| !g.is_empty()).unwrap_or_else(|| name.clone());
        map.entry(exe).or_default().push(RecompPort { slug: text("slug"), name, game, marker });
    }
    *RECOMP.lock().unwrap() = Some(map);
    *RECOMP_HITS.lock().unwrap() = None;
}

/// Признак порта в папке exe: «path:слово» в пути или файл рядом (в папке exe и трех выше:
/// раннер бывает собран в build\...\Release).
fn marker_hit(marker: &str, path: &str) -> bool {
    if let Some(word) = marker.strip_prefix("path:") {
        // Только буквы и цифры с обеих сторон: «God of War» и «god-of-war» оба — godofwar.
        let squash = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
        let word = squash(word);
        return !word.is_empty() && squash(path).contains(&word);
    }
    let rel = marker.replace('/', "\\");
    let Some(dir) = std::path::Path::new(path).parent() else { return false };
    dir.ancestors().take(4).any(|d| d.join(&rel).exists())
}

/// Какой это порт Recomp: название для «Сейчас» и слаги, которые подошли по
/// признаку папки (None — путь к exe не прочитался, решает сервер по имени).
/// Подошел чей-то признак — только эти порты; не подошел ничей — порты без
/// признака (у них хватает имени). Пусто — сервер спросит человека.
fn recomp_match(stem: &str, path: Option<&str>) -> Option<(String, Option<Vec<String>>)> {
    let ports = RECOMP.lock().unwrap().as_ref()?.get(stem).cloned()?;
    let first = ports.first()?.name.clone();
    let Some(path) = path else { return Some((first, None)) };
    if let Some((name, slugs)) = RECOMP_HITS.lock().unwrap().as_ref().and_then(|m| m.get(path).cloned()) {
        return Some((name, Some(slugs)));
    }
    let hit: Vec<&RecompPort> =
        ports.iter().filter(|p| p.marker.iter().any(|m| marker_hit(m, path))).collect();
    let picked: Vec<&RecompPort> =
        if hit.is_empty() { ports.iter().filter(|p| p.marker.is_empty()).collect() } else { hit };
    let name = match picked.as_slice() {
        [one] if ports.len() > 1 => one.game.clone(),
        _ => first,
    };
    let slugs: Vec<String> = picked.iter().map(|p| p.slug.clone()).filter(|s| !s.is_empty()).collect();
    RECOMP_HITS.lock().unwrap().get_or_insert_with(HashMap::new).insert(path.to_string(), (name.clone(), slugs.clone()));
    Some((name, Some(slugs)))
}

/// Короткий хэш папки exe (FNV-1a): сервер привязывает к нему выбор порта, а
/// сам путь (с именем пользователя Windows) не уходит с компьютера.
fn dir_hash(path: &str) -> String {
    let dir = path.rsplit_once('\\').map(|(d, _)| d).unwrap_or(path);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in dir.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Где лежит exe порта Recomp, который запускался в этом сеансе приложения (exe → полный путь,
/// нижний регистр): от него recomp_ach.rs ищет папку достижений порта.
static RECOMP_PATHS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub fn recomp_path(exe: &str) -> Option<String> {
    RECOMP_PATHS.lock().unwrap().as_ref()?.get(exe).cloned()
}

// ---------- Свои папки с играми ----------

/// Добавленные папки как их указал человек, самая глубокая первой: вложенная
/// папка (`D:\Games\DOS`) точнее той, что ее содержит (`D:\Games`).
static GAME_DIRS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub const FOLDER_SOURCE: &str = "folder";
/// Сторонние игры Steam: на сайте строка полки Steam со значком ярлыка (без
/// подключенного Steam — полка GOG, как своя папка).
pub const SHORTCUT_SOURCE: &str = "steamshortcut";

fn has_game_dirs() -> bool {
    !GAME_DIRS.lock().unwrap().is_empty()
}

pub fn set_game_dirs(dirs: &[String]) {
    let mut v: Vec<String> = dirs.iter().map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect();
    v.sort_by_key(|d| std::cmp::Reverse(parts(d).len()));
    *GAME_DIRS.lock().unwrap() = v;
}

fn parts(p: &str) -> Vec<&str> {
    p.split(['\\', '/']).filter(|c| !c.is_empty()).collect()
}

/// Какая игра из своих папок у exe: (имя папки игры, чистое название). Сравнение по
/// частям пути без учета регистра — у кириллицы и прочего регистр меняет длину в байтах.
pub fn folder_match(exe_path: &str) -> Option<(String, String)> {
    let dirs = GAME_DIRS.lock().unwrap();
    if dirs.is_empty() {
        return None;
    }
    let path = parts(exe_path);
    for dir in dirs.iter() {
        let root = parts(dir);
        if path.len() <= root.len() || !root.iter().zip(&path).all(|(a, b)| a.to_lowercase() == b.to_lowercase()) {
            continue;
        }
        let folder = if path.len() == root.len() + 1 { *root.last()? } else { path[root.len()] };
        return Some((folder.to_string(), clean_name(folder)));
    }
    None
}

/// Подпапки первого уровня своей папки — для окна: какие игры в ней узнаются.
/// exe прямо в папке — она сама одна игра.
pub fn folder_games(dir: &str) -> (bool, Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return (false, Vec::new()) };
    let mut subs = Vec::new();
    let mut exe_here = false;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        match e.file_type() {
            Ok(t) if t.is_dir() => subs.push(name),
            Ok(t) if t.is_file() && name.to_lowercase().ends_with(".exe") => exe_here = true,
            _ => {}
        }
    }
    subs.sort_by_key(|n| n.to_lowercase());
    (exe_here, subs)
}

/// Название игры по имени папки: без пометок раздач в скобках, версий, «GOG»,
/// «Portable» и подчеркиваний. Ничего не осталось — имя папки как есть.
pub fn clean_name(folder: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in folder.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = (depth - 1).max(0),
            '_' if depth == 0 => out.push(' '),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    const NOISE: &[&str] = &["gog", "gog.com", "drm-free", "drmfree", "portable", "repack", "rip", "goty"];
    let is_version = |w: &str| {
        let w = w.strip_prefix(['v', 'V']).unwrap_or(w);
        !w.is_empty() && w.chars().next().is_some_and(|c| c.is_ascii_digit())
            && w.contains('.') && w.chars().all(|c| c.is_ascii_digit() || c == '.' || c.is_ascii_lowercase())
    };
    let words: Vec<&str> = out.split_whitespace()
        .filter(|w| !NOISE.contains(&w.to_lowercase().as_str()) && !is_version(w))
        .collect();
    let name = words.join(" ").trim_matches(|c: char| c == '-' || c == '–' || c == '.' || c.is_whitespace()).to_string();
    if name.is_empty() { folder.trim().to_string() } else { name }
}

/// Папки, которые нельзя добавить целиком: в них не только игры, и браузер или
/// офис посчитались бы игрой. Возвращает, что сказать человеку.
pub fn forbidden_dir(dir: &str) -> Option<&'static str> {
    let d = norm_dir(dir);
    if parts(&d).len() <= 1 {
        return Some("целый диск добавить нельзя, укажите папку с играми");
    }
    let env = |k: &str| std::env::var(k).ok().map(|v| norm_dir(&v));
    for k in ["WINDIR", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "APPDATA", "LOCALAPPDATA",
              "USERPROFILE", "ProgramData"] {
        if env(k).is_some_and(|e| e == d || (k == "WINDIR" && d.starts_with(&e))) {
            return Some("в этой папке не только игры, укажите папку с играми внутри нее");
        }
    }
    None
}

/// Не игры, хоть и лежат в папке игры.
fn is_helper(exe_lower: &str) -> bool {
    const PARTS: &[&str] = &[
        "launcher", "editor", "switcher", "crash", "helper", "updater", "update", "setup", "unins",
        "redist", "vcredist", "dxsetup", "easyanticheat", "eac_", "battleye", "beservice", "werfault",
        "report", "installer", "agent", "service", "overlay", "webhelper", "cefprocess", "browser",
    ];
    PARTS.iter().any(|p| exe_lower.contains(p))
}

fn norm_dir(p: &str) -> String {
    let mut s = p.trim().trim_matches('"').replace('/', "\\").to_lowercase();
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

/// Строки `"ключ" "значение"` из VDF/ACF Steam (вложенность не нужна).
fn vdf_pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('"').collect();
            (parts.len() >= 5).then(|| (parts[1].to_lowercase(), parts[3].replace("\\\\", "\\")))
        })
        .collect()
}

/// Папка самого Steam (SteamPath из реестра).
pub fn steam_path() -> Option<PathBuf> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Valve\Steam").ok()?;
    let steam_path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(steam_path.replace('/', "\\")))
}

/// Папка статистики Steam (appcache\stats). Файл
/// UserGameStats_<accountid>_<appid>.bin Steam переписывает, когда игра
/// сохраняет статистику, — в том числе в момент ачивки.
pub fn steam_stats_dir() -> Option<PathBuf> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam").ok()?;
    let steam_path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(steam_path.replace('/', "\\")).join("appcache").join("stats"))
}

/// Библиотеки Steam: папка самого Steam и пути из libraryfolders.vdf, без повторов.
fn steam_libraries() -> Vec<PathBuf> {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam") else { return vec![] };
    let Ok(steam_path) = key.get_value::<String, _>("SteamPath") else { return vec![] };
    let base = PathBuf::from(steam_path.replace('/', "\\"));
    let mut libs = vec![base.clone()];
    if let Ok(vdf) = std::fs::read_to_string(base.join("steamapps").join("libraryfolders.vdf")) {
        libs.extend(vdf_pairs(&vdf).into_iter().filter(|(k, _)| k == "path").map(|(_, v)| PathBuf::from(v)));
    }
    let mut seen = std::collections::HashSet::new();
    libs.retain(|lib| seen.insert(norm_dir(&lib.to_string_lossy())));
    libs
}

/// Папка установленной Steam-игры: appmanifest_<appid>.acf → installdir.
pub fn steam_app_dir(appid: &str) -> Option<PathBuf> {
    steam_libraries().into_iter().find_map(|lib| {
        let apps = lib.join("steamapps");
        let text = std::fs::read_to_string(apps.join(format!("appmanifest_{appid}.acf"))).ok()?;
        let name = vdf_pairs(&text).into_iter().find(|(k, _)| k == "installdir").map(|(_, v)| v)?;
        // Пустой installdir дал бы всю steamapps\common.
        if name.trim().is_empty() {
            return None;
        }
        let dir = apps.join("common").join(name.trim());
        dir.is_dir().then_some(dir)
    })
}

/// Запущен ли процесс с таким именем exe (без учёта регистра, «sacred.exe»).
pub fn exe_running(exe: &str) -> bool {
    sys::processes().iter().any(|(_, name)| name.eq_ignore_ascii_case(exe))
}

fn steam(roots: &mut Vec<Root>) {
    for lib in steam_libraries() {
        let apps = lib.join("steamapps");
        let Ok(entries) = std::fs::read_dir(&apps) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
            let pairs: HashMap<String, String> = vdf_pairs(&text).into_iter().collect();
            let (Some(appid), Some(dir)) = (pairs.get("appid"), pairs.get("installdir")) else { continue };
            // Пустой installdir дал бы корнем всю steamapps\common — под эту
            // игру попал бы любой процесс из чужих папок.
            if dir.trim().is_empty() {
                continue;
            }
            roots.push(Root {
                dir: norm_dir(&apps.join("common").join(dir).to_string_lossy()),
                info: GameInfo {
                    source: "steam".into(),
                    id: appid.clone(),
                    name: pairs.get("name").cloned().unwrap_or_default(),
                    ..Default::default()
                },
            });
        }
    }
}

fn gog(roots: &mut Vec<Root>, installed: &mut Vec<GameInfo>) {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(games) = hklm.open_subkey_with_flags("SOFTWARE\\GOG.com\\Games", KEY_READ | KEY_WOW64_32KEY) else {
        return;
    };
    for sub in games.enum_keys().flatten() {
        let Ok(k) = games.open_subkey(&sub) else { continue };
        let id: String = k.get_value("gameID").unwrap_or(sub.clone());
        let name: String = k.get_value("gameName").unwrap_or_default();
        let Ok(path) = k.get_value::<String, _>("path") else { continue };
        let info = GameInfo { source: "gog".into(), id, name, ..Default::default() };
        installed.push(info.clone());
        roots.push(Root { dir: norm_dir(&path), info });
    }
}

fn battlenet(roots: &mut Vec<Root>, installed: &mut Vec<GameInfo>) {
    const UNINSTALL: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
    let views = [
        (HKEY_LOCAL_MACHINE, KEY_READ | KEY_WOW64_32KEY),
        (HKEY_LOCAL_MACHINE, KEY_READ | KEY_WOW64_64KEY),
        (HKEY_CURRENT_USER, KEY_READ),
    ];
    let mut seen = std::collections::HashSet::new();
    for (hive, flags) in views {
        let Ok(root) = RegKey::predef(hive).open_subkey_with_flags(UNINSTALL, flags) else { continue };
        for sub in root.enum_keys().flatten() {
            let Ok(k) = root.open_subkey(&sub) else { continue };
            let publisher: String = k.get_value("Publisher").unwrap_or_default();
            let name: String = k.get_value("DisplayName").unwrap_or_default();
            let location: String = k.get_value("InstallLocation").unwrap_or_default();
            if !publisher.contains("Blizzard") || name.is_empty() || name == "Battle.net" || location.is_empty() {
                continue;
            }
            // Steam пишет в «Удаление программ» свои игры с издателем из
            // appmanifest («Steam App 2344520» — Diablo IV из Steam): это
            // Steam-игры, их узнаёт каталог Steam.
            if sub.starts_with("Steam App ") || location.to_lowercase().contains("\\steamapps\\") {
                continue;
            }
            if !seen.insert(sub.clone()) {
                continue;
            }
            let info = GameInfo { source: "battlenet".into(), id: sub.clone(), name, ..Default::default() };
            installed.push(info.clone());
            // Папка exe из DisplayIcon точнее папки установки: у WoW retail и
            // трёх Classic папка установки общая, а exe — в _retail_/_classic_.
            let icon: String = k.get_value("DisplayIcon").unwrap_or_default();
            let icon = icon.trim().trim_matches('"');
            let icon = icon.split(',').next().unwrap_or("");
            let icon_lower = icon.to_lowercase();
            let icon_file = icon_lower.rsplit('\\').next().unwrap_or("");
            if !icon.is_empty() && !is_helper(icon_file) {
                if let Some(parent) = PathBuf::from(icon).parent() {
                    roots.push(Root { dir: norm_dir(&parent.to_string_lossy()), info: info.clone() });
                }
            }
            roots.push(Root { dir: norm_dir(&location), info });
        }
    }
}

// ---------- Сторонние игры Steam (shortcuts.vdf) ----------

/// Не игры, хоть их и добавляют в Steam: браузеры, оболочки, другие лаунчеры.
/// Их папка стала бы игрой целиком.
const SHORTCUT_NOT_GAMES: &[&str] = &[
    "steam", "chrome", "msedge", "firefox", "opera", "brave", "vivaldi", "yandex", "explorer", "cmd",
    "powershell", "pwsh", "wscript", "cscript", "conhost", "upc", "uplay", "eadesktop", "origin", "galaxyclient",
    "playnite.desktopapp", "playnite.fullscreenapp", "heroic", "itch", "battle.net", "xboxpcapp", "gamebar",
    "discord", "spotify", "obs64", "vlc", "mpc-hc64", "kodi", "parsecd", "moonlight", "steamlink",
];

/// Записи binary VDF (shortcuts.vdf): у каждой игры ключи в нижнем регистре →
/// строки (числа не нужны). 0x00 — вложенная запись, 0x01 — строка,
/// 0x02 — int32, 0x08 — конец записи.
fn binary_vdf_entries(data: &[u8]) -> Vec<HashMap<String, String>> {
    fn cstr(data: &[u8], pos: &mut usize) -> Option<String> {
        let end = data.get(*pos..)?.iter().position(|&b| b == 0)? + *pos;
        let s = String::from_utf8_lossy(&data[*pos..end]).to_string();
        *pos = end + 1;
        Some(s)
    }
    fn map(data: &[u8], pos: &mut usize, depth: usize, out: &mut Vec<HashMap<String, String>>) -> Option<()> {
        let mut fields = HashMap::new();
        loop {
            let kind = *data.get(*pos)?;
            *pos += 1;
            if kind == 0x08 {
                break;
            }
            let key = cstr(data, pos)?.to_lowercase();
            match kind {
                0x00 => map(data, pos, depth + 1, out)?,
                0x01 => {
                    fields.insert(key, cstr(data, pos)?);
                }
                0x02 | 0x04 => *pos += 4,
                0x07 => *pos += 8,
                _ => return None,
            }
        }
        // Корень → «shortcuts» → «0», «1»…: игры на третьем уровне.
        if depth == 2 {
            out.push(fields);
        }
        Some(())
    }
    let mut out = Vec::new();
    let mut pos = 0;
    // Битый хвост файла не отменяет уже прочитанные игры.
    let _ = map(data, &mut pos, 0, &mut out);
    out
}

/// Сторонняя игра Steam → корень каталога: папка ее exe, название, которое
/// человек дал ей в Steam. None — не игра (лаунчер, браузер, эмулятор, ярлык).
fn shortcut_root(fields: &HashMap<String, String>) -> Option<Root> {
    let name = fields.get("appname")?.trim().to_string();
    let exe = fields.get("exe")?.trim().trim_matches('"').replace('/', "\\");
    let exe_lower = exe.to_lowercase();
    let file = exe_lower.rsplit('\\').next()?;
    let (stem, ext) = file.rsplit_once('.')?;
    if name.is_empty() || !matches!(ext, "exe" | "bat" | "cmd") || SHORTCUT_NOT_GAMES.contains(&stem)
        || EMULATORS.iter().any(|(e, _, _)| *e == stem) || is_helper(file) {
        return None;
    }
    let dir = PathBuf::from(&exe).parent()?.to_string_lossy().to_string();
    if forbidden_dir(&dir).is_some() {
        return None;
    }
    let info = GameInfo { source: SHORTCUT_SOURCE.into(), id: name.clone(), name, ..Default::default() };
    Some(Root { dir: norm_dir(&dir), info })
}

/// Сторонние игры из Steam всех аккаунтов этого ПК (userdata\<id>\config\shortcuts.vdf).
/// Steam не отдает их в Web API и не считает по ним время, так что считает приложение.
/// Игра внутри папки, которую уже знает каталог (Steam, GOG, Battle.net), остается той игрой.
fn steam_shortcuts(roots: &mut Vec<Root>, installed: &mut Vec<GameInfo>) {
    let Some(base) = steam_path() else { return };
    let Ok(users) = std::fs::read_dir(base.join("userdata")) else { return };
    let mut seen = std::collections::HashSet::new();
    for user in users.flatten() {
        let Ok(data) = std::fs::read(user.path().join("config").join("shortcuts.vdf")) else { continue };
        for fields in binary_vdf_entries(&data) {
            let Some(root) = shortcut_root(&fields) else { continue };
            if roots.iter().any(|r| root.dir.starts_with(&r.dir) || r.dir.starts_with(&root.dir))
                || !seen.insert(root.info.name.to_lowercase()) {
                continue;
            }
            installed.push(root.info.clone());
            roots.push(root);
        }
    }
}

impl Catalog {
    pub fn load() -> Catalog {
        let mut cat = Catalog::default();
        steam(&mut cat.roots);
        gog(&mut cat.roots, &mut cat.installed);
        battlenet(&mut cat.roots, &mut cat.installed);
        steam_shortcuts(&mut cat.roots, &mut cat.installed);
        // Самая длинная (самая точная) папка — первой.
        cat.roots.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()));
        cat
    }

    pub fn roots_count(&self) -> usize {
        self.roots.len()
    }

    fn by_path(&self, exe_path_lower: &str) -> Option<&GameInfo> {
        self.roots.iter().find(|r| exe_path_lower.starts_with(&r.dir)).map(|r| &r.info)
    }
}

// ---------- Процессы ----------

#[derive(Clone, Debug)]
pub struct Running {
    pub info: GameInfo,
    /// Заголовок окна — для эмуляторов это и есть «какая игра».
    pub title: Option<String>,
    /// Есть видимое окно заметного размера: фоновые службы игрой не считаются.
    pub has_window: bool,
}

mod sys {
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };

    pub fn processes() -> Vec<(u32, String)> {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap as isize == -1 {
                return out;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut entry);
            while ok != 0 {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                out.push((entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile[..len])));
                ok = Process32NextW(snap, &mut entry);
            }
            CloseHandle(snap);
        }
        out
    }

    pub fn image_path(pid: u32) -> Option<String> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
        }
    }

    /// pid → заголовки видимых окон заметного размера (самое длинное — первым).
    pub fn windows() -> HashMap<u32, Vec<String>> {
        unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let map = &mut *(lparam as *mut HashMap<u32, Vec<String>>);
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            let mut r: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut r) == 0 || r.right - r.left < 320 || r.bottom - r.top < 200 {
                return 1;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            map.entry(pid).or_default().push(title);
            1
        }
        let mut map: HashMap<u32, Vec<String>> = HashMap::new();
        unsafe {
            EnumWindows(Some(cb), &mut map as *mut _ as LPARAM);
        }
        for titles in map.values_mut() {
            titles.sort_by_key(|t| std::cmp::Reverse(t.len()));
        }
        map
    }
}

/// Кэш путей exe по pid: OpenProcess на каждый процесс каждые 5 секунд не
/// нужен — путь у живого pid не меняется.
#[derive(Default)]
pub struct Scanner {
    // pid → (имя exe, путь в нижнем регистре, путь как есть — только при своих папках с играми)
    paths: HashMap<u32, (String, Option<String>, Option<String>)>,
}

impl Scanner {
    pub fn scan(&mut self, cat: &Catalog) -> Vec<Running> {
        let procs = sys::processes();
        let alive: std::collections::HashSet<u32> = procs.iter().map(|(pid, _)| *pid).collect();
        self.paths.retain(|pid, _| alive.contains(pid));
        let windows = sys::windows();
        let mut found: HashMap<GameInfo, Running> = HashMap::new();
        for (pid, exe) in procs {
            let exe_lower = exe.to_lowercase();
            let stem = exe_lower.trim_end_matches(".exe");
            let titles = windows.get(&pid);
            let has_window = titles.is_some();
            let title = titles.and_then(|t| t.first().cloned()).filter(|t| !t.is_empty());

            if let Some((_, platform, emu)) = EMULATORS.iter().find(|(name, _, _)| *name == stem) {
                let info = GameInfo {
                    source: "emu".into(),
                    id: (*platform).into(),
                    name: (*emu).into(),
                    ..Default::default()
                };
                merge(&mut found, info, title, has_window);
                continue;
            }
            if is_helper(&exe_lower) {
                continue;
            }
            let with_path = |exe: &str| {
                let path = sys::image_path(pid);
                (exe.to_string(), path.as_ref().map(|p| p.to_lowercase()), path.filter(|_| has_game_dirs()))
            };
            let entry = self.paths.entry(pid).or_insert_with(|| with_path(&exe_lower));
            if entry.0 != exe_lower {
                *entry = with_path(&exe_lower);
            }
            if let Some(info) = entry.1.as_deref().and_then(|path| cat.by_path(path)) {
                merge(&mut found, info.clone(), title, has_window);
            } else if let Some((name, ports)) = recomp_match(stem, entry.1.as_deref()) {
                if let Some(path) = entry.1.clone() {
                    RECOMP_PATHS.lock().unwrap().get_or_insert_with(HashMap::new).insert(stem.to_string(), path);
                }
                let dir = entry.1.as_deref().map(dir_hash);
                let info = GameInfo { source: "recomp".into(), id: stem.to_string(), name, ports, dir };
                merge(&mut found, info, title, has_window);
            } else if let Some((id, name)) = entry.2.as_deref().and_then(folder_match) {
                let info = GameInfo { source: FOLDER_SOURCE.into(), id, name, ..Default::default() };
                merge(&mut found, info, title, has_window);
            }
        }
        found.into_values().collect()
    }
}

/// У одной игры бывает несколько процессов (игра + её помощники) — окно
/// засчитывается от любого из них, заголовок — самый длинный.
fn merge(found: &mut HashMap<GameInfo, Running>, info: GameInfo, title: Option<String>, has_window: bool) {
    let r = found.entry(info.clone()).or_insert(Running { info, title: None, has_window: false });
    r.has_window |= has_window;
    if let Some(t) = title {
        if r.title.as_ref().map(|old| t.len() > old.len()).unwrap_or(true) {
            r.title = Some(t);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn state(rows: serde_json::Value) {
        set_recomp_from_state(&serde_json::json!({ "recomp_exes": rows }));
    }

    #[test]
    fn recomp_marker_picks_port() {
        let root = std::env::temp_dir().join(format!("bb-recomp-{}", std::process::id()));
        let bb = root.join("bloodborne");
        let sotc = root.join("sotc").join("build").join("release");
        std::fs::create_dir_all(&bb).unwrap();
        std::fs::create_dir_all(&sotc).unwrap();
        std::fs::write(bb.join("Bloodborne.exe"), b"").unwrap();
        std::fs::write(root.join("sotc").join("SotC.exe"), b"").unwrap();
        state(serde_json::json!([
            {"exe": "bb-probe", "name": "Bloodborne", "slug": "bloodborne", "game": "Bloodborne", "marker": ["Bloodborne.exe"]},
            {"exe": "bb-probe", "name": "Bloodborne", "slug": "sotc", "game": "Shadow of the Colossus", "marker": ["SotC.exe"]},
            {"exe": "runner", "name": "DOAXBV", "slug": "doaxbv", "game": "DOAXBV", "marker": ["path:doax"]},
            {"exe": "solo", "name": "Solo"}
        ]));
        let p = |d: &std::path::Path, exe: &str| d.join(exe).to_string_lossy().to_lowercase();
        let (name, ports) = recomp_match("bb-probe", Some(&p(&bb, "bb-probe.exe"))).unwrap();
        assert_eq!((name.as_str(), ports), ("Bloodborne", Some(vec!["bloodborne".to_string()])));
        // Признак в папке выше exe: раннер собран в build\release.
        let (name, ports) = recomp_match("bb-probe", Some(&p(&sotc, "bb-probe.exe"))).unwrap();
        assert_eq!((name.as_str(), ports), ("Shadow of the Colossus", Some(vec!["sotc".to_string()])));
        // Ничей признак не подошел: пусто, сервер спросит человека.
        let (_, ports) = recomp_match("bb-probe", Some(&p(&root, "bb-probe.exe"))).unwrap();
        assert_eq!(ports, Some(vec![]));
        assert_eq!(recomp_match("runner", Some(r"c:\games\doaxbv-0.5.3\runner.exe")).unwrap().1,
                   Some(vec!["doaxbv".to_string()]));
        assert_eq!(recomp_match("runner", Some(r"c:\games\doa3\runner.exe")).unwrap().1, Some(vec![]));
        assert!(marker_hit("path:godofwar", r"d:\god of war - recomp\ps2entryrunner.exe"));
        // Без признака и без пути: по одному имени, как раньше.
        assert_eq!(recomp_match("solo", None).unwrap(), ("Solo".to_string(), None));
        assert!(recomp_match("other", None).is_none());
        assert_eq!(dir_hash(r"c:\a\x.exe"), dir_hash(r"c:\a\y.exe"));
        assert_ne!(dir_hash(r"c:\a\x.exe"), dir_hash(r"c:\b\x.exe"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn folder_games_by_first_level_subfolder() {
        set_game_dirs(&[r"D:\Games".into(), r"d:\games\DOS\".into(), r"E:\Fallout2".into()]);
        assert_eq!(folder_match(r"D:\Games\Fallout 2\bin\x64\fallout2.exe"),
                   Some(("Fallout 2".into(), "Fallout 2".into())));
        // Вложенная добавленная папка точнее: игра — ее подпапка, а не «DOS».
        assert_eq!(folder_match(r"D:\GAMES\DOS\Doom\doom.exe").map(|m| m.0), Some("Doom".into()));
        // exe прямо в добавленной папке — она сама игра.
        assert_eq!(folder_match(r"e:\fallout2\fallout2.exe").map(|m| m.0), Some("Fallout2".into()));
        assert_eq!(folder_match(r"D:\Другое\Игра\game.exe"), None);
        assert_eq!(folder_match(r"D:\Gamesx\a\b.exe"), None);
        set_game_dirs(&[r"D:\Игры".into()]);
        assert_eq!(folder_match(r"d:\ИГРЫ\Ведьмак\witcher.exe").map(|m| m.0), Some("Ведьмак".into()));
        set_game_dirs(&[]);
    }

    #[test]
    fn steam_shortcuts_parsed() {
        fn entry(i: &str, fields: &[(&str, &str)]) -> Vec<u8> {
            let mut v = vec![0x00];
            v.extend(i.as_bytes());
            v.push(0);
            v.push(0x02);
            v.extend(b"appid\0");
            v.extend(7u32.to_le_bytes());
            for (k, val) in fields {
                v.push(0x01);
                v.extend(k.as_bytes());
                v.push(0);
                v.extend(val.as_bytes());
                v.push(0);
            }
            v.push(0x00);
            v.extend(b"tags\0");
            v.extend(b"\x010\0favorite\0");
            v.push(0x08);
            v.push(0x08);
            v
        }
        let mut data = vec![0x00];
        data.extend(b"shortcuts\0");
        data.extend(entry("0", &[("AppName", "Fallout 2"), ("Exe", r#""D:\Old\Fallout2\fallout2.exe""#)]));
        data.extend(entry("1", &[("appname", "Chrome"), ("exe", r#""C:\Apps\Chrome\chrome.exe""#)]));
        data.extend(entry("2", &[("AppName", "Doom"), ("Exe", r"D:\Old\dosbox\doom.bat")]));
        data.extend(entry("3", &[("AppName", "Store"), ("Exe", r#""steam://rungameid/1""#)]));
        data.extend([0x08, 0x08]);
        let entries = binary_vdf_entries(&data);
        assert_eq!(entries.len(), 4);
        let roots: Vec<Root> = entries.iter().filter_map(shortcut_root).collect();
        let got: Vec<(&str, &str)> = roots.iter().map(|r| (r.dir.as_str(), r.info.name.as_str())).collect();
        assert_eq!(got, vec![(r"d:\old\fallout2\", "Fallout 2"), (r"d:\old\dosbox\", "Doom")]);
        assert_eq!(roots[0].info.source, SHORTCUT_SOURCE);
        // Обрезанный файл: что успели прочитать, то и есть.
        assert_eq!(binary_vdf_entries(&data[..data.len() / 2]).len(), 1);
    }

    #[test]
    fn folder_name_cleaned() {
        assert_eq!(clean_name("Jagged_Alliance_2 [GOG]"), "Jagged Alliance 2");
        assert_eq!(clean_name("Heroes of Might and Magic 3 (v4.0) GOG"), "Heroes of Might and Magic 3");
        assert_eq!(clean_name("Blood v1.21 Portable"), "Blood");
        assert_eq!(clean_name("Diablo 2"), "Diablo 2");
        assert_eq!(clean_name("S.T.A.L.K.E.R. Shadow of Chernobyl"), "S.T.A.L.K.E.R. Shadow of Chernobyl");
        assert_eq!(clean_name("[GOG]"), "[GOG]");
    }

    #[test]
    fn whole_disk_and_system_folders_refused() {
        assert!(forbidden_dir(r"D:\").is_some());
        assert!(forbidden_dir(r"C:\Windows\System32").is_some());
        if let Ok(pf) = std::env::var("ProgramFiles") {
            assert!(forbidden_dir(&pf).is_some());
            assert!(forbidden_dir(&format!(r"{pf}\Games")).is_none());
        }
        assert!(forbidden_dir(r"D:\Games").is_none());
    }
}
