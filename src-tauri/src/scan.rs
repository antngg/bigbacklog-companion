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
//!
//! Доступ к чужим процессам — только PROCESS_QUERY_LIMITED_INFORMATION (путь к
//! exe): ни памяти, ни командной строки игр агент не читает — безопасно для
//! античитов.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY};
use winreg::RegKey;

#[derive(Clone, Serialize, Debug, PartialEq, Eq, Hash)]
pub struct GameInfo {
    /// steam | gog | battlenet | emu
    pub source: String,
    /// appid / GOG gameID / ключ записи Blizzard / платформа эмулятора (ps2, psp, …)
    pub id: String,
    pub name: String,
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
    ("retroarch", "retroarch", "RetroArch"),
];

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
        let info = GameInfo { source: "gog".into(), id, name };
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
            let info = GameInfo { source: "battlenet".into(), id: sub.clone(), name };
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

impl Catalog {
    pub fn load() -> Catalog {
        let mut cat = Catalog::default();
        steam(&mut cat.roots);
        gog(&mut cat.roots, &mut cat.installed);
        battlenet(&mut cat.roots, &mut cat.installed);
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
    paths: HashMap<u32, (String, Option<String>)>, // pid → (имя exe, путь в нижнем регистре)
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
                let info = GameInfo { source: "emu".into(), id: (*platform).into(), name: (*emu).into() };
                merge(&mut found, info, title, has_window);
                continue;
            }
            if is_helper(&exe_lower) {
                continue;
            }
            let entry = self.paths.entry(pid).or_insert_with(|| {
                (exe_lower.clone(), sys::image_path(pid).map(|p| p.to_lowercase()))
            });
            if entry.0 != exe_lower {
                *entry = (exe_lower.clone(), sys::image_path(pid).map(|p| p.to_lowercase()));
            }
            let Some(path) = entry.1.as_deref() else { continue };
            if let Some(info) = cat.by_path(path) {
                merge(&mut found, info.clone(), title, has_window);
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

