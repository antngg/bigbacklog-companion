//! Big Backlog Agent — трей и плашки опыта/квестов/сессий поверх игр (Windows).
//!
//! Раз в POLL_SECS спрашивает у сервера /api/agent/state (вход по токену из
//! «Настройки → Аккаунт» на сайте) и показывает прирост опыта и квесты,
//! которые можно сдать; по выходу из игры — итог сессии. Вид плашек — не
//! копия, а тот же код, что у сайта: static/popups.js и popups.css агент
//! скачивает с сервера и держит на диске (refresh_popup_assets), окно плашки
//! (ui/toast.*) только вставляет их и отдаёт данные.
//!
//! Окно плашки создаётся только на время показа (в покое агент — это трей и
//! поток опроса, WebView2 не живёт), прозрачное, не ловит мышь и не
//! активируется: показ идёт через ShowWindow(SW_SHOWNOACTIVATE), а не через
//! window.show(), — игра в оконном или безрамочном режиме фокус не теряет.
//! Плашки идут строго по одной. Пока Windows сообщает эксклюзивный
//! полноэкранный режим (старые DX9-игры), они копятся и показываются после
//! выхода из игры — по очереди, а не друг на друге.
//!
//! В Steam Big Picture с плагином Decky плашки показывает плагин —
//! уведомлением Steam, как на Деке: агент отдаёт ему плашку файлом
//! (relay_target), а своё окно не рисует.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, PhysicalPosition, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

mod cp77;
mod d2r;
mod inbox;
mod isaac;
mod openmw;
mod omwlog;
mod specs;
mod install;
mod tls;
mod update;
mod ror2;
mod s2;
mod sacred;
mod saves;
mod scan;
mod w3;
mod xbox360;

const DEFAULT_SERVER: &str = "https://bigbacklog.online";
const POLL_SECS: u64 = 45;
/// Плашка опыта — с 5% стоимости текущего уровня: порог отдаёт сервер
/// (toast_min_xp), это — на случай сервера без него. Мелочь копится до
/// следующего раза, как у попапа сайта.
const MIN_TOAST_XP: i64 = 10;
/// После выхода из игры прирост показывается любой — видно, что сессия
/// засчиталась. Окно — если опыт за неё доехал не первым опросом.
const AFTER_GAME_SECS: i64 = 300;
/// Точка отсчёта прироста (poll.json рядом с config.json) переживает
/// перезапуск агента, но после суток простоя начинается заново.
const BASELINE_MAX_AGE_SECS: i64 = 24 * 3600;
/// Окно плашки в логических пикселях: сама плашка (366×66 у опыта, 460×98 у
/// квеста, 430×86 у сессии) плюс место под тень и выезд снизу.
const TOAST_W: f64 = 560.0;
const TOAST_H: f64 = 210.0;
/// Страховка: окно, чей JS не сообщил о конце показа, сносится само.
/// Насколько плашка выше низа рабочей области, доля ее высоты (place_toast).
const TOAST_LIFT: f64 = 0.05;
const TOAST_WATCHDOG: Duration = Duration::from_secs(40);
/// Урезанный WebView2 на время плашки. Замер (private working set дерева
/// процессов во время показа): по умолчанию 104 МБ, с этим набором ~88–91,
/// без GPU вовсе 77 — но тогда анимацию считает процессор, а он в игре
/// нужнее. Первые три фичи — те, что Tauri выключает сам: свой набор
/// аргументов его заменяет. Набор ОБЯЗАН быть одним у всех окон: WebView2 не
/// открывает второе окно с другими аргументами в той же папке данных.
const WEBVIEW_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
SpareRendererForSitePerProcess,CalculateNativeWinOcclusion --in-process-gpu --renderer-process-limit=1 \
--disable-background-networking --disable-extensions --no-first-run --autoplay-policy=no-user-gesture-required";
/// Вид плашек с сервера: файл → чем он обязан быть (content-type). Проверка
/// обновлений — раз в ASSETS_TTL, по ETag: не менялось — пустой ответ 304.
const POPUP_ASSETS: [(&str, &str); 2] = [("popups.js", "javascript"), ("popups.css", "text/css")];
const ASSETS_TTL: Duration = Duration::from_secs(600);
/// Строки последних сессий в меню трея — сразу под «Сейчас» (позиция в меню).
const RECENT_AT: usize = 2;
/// Сколько итог сессии ждет квестов перед показом одной плашкой (present_loop).
const SUMMARY_WAIT_MS: i64 = 4000;
/// Сборка для владельца: тестовые плашки и оверлей в меню, обновления из канала dev.
/// `npx tauri build --features dev` (scripts/companion_release.py --dev).
pub(crate) const DEV: bool = cfg!(feature = "dev");
const RECENT_MENU: usize = 3;

// ---------- Слежение за играми (см. scan.rs) ----------
/// Снимок процессов — меньше миллисекунды, чаще не нужно.
const SCAN_EVERY: Duration = Duration::from_secs(5);
/// Сердцебиение сессий на сервер (и сразу — при выходе из игры).
const REPORT_EVERY: Duration = Duration::from_secs(60);
/// Каталог установленных игр перечитывается — поставил игру, агент узнает.
const CATALOG_TTL: Duration = Duration::from_secs(600);
/// Установленные GOG/Battle.net — на полки сайта: когда список поменялся, и раз в сутки на всякий случай.
const INVENTORY_TTL: Duration = Duration::from_secs(24 * 3600);
// Правила «простоя» у агента нет сознательно: время идёт, пока игра запущена.
// Защита от игры, висящей фоном, — на сервере (потолок опыта за сутки и
// потолок сессии), а ввод агент честно не видит: геймпад Windows за ввод не
// считает (так решено 13.09.2026).
/// Сайт не ответил на отчёт — столько ждём его итога после выхода из игры,
/// прежде чем показать свой (без обложки и полоски HLTB). Перезапуск сайта
/// отвечает заглушкой 503 и укладывается в это окно (замер 17.09: 33 с).
const LOCAL_SUMMARY_WAIT_SECS: i64 = 60;
/// Процесс пропал на столько — сессия кончилась (переживает перезапуск игры).
/// Это же и задержка плашек после выхода: было 20 с — «прилетают долго»
/// (пользователь, 14.09); 10 с при снимке раз в 5 с — плашки через 10–15 с.
const END_AFTER_SECS: i64 = 10;
/// Ачивка Steam: Steam переписал файл статистики идущей игры — агент просит
/// сервер забрать её ачивки сразу (/api/agent/steam-sync), но не чаще этого
/// на игру: файл пишется и при обычной статистике, не только при ачивке.
const STEAM_NUDGE_GAP: Duration = Duration::from_secs(45);

/// Плагин Decky на этом ПК, пока у него выключено своё слежение за играми,
/// раз в 5 с отмечается файлом alive (decky/main.py, _relay_loop). Старше
/// этого — плагина нет, плашки рисует агент.
const RELAY_ALIVE_MAX: Duration = Duration::from_secs(15);
/// «Сейчас» для плагина (write_now) переписывается хотя бы так часто.
const NOW_REFRESH: Duration = Duration::from_secs(30);

// ---------- Конфиг ----------

#[derive(Serialize, Deserialize, Clone, Default)]
struct Config {
    #[serde(default)]
    server: String,
    #[serde(default)]
    token: String,
    /// Адрес прошитого Xbox 360 в домашней сети (xbdm, см. xbox360.rs).
    /// Пусто — за приставкой не следим.
    #[serde(default)]
    xbox360: String,
    /// Свои папки сохранений игр (окно «Сохранения игр»): id игры → путь.
    #[serde(default)]
    save_dirs: std::collections::HashMap<String, String>,
}

/// Портативный режим: config.json рядом с exe — токен (и копия вида плашек)
/// живут в той же папке, её можно унести на другой ПК целиком. Иначе —
/// %APPDATA%\BigBacklogAgent.
fn config_path() -> PathBuf {
    let portable = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("config.json")));
    if let Some(path) = portable.filter(|p| p.is_file()) {
        return path;
    }
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("BigBacklogAgent").join("config.json")
}

fn load_config() -> Config {
    let cfg: Config = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    saves::set_dirs(&cfg.save_dirs);
    cfg
}

fn write_config(cfg: &Config) -> std::io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg).unwrap_or_default())
}

fn server_of(cfg: &Config) -> String {
    let s = cfg.server.trim().trim_end_matches('/');
    if s.is_empty() { DEFAULT_SERVER.into() } else { s.into() }
}

// ---------- Сервер ----------

enum FetchError {
    Auth(String),
    Other(String),
}

impl FetchError {
    fn message(self) -> String {
        match self {
            FetchError::Auth(m) | FetchError::Other(m) => m,
        }
    }
}

fn http() -> ureq::Agent {
    tls::agent().timeout(Duration::from_secs(15)).build()
}

const USER_AGENT: &str = concat!("BigBacklogAgent/", env!("CARGO_PKG_VERSION"));

fn fetch_state(server: &str, token: &str) -> Result<Value, FetchError> {
    read_response(
        http()
            .get(&format!("{server}/api/agent/state"))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json")
            .set("User-Agent", USER_AGENT)
            .call(),
    )
}

fn post_json(server: &str, token: &str, path: &str, body: &Value) -> Result<Value, FetchError> {
    read_response(
        http()
            .post(&format!("{server}{path}"))
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .send_json(body.clone()),
    )
}

fn read_response(res: Result<ureq::Response, ureq::Error>) -> Result<Value, FetchError> {
    match res {
        Ok(resp) => resp.into_json::<Value>().map_err(|e| FetchError::Other(format!("ответ не разобрать: {e}"))),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp
                .into_json::<Value>()
                .ok()
                .and_then(|v| v.get("detail").and_then(|d| d.as_str()).map(String::from))
                .unwrap_or_else(|| format!("сервер ответил {code}"));
            if code == 401 || code == 403 { Err(FetchError::Auth(detail)) } else { Err(FetchError::Other(detail)) }
        }
        Err(_) => Err(FetchError::Other("нет связи с сервером".into())),
    }
}

// ---------- Вид плашек (static/popups.* с сервера) ----------

type Assets = HashMap<String, (String, String)>; // файл → (текст, ETag)

fn assets_dir() -> PathBuf {
    config_path().parent().map(|p| p.join("popups")).unwrap_or_else(|| PathBuf::from("popups"))
}

fn load_assets() -> Assets {
    let dir = assets_dir();
    let etags: HashMap<String, String> = std::fs::read_to_string(dir.join("etags.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut out = Assets::new();
    for (name, _) in POPUP_ASSETS {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            out.insert(name.to_string(), (text, etags.get(name).cloned().unwrap_or_default()));
        }
    }
    out
}

fn save_assets(assets: &Assets) {
    let dir = assets_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut etags = serde_json::Map::new();
    for (name, (text, etag)) in assets {
        let _ = std::fs::write(dir.join(name), text);
        etags.insert(name.clone(), json!(etag));
    }
    let _ = std::fs::write(dir.join("etags.json"), Value::Object(etags).to_string());
}

/// Свежий вид плашек с сервера. Берётся только настоящий JS/CSS: сервер без
/// этих файлов (старая версия) увёл бы на страницу входа, и её HTML лёг бы
/// вместо стилей. Нет связи — остаётся копия на диске.
fn refresh_popup_assets(s: &Shared, server: &str) {
    let mut changed = false;
    for (name, kind) in POPUP_ASSETS {
        let etag = s.assets.lock().unwrap().get(name).map(|a| a.1.clone()).unwrap_or_default();
        let mut req = http().get(&format!("{server}/{name}")).set("User-Agent", USER_AGENT);
        if !etag.is_empty() {
            req = req.set("If-None-Match", &etag);
        }
        let Ok(resp) = req.call() else { continue };
        if resp.status() != 200 || !resp.content_type().contains(kind) {
            continue; // 304 — не менялось
        }
        let new_etag = resp.header("ETag").unwrap_or("").to_string();
        let Ok(text) = resp.into_string() else { continue };
        s.assets.lock().unwrap().insert(name.to_string(), (text, new_etag));
        changed = true;
    }
    if changed {
        save_assets(&s.assets.lock().unwrap());
    }
    *s.assets_at.lock().unwrap() = Some(Instant::now());
}

// ---------- Состояние ----------

/// Строки последних сессий в меню трея. Пунктов меню три, но в меню стоят
/// только те, для которых есть сессия (shown): лишние вынимаются.
#[derive(Default)]
struct RecentMenu {
    menu: Option<Menu<tauri::Wry>>,
    items: Vec<MenuItem<tauri::Wry>>,
    shown: usize,
    texts: Vec<String>,
    links: Vec<String>,
    /// Сессии как их отдал сервер — обложка для «Тест: сессия».
    sessions: Vec<Value>,
}

struct Shared {
    cfg: Mutex<Config>,
    /// Прошлый ответ сервера — прирост считается от него.
    baseline: Mutex<Option<Value>>,
    /// id квестов «можно сдать», о которых уже сказали. None — ещё не было
    /// первого ответа: всё, что готово на старте агента, считается известным.
    seen_quests: Mutex<Option<HashSet<i64>>>,
    /// До какого момента (unix) прирост опыта показывается любым: игра только
    /// что кончилась (AFTER_GAME_SECS).
    after_game_until: AtomicI64,
    pending: Mutex<VecDeque<Value>>,
    current: Mutex<Option<Value>>,
    busy: AtomicBool,
    /// Когда (unix, мс) с экрана ушла последняя плашка: обновление ждет после нее тишины.
    last_toast_ms: AtomicI64,
    paused: AtomicBool,
    seq: AtomicU64,
    wake: (Mutex<bool>, Condvar),
    status_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    app: Mutex<Option<AppHandle>>,
    /// Запуск с --demo: тестовые плашки после первого опроса (к этому
    /// времени уже есть настоящий значок уровня и вид плашек).
    demo: AtomicBool,
    tracker: Mutex<Tracker>,
    /// Слежение за играми включено (пункт меню трея).
    tracking: AtomicBool,
    now_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// Пункт «Подключение…» и есть ли он сейчас в меню: после подключения он не нужен,
    /// возвращается, когда подключения нет или его отозвали (set_connected).
    connect_item: Mutex<Option<(Menu<tauri::Wry>, MenuItem<tauri::Wry>, bool)>>,
    recent: Mutex<RecentMenu>,
    assets: Mutex<Assets>,
    /// Когда вид плашек последний раз сверялся с сервером.
    assets_at: Mutex<Option<Instant>>,
}

impl Shared {
    fn new(cfg: Config) -> Self {
        let (baseline, seen_quests) = load_poll();
        Shared {
            cfg: Mutex::new(cfg),
            baseline: Mutex::new(baseline),
            seen_quests: Mutex::new(seen_quests),
            after_game_until: AtomicI64::new(0),
            pending: Mutex::new(VecDeque::new()),
            current: Mutex::new(None),
            busy: AtomicBool::new(false),
            last_toast_ms: AtomicI64::new(0),
            paused: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            wake: (Mutex::new(false), Condvar::new()),
            status_item: Mutex::new(None),
            app: Mutex::new(None),
            demo: AtomicBool::new(std::env::args().any(|a| a == "--demo")),
            tracker: Mutex::new(Tracker::default()),
            tracking: AtomicBool::new(true),
            now_item: Mutex::new(None),
            connect_item: Mutex::new(None),
            recent: Mutex::new(RecentMenu::default()),
            assets: Mutex::new(load_assets()),
            assets_at: Mutex::new(None),
        }
    }

    fn wake_poll(&self) {
        let (lock, cv) = &self.wake;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }

    // Правило для всех set_*: замок только на чтение/запись своих полей, сами вызовы меню и трея — уже
    // без него. Меню меняется через главный поток с ожиданием; держи мы замок, клик по тому же пункту
    // меню (главный поток ждет замок) замыкал бы их друг на друга, и Windows снимала зависшую программу
    // (так было 09.10 в 01:18 у dev-сборки на ПК).
    fn set_now(&self, text: &str) {
        let item = self.now_item.lock().unwrap().clone();
        if let Some(item) = item {
            ui_step("строка «Сейчас»");
            let _ = item.set_text(menu_text(text));
        }
    }

    /// Подключено ли приложение: «Подключение…» в меню только без подключения. Встает перед
    /// «Сохранения игр…» (строки последних сессий выше, их число меняется).
    fn set_connected(&self, on: bool) {
        let (menu, item) = {
            let mut slot = self.connect_item.lock().unwrap();
            let Some((menu, item, shown)) = slot.as_mut() else { return };
            if *shown != on {
                return;
            }
            *shown = !on;
            (menu.clone(), item.clone())
        };
        ui_step("пункт «Подключение…»");
        if on {
            let _ = menu.remove(&item);
        } else {
            let at = menu.items().ok()
                .and_then(|all| all.iter().position(|k| k.id().as_ref() == "saves"))
                .unwrap_or(0);
            let _ = menu.insert(&item, at);
        }
    }

    fn set_status(&self, text: &str) {
        let item = self.status_item.lock().unwrap().clone();
        if let Some(item) = item {
            ui_step("строка статуса");
            let _ = item.set_text(text);
        }
        let app = self.app.lock().unwrap().clone();
        if let Some(app) = app {
            ui_step("подсказка значка");
            if let Some(tray) = app.tray_by_id("tray") {
                let dev = if DEV { " dev" } else { "" };
                let _ = tray.set_tooltip(Some(format!("BigBacklog Companion {}{dev} · {text}", update::VERSION)));
            }
        }
    }

    /// Последние сессии — строками меню под «Сейчас». Меню трогается, только
    /// когда строки поменялись (опрос раз в 45 с).
    fn set_recent(&self, sessions: &[Value]) {
        let list: Vec<&Value> = sessions.iter().take(RECENT_MENU).collect();
        let texts: Vec<String> = list.iter().map(|v| session_line(v)).collect();
        let (menu, items, was_shown) = {
            let mut r = self.recent.lock().unwrap();
            r.sessions = sessions.to_vec();
            r.links = list.iter().map(|v| v.get("link").and_then(|l| l.as_str()).unwrap_or("").to_string()).collect();
            if r.texts == texts {
                return;
            }
            let Some(menu) = r.menu.clone() else { return };
            let was_shown = r.shown;
            r.shown = texts.len();
            r.texts = texts.clone();
            (menu, r.items.clone(), was_shown)
        };
        ui_step("строки последних сессий");
        for (i, item) in items.iter().enumerate() {
            if i < texts.len() {
                let _ = item.set_text(menu_text(&texts[i]));
                if i >= was_shown {
                    let _ = menu.insert(item, RECENT_AT + i);
                }
            } else if i < was_shown {
                let _ = menu.remove(item);
            }
        }
    }
}

// ---------- Сторож главного потока ----------

/// Что последним просили у главного потока (меню, окно плашки): попадет в журнал, если он зависнет.
static UI_STEP: Mutex<&'static str> = Mutex::new("старт");

fn ui_step(what: &'static str) {
    *UI_STEP.lock().unwrap() = what;
}

/// Раз в WATCH_EVERY шлет главному потоку пустое дело и ждет, что тот его выполнит. Молчит больше
/// WATCH_LOG — пишет в журнал, на каком шаге (ui_step); больше WATCH_RESTART — перезапускает
/// приложение (новая копия ждет выхода этой по --wait-pid). Время считается тиками по секунде, а не
/// часами: после сна компьютера ложного «зависания» не будет.
const WATCH_EVERY: u32 = 15;
const WATCH_LOG: u32 = 20;
const WATCH_RESTART: u32 = 60;

fn main_watchdog(app: AppHandle) {
    loop {
        std::thread::sleep(Duration::from_secs(WATCH_EVERY as u64));
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        if app.run_on_main_thread(move || flag.store(true, Ordering::SeqCst)).is_err() {
            return; // приложение закрывается
        }
        let mut waited = 0;
        while !done.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
            waited += 1;
            if waited == WATCH_LOG {
                log(&format!("главный поток не отвечает {WATCH_LOG} с, последний шаг: {}", *UI_STEP.lock().unwrap()));
            }
            if waited >= WATCH_RESTART {
                log(&format!("главный поток завис ({}), перезапускаю приложение", *UI_STEP.lock().unwrap()));
                if let Ok(me) = std::env::current_exe() {
                    let _ = std::process::Command::new(me)
                        .arg("--wait-pid")
                        .arg(std::process::id().to_string())
                        .spawn();
                }
                std::process::exit(1);
            }
        }
    }
}

fn num(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

/// «&» в пункте меню Windows — подчёркнутая буква быстрого доступа:
/// «Ratchet & Clank» потерял бы амперсанд.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

/// Длительность как на сайте: «1 ч 25 мин», «45 мин», «2 ч».
fn duration(minutes: i64) -> String {
    let (h, m) = (minutes.max(0) / 60, minutes.max(0) % 60);
    match (h, m) {
        (0, m) => format!("{m} мин"),
        (h, 0) => format!("{h} ч"),
        (h, m) => format!("{h} ч {m} мин"),
    }
}

const MONTHS: [&str; 12] = ["янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"];

/// День по местному времени: «сегодня», «вчера», «12 сен».
fn day_label(ts: i64) -> String {
    let Some(t) = win::local_time(ts) else { return String::new() };
    let now = unix_now();
    let same_day = |other: Option<win::LocalTime>| other.map(|o| o.date() == t.date()).unwrap_or(false);
    if same_day(win::local_time(now)) {
        "сегодня".into()
    } else if same_day(win::local_time(now - 86_400)) {
        "вчера".into()
    } else {
        format!("{} {}", t.day, MONTHS[(t.month as usize).clamp(1, 12) - 1])
    }
}

fn clock(ts: i64) -> String {
    win::local_time(ts).map(|t| format!("{:02}:{:02}", t.hour, t.minute)).unwrap_or_default()
}

/// Строка меню: «Crimson Desert · 2 ч 15 мин · вчера 23:10–01:25». День — по
/// началу (ночная игра остаётся вчерашней, как в ленте); без начала —
/// только день.
fn session_line(v: &Value) -> String {
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
    let name = if name.chars().count() > 38 {
        format!("{}…", name.chars().take(37).collect::<String>().trim_end())
    } else {
        name.to_string()
    };
    let end = int(v, "ended_at");
    let when = match v.get("started_at").and_then(|x| x.as_i64()) {
        Some(start) => format!("{} {}–{}", day_label(start), clock(start), clock(end)),
        None => day_label(end),
    };
    format!("{name} · {} · {when}", duration(int(v, "minutes")))
}

/// Кусок состояния, который нужен кадру плашки: уровень, полоска, значок.
fn level_payload(v: &Value) -> Value {
    json!({
        "level": v.get("level"), "xp": v.get("xp"), "into_level": v.get("into_level"),
        "need": v.get("need"), "max_level": v.get("max_level"), "badge": v.get("badge"),
    })
}

// ---------- Точка отсчёта прироста на диске ----------
// Прошлый ответ сервера и квесты, о которых уже сказали, лежат в poll.json
// рядом с config.json: перезапуск агента (обновление, перезагрузка ПК) не
// съедает плашку опыта, который пришёл в этот момент. Пишется, только когда
// они меняются; старше суток — не в счёт.

fn poll_path() -> PathBuf {
    config_path().with_file_name("poll.json")
}

/// Журнал агента (agent.log рядом с config.json): игры, отчёты сайту, итоги,
/// пауза, куда ушла каждая плашка — «плашка не пришла» разбирается по
/// записям, а не догадками. Больше LOG_MAX — старое уезжает в agent.log.old.
const LOG_MAX: u64 = 512 * 1024;

fn log(msg: &str) {
    let path = config_path().with_file_name("agent.log");
    if std::fs::metadata(&path).map(|m| m.len() > LOG_MAX).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let ts = win::local_time(unix_now())
        .map(|t| format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second))
        .unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{ts} {msg}");
    }
}

fn load_poll() -> (Option<Value>, Option<HashSet<i64>>) {
    let Some(v) = std::fs::read_to_string(poll_path()).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else {
        return (None, None);
    };
    if unix_now() - int(&v, "at") > BASELINE_MAX_AGE_SECS {
        return (None, None);
    }
    let baseline = v.get("baseline").filter(|b| b.is_object()).cloned();
    let seen = v.get("seen_quests").and_then(|q| q.as_array()).map(|a| a.iter().filter_map(|x| x.as_i64()).collect());
    (baseline, seen)
}

fn save_poll(s: &Shared) {
    let baseline = s.baseline.lock().unwrap().clone();
    let seen: Option<Vec<i64>> = s.seen_quests.lock().unwrap().as_ref().map(|set| {
        let mut ids: Vec<i64> = set.iter().copied().collect();
        ids.sort_unstable();
        ids
    });
    let body = json!({"at": unix_now(), "baseline": baseline, "seen_quests": seen});
    let path = poll_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Ответ сервера для точки отсчёта — без списков сессий и квестов.
fn slim(state: &Value) -> Value {
    let mut v = state.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("recent_sessions");
        o.remove("quests_ready");
    }
    v
}

fn on_state(s: &Shared, state: Value) {
    xbox360::nudge_from_state(&state);
    saves::set_flags_from_state(&state);
    let right = if state.get("max_level").and_then(|m| m.as_bool()).unwrap_or(false) {
        "MAX".to_string()
    } else {
        format!("{} / {} XP", num(int(&state, "into_level")), num(int(&state, "need")))
    };
    s.set_status(&format!("уровень {} · {right}", int(&state, "level")));
    if let Some(recent) = state.get("recent_sessions").and_then(|r| r.as_array()) {
        s.set_recent(recent);
    }

    let paused = s.paused.load(Ordering::SeqCst);
    let mut out: Vec<Value> = Vec::new();
    // Точка отсчёта или список квестов поменялись — на диск (save_poll).
    let mut dirty = false;

    {
        let mut base = s.baseline.lock().unwrap();
        let same_user = base.as_ref().map(|b| b.get("user_id") == state.get("user_id")).unwrap_or(false);
        if !same_user {
            *base = Some(slim(&state));
            dirty = true;
        } else {
            let prev = base.clone().unwrap();
            let gained = int(&state, "xp") - int(&prev, "xp");
            let levelled = int(&state, "level") > int(&prev, "level");
            let site = int(&state["sources"], "site") - int(&prev["sources"], "site");
            let min = state.get("toast_min_xp").and_then(|v| v.as_i64()).filter(|v| *v > 0).unwrap_or(MIN_TOAST_XP);
            let after_game = unix_now() < s.after_game_until.load(Ordering::SeqCst);
            if gained < 0 {
                *base = Some(slim(&state)); // опыт отняли отладкой — новая точка отсчёта
                dirty = true;
            } else if gained > 0 {
                if gained - site.max(0) <= 0 && !levelled {
                    // Опыт за время на сайте показывает сам сайт.
                    *base = Some(slim(&state));
                    dirty = true;
                } else if gained >= min || levelled || after_game {
                    *base = Some(slim(&state));
                    dirty = true;
                    s.after_game_until.store(0, Ordering::SeqCst); // показали — дальше обычный порог
                    log(&format!("опыт +{gained}{}", if paused { " — плашки на паузе, не показываю" } else { "" }));
                    if !paused {
                        out.push(json!({
                            "type": "xp", "gained": gained,
                            "from": level_payload(&prev), "to": level_payload(&state),
                        }));
                    }
                }
            }
        }
    }

    {
        let ready = state.get("quests_ready").and_then(|q| q.as_array()).cloned().unwrap_or_default();
        let mut seen = s.seen_quests.lock().unwrap();
        match seen.as_mut() {
            None => {
                *seen = Some(ready.iter().map(|q| int(q, "id")).collect());
                dirty = true;
            }
            Some(set) => {
                for q in ready {
                    if set.insert(int(&q, "id")) {
                        dirty = true;
                        log(&format!("квест {} можно сдать{}", int(&q, "id"), if paused { " — плашки на паузе" } else { "" }));
                        if !paused {
                            out.push(json!({"type": "quest", "quest": q}));
                        }
                    }
                }
            }
        }
    }

    if dirty {
        save_poll(s);
    }

    if !out.is_empty() {
        s.pending.lock().unwrap().extend(out);
    }
}

/// Следующая плашка. Подряд идущие приросты опыта (накопились за
/// полноэкранную игру) склеиваются в одну: от уровня до первого — к уровню
/// после последнего.
fn take_next(s: &Shared, merge: bool) -> Option<Value> {
    let mut q = s.pending.lock().unwrap();
    let mut first = q.pop_front()?;
    // Итог после игры одной плашкой (buildSummaryPopup в static/popups.js): к сессии подтягиваются
    // весь опыт и все выполненные квесты, что ждут в очереди. Не для плагина Decky (merge false):
    // он такую плашку не знает.
    if merge && first["type"] == "session" && first.get("test").is_none() {
        let mut xp: Option<Value> = None;
        let mut quests: Vec<Value> = Vec::new();
        q.retain(|d| {
            if d.get("test").is_some() {
                return true;
            }
            if d["type"] == "xp" {
                xp = Some(match xp.take() {
                    None => d.clone(),
                    Some(acc) => merge_xp(&acc, d),
                });
                return false;
            }
            if d["type"] == "quest" {
                quests.push(d["quest"].clone());
                return false;
            }
            true
        });
        if xp.is_some() || !quests.is_empty() {
            return Some(json!({"type": "summary", "session": first["session"], "xp": xp, "quests": quests}));
        }
        return Some(first);
    }
    if first["type"] == "xp" && first.get("test").is_none() {
        while q.front().map(|n| n["type"] == "xp" && n.get("test").is_none()).unwrap_or(false) {
            let next = q.pop_front().unwrap();
            first["gained"] = json!(int(&first, "gained") + int(&next, "gained"));
            first["to"] = next["to"].clone();
        }
    }
    Some(first)
}

/// Плашка приложения о себе (buildNoticePopup в static/popups.js): обновилось, обновлений нет.
pub(crate) fn push_notice(s: &Shared, label: &str, name: &str, sub: &str) {
    s.pending.lock().unwrap().push_back(json!({"type": "notice", "notice": {
        "label": label, "name": name, "sub": sub,
    }}));
}

/// После самообновления: «Приложение обновлено». Ждем, пока с сервера придет вид плашки, который
/// ее умеет (popups.js на диске мог остаться от прошлой версии сайта), но не дольше двух минут.
fn notice_updated(s: Arc<Shared>, from: String) {
    for _ in 0..120 {
        let ready = s.assets.lock().unwrap().get("popups.js").map(|a| a.0.contains("buildNoticePopup")).unwrap_or(false);
        if ready {
            push_notice(&s, "Приложение обновлено", &format!("BigBacklog Companion {}", update::VERSION),
                        &format!("было {from}"));
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    log("обновление: вид плашки без buildNoticePopup, плашку «обновлено» не показали");
}

/// Два начисления опыта одним: сумма, «было» раньшее из двух, «стало» позднее.
fn merge_xp(a: &Value, b: &Value) -> Value {
    let key = |v: &Value| (int(v, "level"), int(v, "into_level"));
    let from = if key(&a["from"]) <= key(&b["from"]) { a["from"].clone() } else { b["from"].clone() };
    let to = if key(&a["to"]) >= key(&b["to"]) { a["to"].clone() } else { b["to"].clone() };
    json!({"type": "xp", "gained": int(a, "gained") + int(b, "gained"), "from": from, "to": to})
}

fn push_test(s: &Shared, kind: &str) {
    let base = s.baseline.lock().unwrap().clone().unwrap_or_else(|| {
        json!({"level": 23, "xp": 5000, "into_level": 1340, "need": 1900, "max_level": false})
    });
    let to = level_payload(&base);
    let into = int(&to, "into_level");
    let data = match kind {
        "level" => {
            let mut from = to.clone();
            let need = int(&to, "need");
            from["level"] = json!(int(&to, "level") - 1);
            from["into_level"] = json!((need - 60).max(0));
            json!({"type": "xp", "test": true, "gained": 60 + into, "from": from, "to": to})
        }
        "summary" => {
            // Итог после игры одной плашкой: сессия, опыт с полоской, два квеста.
            let last = s.recent.lock().unwrap().sessions.first().cloned().unwrap_or(Value::Null);
            let pick = |key: &str| last.get(key).cloned().unwrap_or(Value::Null);
            let mut from = to.clone();
            from["into_level"] = json!((into - 211).max(0));
            json!({"type": "summary", "test": true,
                "session": {
                    "name": last.get("name").and_then(|n| n.as_str()).unwrap_or("Hollow Knight"),
                    "platform": pick("platform"), "cover": pick("cover"), "orb": pick("orb"),
                    "cover_pos_x": pick("cover_pos_x"), "cover_pos_y": pick("cover_pos_y"),
                    "cover_scale": pick("cover_scale"),
                    "minutes": 85, "before_minutes": 744, "after_minutes": 829,
                    "hltb_hours": 40.5, "pct_before": 30.6, "pct_after": 34.1,
                },
                "xp": {"gained": 211, "from": from, "to": to},
                "quests": [
                    {"title": "Выбей <i>3 достижения</i> в Hollow Knight", "reward_xp": 180},
                    {"title": "Сыграй <i>2 часа</i> в игру из бэклога", "reward_xp": 120},
                ],
            })
        }
        "quest" => json!({"type": "quest", "test": true, "quest": {
            "id": 0, "kind": "ach_n", "glyph": "🎯", "reward_xp": 180, "game": "Hollow Knight",
            "title": "Выбей <i>3 достижения</i> в Hollow Knight",
        }}),
        "session" => {
            // Обложка и название — последней настоящей сессии, цифры выдуманы.
            let last = s.recent.lock().unwrap().sessions.first().cloned().unwrap_or(Value::Null);
            let pick = |key: &str| last.get(key).cloned().unwrap_or(Value::Null);
            json!({"type": "session", "test": true, "session": {
                "name": last.get("name").and_then(|n| n.as_str()).unwrap_or("Hollow Knight"),
                "platform": pick("platform"), "cover": pick("cover"), "orb": pick("orb"),
                "cover_pos_x": pick("cover_pos_x"), "cover_pos_y": pick("cover_pos_y"),
                "cover_scale": pick("cover_scale"),
                "minutes": 85, "before_minutes": 744, "after_minutes": 829,
                "hltb_hours": 40.5, "pct_before": 30.6, "pct_after": 34.1,
            }})
        }
        _ => {
            let mut from = to.clone();
            from["into_level"] = json!((into - 45).max(0));
            json!({"type": "xp", "test": true, "gained": 45, "from": from, "to": to})
        }
    };
    s.pending.lock().unwrap().push_back(data);
}

// ---------- Win32 ----------

mod win {
    use windows_sys::Win32::Foundation::{FILETIME, HWND, RECT, SYSTEMTIME};
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, ShellExecuteW, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetForegroundWindow, GetSystemMetrics, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        GWL_EXSTYLE, HWND_TOPMOST, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    /// Тот же сигнал, по которому Windows сама глушит уведомления в играх.
    pub fn exclusive_fullscreen() -> bool {
        let mut state = 0;
        let ok = unsafe { SHQueryUserNotificationState(&mut state) } == 0;
        ok && (state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE)
    }

    /// Открыт Steam Big Picture: его окно так и называется на любом языке
    /// клиента. Пока идёт игра, запущенная из него, окно живо позади неё.
    pub fn big_picture_open() -> bool {
        let title: Vec<u16> = "Steam Big Picture Mode".encode_utf16().chain(std::iter::once(0)).collect();
        !unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) }.is_null()
    }

    /// Рабочая область монитора, где активное окно (там и игра).
    pub fn active_work_area() -> (i32, i32, i32, i32) {
        unsafe {
            let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
                let RECT { left, top, right, bottom } = info.rcWork;
                return (left, top, right, bottom);
            }
            (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        }
    }

    /// Показать окно поверх всех, НЕ активируя: игра фокус не теряет.
    pub fn show_no_activate(hwnd: HWND) {
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }

    pub fn open_url(url: &str) {
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (verb, target) = (wide("open"), wide(url));
        unsafe {
            ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
        }
    }

    #[derive(Clone, Copy)]
    pub struct LocalTime {
        pub year: u16,
        pub month: u16,
        pub day: u16,
        pub hour: u16,
        pub minute: u16,
        pub second: u16,
    }

    impl LocalTime {
        pub fn date(&self) -> (u16, u16, u16) {
            (self.year, self.month, self.day)
        }
    }

    /// Unix-секунды → местное время по часовому поясу Windows (с летним
    /// временем, действовавшим в ту дату).
    pub fn local_time(ts: i64) -> Option<LocalTime> {
        let ticks = u64::try_from(ts.checked_add(11_644_473_600)?.checked_mul(10_000_000)?).ok()?;
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        unsafe {
            let mut utc: SYSTEMTIME = std::mem::zeroed();
            let mut local: SYSTEMTIME = std::mem::zeroed();
            if FileTimeToSystemTime(&ft, &mut utc) == 0
                || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
            {
                return None;
            }
            Some(LocalTime {
                year: local.wYear, month: local.wMonth, day: local.wDay, hour: local.wHour, minute: local.wMinute,
                second: local.wSecond,
            })
        }
    }
}

fn raw_hwnd(w: &WebviewWindow) -> Option<windows_sys::Win32::Foundation::HWND> {
    w.hwnd().ok().map(|h| h.0 as windows_sys::Win32::Foundation::HWND)
}

// ---------- Плашки ----------

/// Папка, куда плагин Decky ждёт плашки (его DECKY_PLUGIN_RUNTIME_DIR).
fn relay_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    Some(PathBuf::from(home).join("homebrew").join("data").join("BigBacklog").join("relay"))
}

/// Папка плагина, если он жив (свежий alive).
fn relay_alive() -> Option<PathBuf> {
    let dir = relay_dir()?;
    let beat = std::fs::metadata(dir.join("alive")).ok()?.modified().ok()?;
    let fresh = SystemTime::now().duration_since(beat).map(|d| d < RELAY_ALIVE_MAX).unwrap_or(true);
    fresh.then_some(dir)
}

/// Кому показывать плашку: открыт Big Picture и плагин жив — плагину (его
/// папка), иначе None — окном агента. Уведомление Steam видно и поверх игры
/// (оверлей Steam), даже в эксклюзивном полноэкранном режиме.
fn relay_target() -> Option<PathBuf> {
    relay_alive().filter(|_| win::big_picture_open())
}

/// Что идёт сейчас — плагину Decky: строка под уровнем в его меню (своё
/// слежение у плагина на ПК выключено). Файл рядом с папкой плашек, а не в
/// ней: оттуда плагин забирает и удаляет каждый .json. Пишется, когда
/// поменялись игры или минуты, и не реже NOW_REFRESH — по его «at» плагин
/// понимает, что агент жив.
fn write_now(s: &Shared, last: &mut Option<(String, Instant)>) {
    let Some(path) = relay_alive().and_then(|dir| dir.parent().map(|p| p.join("agent-now.json"))) else { return };
    let games: Vec<Value> = s.tracker.lock().unwrap().live.values()
        .map(|v| json!({
            "name": v.shown_name.clone().unwrap_or_else(|| v.info.name.clone()),
            "minutes": v.active_secs / 60, "resolved": v.resolved.unwrap_or(true),
            "source": v.info.source, "id": v.info.id,
        }))
        .collect();
    let body = Value::Array(games).to_string();
    if last.as_ref().map(|(b, at)| *b == body && at.elapsed() < NOW_REFRESH).unwrap_or(false) {
        return;
    }
    let text = format!("{{\"at\":{},\"games\":{body}}}", unix_now());
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        *last = Some((body, Instant::now()));
    }
}

/// Плашка плагину: файл целиком через переименование — полузаписанный он не
/// прочтёт. Имя — время в мс: плагин показывает по порядку имён.
fn relay(dir: &std::path::Path, data: &Value, seq: u64) -> bool {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let name = format!("{ms:015}-{seq:06}.json");
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, data.to_string()).is_ok() && std::fs::rename(&tmp, dir.join(&name)).is_ok()
}

fn present_loop(app: AppHandle, s: Arc<Shared>) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if s.pending.lock().unwrap().is_empty() {
            continue;
        }
        // Опыт, уровень и квесты не мешают игре: копятся и показываются, когда из нее вышли
        // (подряд идущий опыт take_next склеит в одну плашку). Тестовые и уведомления о самом
        // приложении идут сразу.
        let urgent = s.pending.lock().unwrap().front()
            .map(|d| d.get("test").is_some() || d["type"] == "notice")
            .unwrap_or(false);
        if !urgent && !s.tracker.lock().unwrap().live.is_empty() {
            continue;
        }
        if let Some(dir) = relay_target() {
            if let Some(data) = take_next(&s, false) {
                let kind = data["type"].as_str().unwrap_or("?").to_string();
                if relay(&dir, &data, s.seq.fetch_add(1, Ordering::SeqCst)) {
                    s.last_toast_ms.store(unix_ms(), Ordering::SeqCst);
                    log(&format!("плашка {kind} → плагину Decky"));
                } else {
                    log(&format!("плашка {kind}: плагину не записать, ещё раз"));
                    s.pending.lock().unwrap().push_front(data);
                }
            }
            continue;
        }
        if s.busy.load(Ordering::SeqCst) || win::exclusive_fullscreen() {
            continue;
        }
        // Итог после игры одной плашкой — если вид плашек с сервера ее уже умеет. Итог сессии чуть
        // ждет: выполненные квесты приходят следующим опросом, через секунду-другую после него.
        let merge = s.assets.lock().unwrap().get("popups.js").map(|a| a.0.contains("buildSummaryPopup")).unwrap_or(false);
        if merge {
            let fresh = s.pending.lock().unwrap().front()
                .map(|d| d["type"] == "session" && unix_ms() - d["at"].as_i64().unwrap_or(0) < SUMMARY_WAIT_MS)
                .unwrap_or(false);
            if fresh {
                continue;
            }
        }
        let Some(mut data) = take_next(&s, merge) else { continue };
        log(&format!("плашка {} → окно агента", data["type"].as_str().unwrap_or("?")));
        data["server"] = json!(server_of(&s.cfg.lock().unwrap()));
        *s.current.lock().unwrap() = Some(data);
        s.busy.store(true, Ordering::SeqCst);
        let label = format!("toast{}", s.seq.fetch_add(1, Ordering::SeqCst));
        ui_step("окно плашки");
        let built = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App("toast.html".into()))
            .title("Big Backlog")
            .additional_browser_args(WEBVIEW_ARGS)
            .inner_size(TOAST_W, TOAST_H)
            .maximizable(false)
            .transparent(true)
            .decorations(false)
            .shadow(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .visible(false)
            .build();
        match built {
            Ok(w) => {
                let _ = w.set_ignore_cursor_events(true);
                place_toast(&w);
                let (app2, s2) = (app.clone(), s.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(TOAST_WATCHDOG);
                    if let Some(w) = app2.get_webview_window(&label) {
                        let _ = w.destroy();
                        s2.last_toast_ms.store(unix_ms(), Ordering::SeqCst);
                        s2.busy.store(false, Ordering::SeqCst);
                    }
                });
            }
            Err(_) => s.busy.store(false, Ordering::SeqCst),
        }
    }
}

/// Нижняя треть по центру — там, где ачивки у Xbox 360.
fn place_toast(w: &WebviewWindow) {
    ui_step("место плашки");
    // Windows 11 на портативках с маленьким экраном (GPD5) открывает новые окна развернутыми на весь
    // экран, и плашка рисовалась у нижнего края экрана, под панелью задач, куда окно ни ставь.
    if w.is_maximized().unwrap_or(false) {
        let _ = w.unmaximize();
        let _ = w.set_size(tauri::LogicalSize::new(TOAST_W, TOAST_H));
    }
    let (left, top, right, bottom) = win::active_work_area();
    // Сразу после создания окно может еще не знать масштаб экрана и отдать размер без него (GPD5,
    // 175%: 210 px вместо 368, плашка уезжала под панель задач). Размер — заданный × масштаб, а
    // toast_show ставит окно еще раз, когда оно уже на экране.
    let scale = w.scale_factor().unwrap_or(1.0);
    let want = tauri::PhysicalSize { width: (TOAST_W * scale).round() as u32, height: (TOAST_H * scale).round() as u32 };
    let got = w.outer_size().unwrap_or(want);
    let size = tauri::PhysicalSize { width: got.width.max(want.width), height: got.height.max(want.height) };
    let x = left + ((right - left) - size.width as i32) / 2;
    let y = bottom - size.height as i32 - ((bottom - top) as f64 * TOAST_LIFT) as i32;
    let _ = w.set_position(PhysicalPosition::new(x, y));
}

#[tauri::command]
fn toast_data(state: tauri::State<'_, Arc<Shared>>) -> Value {
    state.current.lock().unwrap().clone().unwrap_or(Value::Null)
}

/// Вид плашек — static/popups.js и popups.css с сервера (копия на диске).
#[tauri::command]
fn toast_assets(state: tauri::State<'_, Arc<Shared>>) -> Value {
    let assets = state.assets.lock().unwrap();
    let text = |name: &str| assets.get(name).map(|a| a.0.clone()).unwrap_or_default();
    json!({"js": text("popups.js"), "css": text("popups.css")})
}

#[tauri::command]
fn toast_show(window: WebviewWindow) {
    place_toast(&window);
    if let Some(hwnd) = raw_hwnd(&window) {
        win::show_no_activate(hwnd);
    }
}

#[tauri::command]
fn toast_done(window: WebviewWindow, state: tauri::State<'_, Arc<Shared>>) {
    let shared = state.inner().clone();
    std::thread::spawn(move || {
        let _ = window.destroy();
        shared.last_toast_ms.store(unix_ms(), Ordering::SeqCst);
        shared.busy.store(false, Ordering::SeqCst);
    });
}

// ---------- Настройки подключения ----------

#[tauri::command]
fn get_config(state: tauri::State<'_, Arc<Shared>>) -> Config {
    let mut cfg = state.cfg.lock().unwrap().clone();
    if cfg.server.trim().is_empty() {
        cfg.server = DEFAULT_SERVER.into();
    }
    cfg
}

#[tauri::command(async)]
fn save_config(server: String, token: String, state: tauri::State<'_, Arc<Shared>>) -> Result<String, String> {
    // Адрес Xbox 360 в окне подключения не редактируется — сохраняем прежний.
    let prev = state.cfg.lock().unwrap().clone();
    let cfg = Config { server: server.trim().trim_end_matches('/').to_string(), token: token.trim().to_string(), ..prev };
    let v = fetch_state(&server_of(&cfg), &cfg.token).map_err(FetchError::message)?;
    write_config(&cfg).map_err(|e| format!("не сохранить конфиг: {e}"))?;
    *state.cfg.lock().unwrap() = cfg;
    *state.baseline.lock().unwrap() = None;
    *state.seen_quests.lock().unwrap() = None;
    save_poll(&state);
    *state.assets_at.lock().unwrap() = None; // другой сервер — свой вид плашек
    state.wake_poll();
    Ok(format!("Подключено: уровень {}", int(&v, "level")))
}

/// Привязка коротким кодом, как у плагина Деки (app/routers/agent.py, pair_*): агент заводит пару
/// без токена, человек вводит 4 символа на сайте, токен агент забирает опросом и сам сохраняет.
#[tauri::command(async)]
fn pair_start(server: String) -> Result<Value, String> {
    let server = server.trim().trim_end_matches('/').to_string();
    read_response(http().post(&format!("{server}/api/agent/pair/start")).set("User-Agent", USER_AGENT).call())
        .map_err(FetchError::message)
}

#[tauri::command(async)]
fn pair_poll(server: String, device_id: String, secret: String) -> Result<Value, String> {
    let server = server.trim().trim_end_matches('/').to_string();
    read_response(
        http()
            .post(&format!("{server}/api/agent/pair/poll"))
            .set("User-Agent", USER_AGENT)
            .send_json(serde_json::json!({ "device_id": device_id, "secret": secret })),
    )
    .map_err(FetchError::message)
}

#[tauri::command]
fn close_settings(window: WebviewWindow) {
    std::thread::spawn(move || {
        let _ = window.destroy();
    });
}

// ---------- Сохранения игр ----------

#[tauri::command]
fn get_saves() -> Value {
    saves::list(config_path().parent().unwrap_or(std::path::Path::new(".")))
}

/// Переключатель «Отправлять»: флаг на сервере (его же видит сайт), потом у себя.
#[tauri::command(async)]
fn set_save_sync(id: String, on: bool, state: tauri::State<'_, Arc<Shared>>) -> Result<Value, String> {
    let cfg = state.cfg.lock().unwrap().clone();
    let res = http()
        .post(&format!("{}/api/agent/save-sync/{id}", server_of(&cfg)))
        .set("Authorization", &format!("Bearer {}", cfg.token))
        .set("User-Agent", USER_AGENT)
        .send_json(json!({ "on": on }));
    read_response(res).map_err(FetchError::message)?;
    saves::set_enabled(&id, on);
    log(&format!("Сохранения: {id} {}", if on { "включены" } else { "выключены" }));
    Ok(get_saves())
}

#[tauri::command]
fn send_save_now(id: String) -> Value {
    saves::request_now(&id);
    get_saves()
}

/// Своя папка сохранений (пустая строка — снова по умолчанию).
#[tauri::command]
fn set_save_dir(id: String, path: String, state: tauri::State<'_, Arc<Shared>>) -> Result<Value, String> {
    let path = path.trim().trim_matches('"').to_string();
    if !path.is_empty() && !std::path::Path::new(&path).is_dir() {
        return Err("такой папки нет".into());
    }
    let mut cfg = state.cfg.lock().unwrap().clone();
    if path.is_empty() {
        cfg.save_dirs.remove(&id);
    } else {
        cfg.save_dirs.insert(id.clone(), path);
    }
    write_config(&cfg).map_err(|e| format!("не сохранить конфиг: {e}"))?;
    saves::set_dirs(&cfg.save_dirs);
    *state.cfg.lock().unwrap() = cfg;
    Ok(get_saves())
}

#[tauri::command]
fn open_save_dir(id: String) {
    if let Some(dir) = saves::dir(&id) {
        win::open_url(&dir.display().to_string());
    }
}

#[tauri::command]
fn close_saves(window: WebviewWindow) {
    std::thread::spawn(move || {
        let _ = window.destroy();
    });
}

fn open_saves(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("saves") {
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "saves", WebviewUrl::App("saves.html".into()))
        .title("BigBacklog Companion — сохранения игр")
        .additional_browser_args(WEBVIEW_ARGS)
        .inner_size(600.0, 560.0)
        .min_inner_size(480.0, 420.0)
        .center()
        .build();
}

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("BigBacklog Companion")
        .additional_browser_args(WEBVIEW_ARGS)
        .inner_size(480.0, 320.0)
        .resizable(false)
        .center()
        .build();
}

// ---------- Опрос ----------

fn poll_loop(s: Arc<Shared>) {
    loop {
        let cfg = s.cfg.lock().unwrap().clone();
        if cfg.token.trim().is_empty() {
            s.set_status("не подключено — «Подключение…»");
            s.set_connected(false);
        } else {
            let server = server_of(&cfg);
            match fetch_state(&server, cfg.token.trim()) {
                Ok(v) => {
                    s.set_connected(true);
                    on_state(&s, v);
                    let stale = s.assets_at.lock().unwrap().map(|t| t.elapsed() >= ASSETS_TTL).unwrap_or(true);
                    if stale {
                        refresh_popup_assets(&s, &server);
                    }
                }
                Err(FetchError::Auth(_)) => {
                    s.set_status("подключение отозвано — «Подключение…»");
                    s.set_connected(false);
                }
                Err(FetchError::Other(m)) => s.set_status(&m),
            }
        }
        if s.demo.swap(false, Ordering::SeqCst) {
            for kind in ["summary"] {
                push_test(&s, kind);
            }
        }
        let (lock, cv) = &s.wake;
        let mut woke = lock.lock().unwrap();
        if !*woke {
            woke = cv.wait_timeout(woke, Duration::from_secs(POLL_SECS)).unwrap().0;
        }
        *woke = false;
    }
}

// ---------- Слежение за играми ----------

#[derive(Clone)]
struct Tracked {
    sid: String,
    info: scan::GameInfo,
    title: Option<String>,
    started: i64,
    last_seen: i64,
    active_secs: u64,
    /// Сколько было в игре до этой сессии — первое, что сказал сервер (см.
    /// record_sessions): к концу сессии Steam мог уже досчитать её часть.
    base_minutes: Option<i64>,
    /// Название и «узнана ли» — как их вернул сервер (report): для строки
    /// «Сейчас» в меню плагина Decky (write_now).
    shown_name: Option<String>,
    resolved: Option<bool>,
    /// Итог уже показан своим (local_summary) — итог сайта за неё не нужен.
    summary_shown: bool,
}

impl Tracked {
    /// Когда сессия кончилась: процесс пропал, и его ждали END_AFTER_SECS.
    fn end_at(&self) -> i64 {
        self.last_seen + END_AFTER_SECS
    }
}

#[derive(Default)]
struct Tracker {
    live: HashMap<String, Tracked>,
    /// Кончившиеся, но ещё не отправленные (нет связи — уедут следующим разом).
    finished: Vec<Tracked>,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Ключ сессии. У эмулятора в ключе и игра из заголовка (только буквы —
/// счётчик FPS в заголовке не должен рвать сессию на каждом снимке).
fn session_key(r: &scan::Running) -> String {
    let mut key = format!("{}:{}", r.info.source, r.info.id);
    // Распакованная игра Xbox 360 приходит без Title ID — различаем по папке.
    if r.info.source == xbox360::SOURCE && r.info.id.is_empty() {
        key.push_str(&r.info.name);
    }
    if r.info.source == "emu" {
        let letters: String = r.title.as_deref().unwrap_or("").chars().filter(|c| c.is_alphabetic())
            .flat_map(char::to_lowercase).collect();
        key.push(':');
        key.push_str(&letters);
    }
    key
}

fn track_loop(s: Arc<Shared>) {
    let mut catalog = scan::Catalog::load();
    let mut catalog_at = Instant::now();
    let mut scanner = scan::Scanner::default();
    let mut last_tick = Instant::now();
    let mut last_report = Instant::now();
    // Что и когда последний раз ушло в /api/agent/inventory.
    let mut inventory_sent: Option<(String, Instant)> = None;
    let mut seq: u64 = 0;
    let stats_dir = scan::steam_stats_dir();
    let mut stats_seen: HashMap<String, SystemTime> = HashMap::new();
    let mut nudge_pending: HashMap<String, Option<i64>> = HashMap::new();
    let mut nudged_at: HashMap<String, Instant> = HashMap::new();
    let mut now_written: Option<(String, Instant)> = None;
    // Сохранения D2R: при старте агента и после выхода из игры.
    let mut d2r_due = true;
    // Сохранение Sacred Gold: так же, но только когда Sacred.exe не запущен.
    let mut sacred_due = true;
    // Сохранение The Witcher 3: так же, пока witcher3.exe не запущен.
    let mut w3_due = true;
    // Сохранения The Binding of Isaac: так же, пока isaac-ng.exe не запущен.
    let mut isaac_due = true;
    // Сохранения Sacred 2 Remaster: так же, пока sacred2.exe не запущен.
    let mut s2_due = true;
    // Профиль Risk of Rain 2: так же, пока «Risk of Rain 2.exe» не запущен.
    let mut ror2_due = true;
    // Сохранения Cyberpunk 2077: так же, пока Cyberpunk2077.exe не запущен.
    let mut cp77_due = true;
    // Сохранения OpenMW (в обе стороны): так же, плюс раз в openmw::EVERY, пока
    // openmw.exe не запущен; выход из игры замечаем по пропаже процесса.
    let mut omw_due = true;
    let mut omw_running = false;
    let mut omw_at: Option<Instant> = None;
    // Игры из описаний сервера (specs.rs): пора отправить. Новая игра в списке —
    // сразу повод, как старт агента у встроенных.
    let mut spec_due: HashMap<String, bool> = HashMap::new();
    // Описания, у которых «Отправить сейчас»: свежий файл уходит и повторно.
    let mut spec_force: HashSet<String> = HashSet::new();
    // Входящие от модов: первый такт после старта, дальше не чаще INBOX_EVERY
    // и только когда в папке что-то лежит.
    let inbox_dir = inbox::dir();
    let _ = std::fs::create_dir_all(&inbox_dir);
    let mut inbox_at: Option<Instant> = None;
    loop {
        std::thread::sleep(SCAN_EVERY);
        let dt = last_tick.elapsed().as_secs().min(30);
        last_tick = Instant::now();
        if catalog_at.elapsed() >= CATALOG_TTL {
            catalog = scan::Catalog::load();
            catalog_at = Instant::now();
        }
        let cfg = s.cfg.lock().unwrap().clone();
        let token = cfg.token.trim().to_string();
        if token.is_empty() {
            continue;
        }
        let server = server_of(&cfg);
        // Список установленных игр — только при включенном «Считать время в играх» и только когда он
        // поменялся (поставили или удалили игру), плюс раз в INVENTORY_TTL.
        if s.tracking.load(Ordering::SeqCst) {
            let body = json!({"games": catalog.installed});
            let text = body.to_string();
            let due = match &inventory_sent {
                None => true,
                Some((sent, at)) => *sent != text || at.elapsed() >= INVENTORY_TTL,
            };
            if due && post_json(&server, &token, "/api/agent/inventory", &body).is_ok() {
                inventory_sent = Some((text, Instant::now()));
            }
        }

        let now = unix_now();
        let (has_live, has_finished) = {
            let mut t = s.tracker.lock().unwrap();
            if s.tracking.load(Ordering::SeqCst) {
                let mut seen = HashSet::new();
                // Игра на Xbox 360 — как ещё один процесс (опрос приставки идёт
                // своим потоком, тут берётся последний свежий ответ).
                for r in scanner.scan(&catalog).into_iter().chain(xbox360::current()) {
                    let key = session_key(&r);
                    if !t.live.contains_key(&key) {
                        // Новая сессия — только у игры с окном: фоновые службы и
                        // «тихие» процессы из папки игры сессию не открывают.
                        if !r.has_window {
                            continue;
                        }
                        seq += 1;
                        log(&format!("игра началась: {}", r.info.name));
                        t.live.insert(key.clone(), Tracked {
                            sid: format!("{now:x}-{seq:x}"),
                            info: r.info.clone(),
                            title: r.title.clone(),
                            started: now,
                            last_seen: now,
                            active_secs: 0,
                            base_minutes: None,
                            shown_name: None,
                            resolved: None,
                            summary_shown: false,
                        });
                    }
                    if let Some(tr) = t.live.get_mut(&key) {
                        tr.last_seen = now;
                        if r.title.is_some() {
                            tr.title = r.title.clone();
                        }
                        tr.active_secs += dt;
                    }
                    seen.insert(key);
                }
                let gone: Vec<String> = t.live.iter()
                    .filter(|(k, v)| !seen.contains(*k) && now - v.last_seen >= END_AFTER_SECS)
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in gone {
                    if let Some(v) = t.live.remove(&k) {
                        log(&format!("игра кончилась: {}, {} мин", v.info.name, v.active_secs / 60));
                        // Вышли из игры — хороший момент проверить обновления (встанет, когда все покажут).
                        update::check_soon();
                        if v.info.source == "steam" && v.info.id == d2r::STEAM_APPID {
                            d2r_due = true;
                        }
                        if v.info.source == "steam" && v.info.id == sacred::STEAM_APPID {
                            sacred_due = true;
                        }
                        if v.info.source == "steam" && v.info.id == isaac::STEAM_APPID {
                            isaac_due = true;
                        }
                        if v.info.source == "steam" && v.info.id == s2::STEAM_APPID {
                            s2_due = true;
                        }
                        if v.info.source == "steam" && v.info.id == ror2::STEAM_APPID {
                            ror2_due = true;
                        }
                        if v.info.source == "steam" {
                            for sp in specs::all() {
                                if sp.steam_appids.contains(&v.info.id) {
                                    spec_due.insert(sp.id, true);
                                }
                            }
                        }
                        if (v.info.source == "steam" && v.info.id == cp77::STEAM_APPID)
                            || (v.info.source == "gog" && v.info.id == cp77::GOG_ID)
                        {
                            cp77_due = true;
                        }
                        if v.info.source == "steam" && (v.info.id == w3::STEAM_APPID || v.info.id == w3::STEAM_APPID_GOTY) {
                            w3_due = true;
                        }
                        t.finished.push(v);
                    }
                }
            } else {
                let stopped: Vec<Tracked> = t.live.drain().map(|(_, v)| v).collect();
                t.finished.extend(stopped);
            }
            (!t.live.is_empty(), !t.finished.is_empty())
        };
        if has_finished || (has_live && last_report.elapsed() >= REPORT_EVERY) {
            report(&s, &server, &token);
            last_report = Instant::now();
        }
        write_now(&s, &mut now_written);
        // Сохранения — только у игр, где человек включил отправку (saves.rs);
        // «Отправить сейчас» и только что включённая игра — тоже повод.
        d2r_due |= saves::take_now("d2r");
        sacred_due |= saves::take_now("sacred");
        w3_due |= saves::take_now("w3");
        isaac_due |= saves::take_now("isaac");
        s2_due |= saves::take_now("s2");
        ror2_due |= saves::take_now("ror2");
        cp77_due |= saves::take_now("cp77");
        omw_due |= saves::take_now("omw");
        // Как у остальных игр: пока D2R.exe жив, ждем (файлы пишет сама игра).
        if d2r_due && saves::enabled("d2r") && !scan::exe_running(d2r::EXE) {
            d2r_due = false;
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || d2r_sync(&server, &token));
        }
        // Игра пишет файл сама — пока Sacred.exe жив (агент стартовал посреди
        // игры), ждём: отметка остаётся, процессы смотрим только при ней.
        if sacred_due && saves::enabled("sacred") && !SACRED_BUSY.load(Ordering::SeqCst)
            && !scan::exe_running(sacred::EXE)
        {
            sacred_due = false;
            SACRED_BUSY.store(true, Ordering::SeqCst);
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                sacred_sync(&server, &token);
                SACRED_BUSY.store(false, Ordering::SeqCst);
            });
        }
        if w3_due && saves::enabled("w3") && !W3_BUSY.load(Ordering::SeqCst) && !scan::exe_running(w3::EXE) {
            w3_due = false;
            W3_BUSY.store(true, Ordering::SeqCst);
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                w3_sync(&server, &token);
                W3_BUSY.store(false, Ordering::SeqCst);
            });
        }
        for sp in specs::all() {
            let due = spec_due.entry(sp.id.clone()).or_insert(true);
            // «Отправить сейчас» и только что включенная отправка шлют свежий файл и повторно.
            if saves::take_now(&sp.id) {
                *due = true;
                spec_force.insert(sp.id.clone());
            }
            if !*due || !saves::enabled(&sp.id) || SPEC_BUSY.lock().unwrap().contains(&sp.id)
                || sp.exe.iter().any(|e| scan::exe_running(e))
            {
                continue;
            }
            *due = false;
            let force = spec_force.remove(&sp.id);
            SPEC_BUSY.lock().unwrap().push(sp.id.clone());
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                spec_sync(&sp, &server, &token, force);
                SPEC_BUSY.lock().unwrap().retain(|x| x != &sp.id);
            });
        }
        if isaac_due && saves::enabled("isaac") && !ISAAC_BUSY.load(Ordering::SeqCst) && !scan::exe_running(isaac::EXE) {
            isaac_due = false;
            ISAAC_BUSY.store(true, Ordering::SeqCst);
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                isaac_sync(&server, &token);
                ISAAC_BUSY.store(false, Ordering::SeqCst);
            });
        }
        if s2_due && saves::enabled("s2") && !S2_BUSY.load(Ordering::SeqCst) && !scan::exe_running(s2::EXE) {
            s2_due = false;
            S2_BUSY.store(true, Ordering::SeqCst);
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                s2_sync(&server, &token);
                S2_BUSY.store(false, Ordering::SeqCst);
            });
        }
        if ror2_due && saves::enabled("ror2") && !ROR2_BUSY.load(Ordering::SeqCst) && !scan::exe_running(ror2::EXE) {
            ror2_due = false;
            ROR2_BUSY.store(true, Ordering::SeqCst);
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                ror2_sync(&server, &token);
                ROR2_BUSY.store(false, Ordering::SeqCst);
            });
        }
        if cp77_due && saves::enabled("cp77") && !CP77_BUSY.load(Ordering::SeqCst) && !scan::exe_running(cp77::EXE) {
            cp77_due = false;
            CP77_BUSY.store(true, Ordering::SeqCst);
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                cp77_sync(&server, &token);
                CP77_BUSY.store(false, Ordering::SeqCst);
            });
        }
        if saves::enabled("omw") && openmw::present() {
            let running = scan::exe_running(openmw::EXE);
            if omw_running && !running {
                omw_due = true;
            }
            omw_running = running;
            let periodic = omw_at.is_none_or(|t| t.elapsed() >= openmw::EVERY);
            if !running && (omw_due || periodic) && openmw::try_start() {
                omw_due = false;
                omw_at = Some(Instant::now());
                let (server, token) = (server.clone(), token.clone());
                std::thread::spawn(move || {
                    openmw::sync(&server, &token);
                    openmw::finish();
                });
            }
        }
        // Свои достижения модов (OpenMW из openmw.log, Sacred Gold через ящик). Сервер принимает их только
        // с фичефлагом agent; при отказе файлы остаются в ящике до следующего раза.
        omwlog::pump(&inbox_dir);
        if inbox_at.map(|t| t.elapsed() >= INBOX_EVERY).unwrap_or(true)
            && !INBOX_BUSY.load(Ordering::SeqCst)
            && inbox::has_files(&inbox_dir)
        {
            inbox_at = Some(Instant::now());
            INBOX_BUSY.store(true, Ordering::SeqCst);
            let (server, token, dir) = (server.clone(), token.clone(), inbox_dir.clone());
            std::thread::spawn(move || {
                inbox_sync(&server, &token, &dir);
                INBOX_BUSY.store(false, Ordering::SeqCst);
            });
        }

        // Ачивка Steam: файл статистики идущей игры стал новее — просим сервер
        // забрать её ачивки сразу, опыт придёт следующим опросом (STEAM_NUDGE_GAP).
        let steam_live: Vec<String> = s.tracker.lock().unwrap().live.values()
            .filter(|v| v.info.source == "steam")
            .map(|v| v.info.id.clone())
            .collect();
        if let Some(dir) = stats_dir.as_ref() {
            for appid in &steam_live {
                let Some((mtime, accountid)) = newest_stats(dir, appid) else { continue };
                if let Some(prev) = stats_seen.insert(appid.clone(), mtime) {
                    if mtime > prev {
                        nudge_pending.insert(appid.clone(), accountid);
                    }
                }
            }
            stats_seen.retain(|appid, _| steam_live.contains(appid));
        }
        let due: Vec<(String, Option<i64>)> = nudge_pending.iter()
            .filter(|(appid, _)| nudged_at.get(*appid).map(|t| t.elapsed() >= STEAM_NUDGE_GAP).unwrap_or(true))
            .map(|(appid, accountid)| (appid.clone(), *accountid))
            .collect();
        for (appid, accountid) in due {
            nudge_pending.remove(&appid);
            nudged_at.insert(appid.clone(), Instant::now());
            let (server, token, shared) = (server.clone(), token.clone(), s.clone());
            std::thread::spawn(move || {
                let body = json!({"appid": appid.parse::<i64>().unwrap_or(0), "accountid": accountid});
                if post_json(&server, &token, "/api/agent/steam-sync", &body).is_ok() {
                    shared.wake_poll();
                }
            });
        }
    }
}

/// Изменившиеся сохранения D2R — на сайт по одному файлу; удачно отправленное
/// помечается в d2r.json рядом с config.json, чтобы не слать его снова.
fn d2r_sync(server: &str, token: &str) {
    let Some(dir) = saves::dir("d2r").filter(|d| d.is_dir()) else { return };
    let path = config_path().with_file_name("d2r.json");
    let mut sent = d2r::load_sent(&path);
    let mut any = false;
    for (file, name, mtime) in d2r::changed(&dir, &sent) {
        let Ok(body) = std::fs::read(&file) else { continue };
        let res = http()
            .post(&format!("{server}/api/agent/d2r-save"))
            .query("name", &name)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&body);
        match read_response(res) {
            Ok(_) => {
                log(&format!("D2R: сохранение {name} отправлено на сайт"));
                sent.insert(name, mtime);
                any = true;
            }
            Err(e) => log(&format!("D2R: {name} не отправлено: {}", e.message())),
        }
    }
    if any {
        d2r::save_sent(&path, &sent);
    }
}

/// Отправка сохранения Ведьмака идёт — следующая не запускается.
static W3_BUSY: AtomicBool = AtomicBool::new(false);
/// Позднее сохранение — до ~3 МБ: как у Sacred, своё время на медленный канал.
const W3_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Свежее сохранение The Witcher 3 (*.sav из «Документы\The Witcher 3\gamesaves»)
/// — на сайт, если это не то, что уже ушло, и следом customUserData.json
/// (счётчик смертей), если он поменялся. Отметка — в w3.json рядом с
/// config.json. Не ушло — повторится при следующем старте агента или выходе
/// из игры.
fn w3_sync(server: &str, token: &str) {
    // saves::dir("w3") — папка gamesaves; customUserData.json лежит уровнем выше.
    let Some(saves_dir) = saves::dir("w3").filter(|d| d.is_dir()) else { return };
    let dir = saves_dir.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| saves_dir.clone());
    let path = config_path().with_file_name("w3.json");
    let mut sent = w3::load_sent(&path).unwrap_or_default();
    let post = |name: &str, body: &[u8]| {
        let res = tls::agent()
            .timeout(W3_UPLOAD_TIMEOUT)
            .build()
            .post(&format!("{server}/api/agent/w3-save"))
            .query("name", name)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(body);
        read_response(res)
    };
    if let Some((file, cur)) = w3::changed(&saves_dir, Some(&sent)) {
        let Ok(body) = std::fs::read(&file) else { return };
        match post(&cur.name, &body) {
            Ok(_) => {
                log(&format!("Ведьмак: сохранение {} отправлено на сайт", cur.name));
                sent = w3::Sent { user_mtime: sent.user_mtime, ..cur };
                w3::save_sent(&path, &sent);
            }
            Err(e) => {
                log(&format!("Ведьмак: {} не отправлено: {}", cur.name, e.message()));
                return;
            }
        }
    }
    let user_mtime = w3::user_data_mtime(&dir);
    if user_mtime != 0 && user_mtime != sent.user_mtime {
        let Ok(body) = std::fs::read(dir.join(w3::USER_DATA)) else { return };
        match post(w3::USER_DATA, &body) {
            Ok(_) => {
                sent.user_mtime = user_mtime;
                w3::save_sent(&path, &sent);
            }
            Err(e) => log(&format!("Ведьмак: {} не отправлено: {}", w3::USER_DATA, e.message())),
        }
    }
}

/// Отправка сохранений Sacred 2 идёт — следующая не запускается.
static S2_BUSY: AtomicBool = AtomicBool::new(false);

/// Файлы героев Sacred 2 Remaster (heroNN.sacred2save/.sacred2stats, общий
/// chest.sacred2chest, единицы КБ) — на сайт каждый изменившийся, сайт сам
/// раскладывает их по героям. Отметка {имя: время} — в s2.json рядом с
/// config.json, после каждого удачного файла. Не ушло — стоп, остальное
/// повторится при следующем старте агента или выходе из игры.
fn s2_sync(server: &str, token: &str) {
    let Some(dir) = saves::dir("s2").filter(|d| d.is_dir()) else { return };
    let path = config_path().with_file_name("s2.json");
    let mut sent = s2::load_sent(&path);
    for (file, name, mtime) in s2::changed(&dir, &sent) {
        let Ok(body) = std::fs::read(&file) else { continue };
        let res = http()
            .post(&format!("{server}/api/agent/s2-save"))
            .query("device", "pc")
            .query("name", &name)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&body);
        match read_response(res) {
            Ok(_) => {
                log(&format!("Sacred 2: {name} отправлен на сайт"));
                sent.insert(name, mtime);
                s2::save_sent(&path, &sent);
            }
            Err(e) => {
                log(&format!("Sacred 2: {name} не отправлен: {}", e.message()));
                return;
            }
        }
    }
}

/// Игры из описаний сервера, у которых сейчас идет отправка.
static SPEC_BUSY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Игра из описания сервера (specs.rs): каждый изменившийся файл на ее ручку. Отметка
/// {путь от корня: время} в <id>.json рядом с config.json, после каждого удачного файла.
/// Не ушло — стоп до следующего повода.
fn spec_sync(sp: &specs::Spec, server: &str, token: &str, force: bool) {
    let roots = saves::spec_roots(sp);
    if roots.is_empty() {
        return;
    }
    let path = config_path().with_file_name(format!("{}.json", sp.id));
    let mut sent = specs::load_sent(&path);
    for cur in specs::to_send(sp, &roots, &sent, force) {
        let Ok(body) = std::fs::read(&cur.path) else { continue };
        let res = http()
            .post(&format!("{server}{}", sp.url))
            .query("device", "pc")
            .query("name", &cur.name)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&body);
        match read_response(res) {
            Ok(_) => {
                log(&format!("{}: сохранение {} отправлено на сайт", sp.name, cur.mark));
                sent.insert(cur.mark.clone(), cur.mtime);
                specs::save_sent(&path, &sent);
            }
            Err(e) => {
                log(&format!("{}: {} не отправлено: {}", sp.name, cur.mark, e.message()));
                return;
            }
        }
    }
}

/// Отправка сохранений Isaac идёт — следующая не запускается.
static ISAAC_BUSY: AtomicBool = AtomicBool::new(false);

/// Профили The Binding of Isaac (persistentgamedata1..3.dat, ~7 КБ) — на сайт каждый
/// изменившийся. Отметка {имя: время} — в isaac.json рядом с config.json, после
/// каждого удачного файла. Не ушло (нет связи, 4xx, 5xx) — стоп, остальное
/// повторится при следующем старте агента или выходе из игры.
fn isaac_sync(server: &str, token: &str) {
    let Some(dir) = saves::dir("isaac").filter(|d| d.is_dir()) else { return };
    let path = config_path().with_file_name("isaac.json");
    let mut sent = isaac::load_sent(&path);
    for (file, cur) in isaac::changed(&dir, &sent) {
        let Ok(body) = std::fs::read(&file) else { continue };
        let res = http()
            .post(&format!("{server}/api/agent/isaac-save"))
            .query("device", "pc")
            .query("name", &cur.name)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&body);
        match read_response(res) {
            Ok(_) => {
                log(&format!("Isaac: сохранение {} отправлено на сайт", cur.name));
                sent.insert(cur.name.clone(), cur.mtime);
                isaac::save_sent(&path, &sent);
            }
            Err(e) => {
                log(&format!("Isaac: {} не отправлено: {}", cur.name, e.message()));
                return;
            }
        }
    }
}

/// Отправка профиля Risk of Rain 2 идёт — следующая не запускается.
static ROR2_BUSY: AtomicBool = AtomicBool::new(false);

/// Профили Risk of Rain 2 (UserProfiles\<GUID>.xml, 200–400 КБ) — на сайт каждый
/// изменившийся. Отметка {имя: время} — в ror2.json рядом с config.json, после
/// каждого удачного файла. Не ушло (нет связи, 4xx, 5xx) — стоп, остальное
/// повторится при следующем старте агента или выходе из игры.
fn ror2_sync(server: &str, token: &str) {
    let path = config_path().with_file_name("ror2.json");
    let mut sent = ror2::load_sent(&path);
    // Сначала профиль, затем отчеты истории попыток (их отметки — с приставкой «runs/»).
    let mut todo: Vec<(std::path::PathBuf, String, u64)> = Vec::new();
    if let Some(dir) = saves::dir("ror2").filter(|d| d.is_dir()) {
        for (file, cur) in ror2::changed(&dir, &sent) {
            todo.push((file, cur.name.clone(), cur.mtime));
        }
    }
    if let Some(dir) = ror2::history_dir() {
        let marks: HashMap<String, u64> = sent
            .iter()
            .filter_map(|(k, v)| k.strip_prefix("runs/").map(|n| (n.to_string(), *v)))
            .collect();
        for (file, cur) in ror2::changed(&dir, &marks) {
            todo.push((file, format!("runs/{}", cur.name), cur.mtime));
        }
    }
    for (file, mark, mtime) in todo {
        let name = mark.strip_prefix("runs/").unwrap_or(&mark).to_string();
        let Ok(body) = std::fs::read(&file) else { continue };
        let res = http()
            .post(&format!("{server}/api/agent/ror2-save"))
            .query("device", "pc")
            .query("name", &name)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&body);
        match read_response(res) {
            Ok(_) => {
                log(&format!("Risk of Rain 2: {name} отправлен на сайт"));
                sent.insert(mark, mtime);
                ror2::save_sent(&path, &sent);
            }
            Err(e) => {
                log(&format!("Risk of Rain 2: {name} не отправлен: {}", e.message()));
                return;
            }
        }
    }
}

/// Отправка сохранения Cyberpunk 2077 идет — следующая не запускается.
static CP77_BUSY: AtomicBool = AtomicBool::new(false);
/// sav.dat 3–8 МБ: как у Ведьмака, свое время на медленный канал.
const CP77_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Сохранения Cyberpunk 2077: у каждого прохождения самая свежая папка (AutoSave-0 и т.п.),
/// если она изменилась, телом metadata.9.json + sav.dat; сервер ответил «saved» — следом
/// screenshot.png (part=shot). Отметка {папка: время sav.dat} — в cp77.json рядом с
/// config.json, после каждой удачной папки. Не ушло — стоп, повторится при следующем
/// старте агента или выходе из игры.
fn cp77_sync(server: &str, token: &str) {
    let Some(dir) = saves::dir("cp77").filter(|d| d.is_dir()) else { return };
    let path = config_path().with_file_name("cp77.json");
    let mut sent = cp77::load_sent(&path);
    let agent = tls::agent().timeout(CP77_UPLOAD_TIMEOUT).build();
    let post = |name: &str, extra: &[(&str, &str)], body: &[u8]| {
        let mut req = agent
            .post(&format!("{server}/api/agent/cp77-save"))
            .query("device", "pc")
            .query("name", name);
        for (k, v) in extra {
            req = req.query(k, v);
        }
        read_response(
            req.set("Authorization", &format!("Bearer {token}"))
                .set("User-Agent", USER_AGENT)
                .set("Content-Type", "application/octet-stream")
                .send_bytes(body),
        )
    };
    for (file, cur) in cp77::changed(&dir, &sent) {
        let Some(body) = cp77::body(&file) else { continue };
        match post(&cur.name, &[], &body) {
            Ok(v) => {
                let result = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
                log(&format!("Cyberpunk 2077: сохранение {} отправлено на сайт ({result})", cur.name));
                if result == "saved" {
                    let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("");
                    if let (false, Ok(shot)) = (key.is_empty(), std::fs::read(file.join(cp77::SHOT))) {
                        if let Err(e) = post(&cur.name, &[("part", "shot"), ("key", key)], &shot) {
                            log(&format!("Cyberpunk 2077: снимок {} не отправлен: {}", cur.name, e.message()));
                        }
                    }
                }
                sent.insert(cur.name.clone(), cur.mtime);
                cp77::prune(&mut sent, &cp77::newest_per_playthrough(&dir));
                cp77::save_sent(&path, &sent);
            }
            Err(e) => {
                log(&format!("Cyberpunk 2077: {} не отправлено: {}", cur.name, e.message()));
                return;
            }
        }
    }
}

/// Отправка сохранения Sacred идёт — следующая не запускается.
static SACRED_BUSY: AtomicBool = AtomicBool::new(false);
/// Файл ~2,6 МБ: общих 15 с (http()) на медленном канале может не хватить.
const SACRED_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Сохранения Sacred Gold (GAMEnn.PAK) — на сайт каждое изменившееся, от
/// старых к новым: героев бывает несколько, сайт держит каждого отдельно.
/// Отметка {имя: время} — в sacred.json рядом с config.json, после каждого
/// удачного файла. Не ушло (нет связи, 404, 5xx) — стоп, остальное повторится
/// при следующем старте агента или выходе из игры.
fn sacred_sync(server: &str, token: &str) {
    let Some(dir) = saves::dir("sacred").filter(|d| d.is_dir()) else { return };
    let path = config_path().with_file_name("sacred.json");
    let mut sent = sacred::load_sent(&path);
    for (file, cur) in sacred::changed(&dir, &sent) {
        let Ok(body) = std::fs::read(&file) else { continue };
        let res = tls::agent()
            .timeout(SACRED_UPLOAD_TIMEOUT)
            .build()
            .post(&format!("{server}/api/agent/sacred-save"))
            .query("device", "pc")
            .query("name", &cur.name)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&body);
        match read_response(res) {
            Ok(_) => {
                log(&format!("Sacred: сохранение {} отправлено на сайт", cur.name));
                sent.insert(cur.name.clone(), cur.mtime);
                sacred::save_sent(&path, &sent);
            }
            Err(e) => {
                log(&format!("Sacred: {} не отправлено: {}", cur.name, e.message()));
                return;
            }
        }
        sacred_stats(server, token, &file);
    }
}

/// Параметры героя от мода геймпада (здоровье, урон, сопротивления...) —
/// следом за своим .PAK. Не ушли — не беда: сохранение уже на сайте, а
/// повтора ради ~3 КБ гонять 2,6 МБ заново незачем.
fn sacred_stats(server: &str, token: &str, pak: &std::path::Path) {
    let Some((stats, name)) = sacred::stats_for(pak) else { return };
    let Ok(body) = std::fs::read(&stats) else { return };
    let res = tls::agent()
        .timeout(SACRED_UPLOAD_TIMEOUT)
        .build()
        .post(&format!("{server}/api/agent/sacred-stats"))
        .query("device", "pc")
        .query("name", &name)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", USER_AGENT)
        .set("Content-Type", "application/json")
        .send_bytes(&body);
    match read_response(res) {
        Ok(_) => log(&format!("Sacred: параметры героя {name} отправлены")),
        Err(e) => log(&format!("Sacred: параметры {name} не отправлены: {}", e.message())),
    }
}

const INBOX_EVERY: Duration = Duration::from_secs(15);
/// Отправка входящих идёт — следующий такт новую не запускает.
static INBOX_BUSY: AtomicBool = AtomicBool::new(false);
/// Последняя ошибка отправки: одна и та же пишется в журнал один раз.
static INBOX_ERR: Mutex<String> = Mutex::new(String::new());
/// Пачек за один заход — чтобы не крутиться, если файлы не удаляются.
const INBOX_MAX_BATCHES: usize = 10;

/// Достижения модов (папка inbox) — на сайт пачками, отправленное удаляется.
/// При ошибке файлы остаются до следующего захода.
fn inbox_sync(server: &str, token: &str, dir: &std::path::Path) {
    for _ in 0..INBOX_MAX_BATCHES {
        let items = inbox::collect(dir, inbox::MAX_BATCH, SystemTime::now());
        if items.is_empty() {
            return;
        }
        let events: Vec<Value> = items.iter().map(|it| it.event.clone()).collect();
        let unlocks = items.iter().filter(|it| !it.snapshot).count();
        match post_json(server, token, "/api/agent/custom-achievements", &json!({"events": events})) {
            Ok(resp) => {
                inbox::remove_sent(&items);
                INBOX_ERR.lock().unwrap().clear();
                let unlocked = resp.get("unlocked").and_then(|v| v.as_i64()).unwrap_or(0);
                // Снимок прогресса мод переписывает часто — в журнал только события.
                if unlocks > 0 || unlocked > 0 {
                    log(&format!("достижения мода: отправлено {}, открыто на сайте {unlocked}", items.len()));
                }
            }
            Err(e) => {
                let kind = if matches!(e, FetchError::Auth(_)) { "токен не принят: " } else { "" };
                let msg = format!("{kind}{}", e.message());
                let mut last = INBOX_ERR.lock().unwrap();
                if *last != msg {
                    log(&format!("достижения мода: не отправлено ({} шт.), {msg}", items.len()));
                    *last = msg;
                }
                return;
            }
        }
        if items.len() < inbox::MAX_BATCH {
            return;
        }
    }
}

/// Самый свежий файл статистики игры (UserGameStats_<accountid>_<appid>.bin)
/// и accountid из его имени.
fn newest_stats(dir: &std::path::Path, appid: &str) -> Option<(SystemTime, Option<i64>)> {
    let suffix = format!("_{appid}.bin");
    std::fs::read_dir(dir).ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let account = name.strip_prefix("UserGameStats_")?.strip_suffix(suffix.as_str())?.parse::<i64>().ok();
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, account))
        })
        .max_by_key(|(mtime, _)| *mtime)
}

/// Свой итог кончившейся сессии — когда сайт своего не дал (короткая или нет
/// связи). Тот же вид, что session_summary сервера, без HLTB; «было» — что
/// сказал сайт в начале сессии (или в этом отчёте). Меньше минуты — не итог.
fn local_summary(v: &mut Tracked, base: Option<i64>) -> Option<Value> {
    let minutes = (v.active_secs / 60) as i64;
    if v.summary_shown || minutes < 1 {
        return None;
    }
    v.summary_shown = true;
    let before = v.base_minutes.or(base).unwrap_or(0).max(0);
    let steam = v.info.source == "steam";
    Some(json!({
        "sid": v.sid, "local": true,
        "name": v.shown_name.clone().unwrap_or_else(|| v.info.name.clone()),
        "platform": v.info.source, "minutes": minutes,
        "before_minutes": before, "after_minutes": before + minutes,
        // Плагин Decky в Big Picture берёт по нему обложку у самого Steam.
        "appid": steam.then(|| v.info.id.clone()),
        "cover": steam.then(|| format!(
            "https://shared.steamstatic.com/store_item_assets/steam/apps/{}/library_600x900.jpg", v.info.id)),
        "link": steam.then(|| format!("/library/steam/{}", v.info.id)),
    }))
}

/// Итоги — впереди очереди: сперва «сколько сыграл», за ним — опыт за всю
/// сессию полоской (xp_popup с сервера, xp.session_popup). Показали опыт
/// сессии — точка отсчёта опроса переезжает на «стало», и хвост, что оставался
/// к выходу из игры («+2 XP»), второй плашкой уже не выходит.
fn queue_summaries(s: &Shared, list: Vec<Value>) -> bool {
    if list.is_empty() {
        return false;
    }
    let mut toasts: Vec<Value> = Vec::new();
    let mut session_xp = false;
    for v in list {
        let popup = v.get("xp_popup").filter(|p| int(p, "gained") > 0).cloned();
        toasts.push(json!({"type": "session", "session": v, "at": unix_ms()}));
        if let Some(p) = popup {
            toasts.push(json!({"type": "xp", "gained": int(&p, "gained"), "from": p["from"], "to": p["to"]}));
            move_baseline(s, &p["to"]);
            session_xp = true;
        }
    }
    if !s.paused.load(Ordering::SeqCst) {
        let mut q = s.pending.lock().unwrap();
        for t in toasts.into_iter().rev() {
            q.push_front(t);
        }
    }
    s.wake_poll();
    session_xp
}

/// Точка отсчёта прироста — на уровень `to` (опыт до него уже показан).
fn move_baseline(s: &Shared, to: &Value) {
    {
        let mut base = s.baseline.lock().unwrap();
        let Some(b) = base.as_mut() else { return };
        // Опрос уже ушёл дальше — назад не двигаем, иначе прирост покажется дважды.
        if int(b, "xp") >= int(to, "xp") {
            return;
        }
        let Some(b) = b.as_object_mut() else { return };
        for key in ["xp", "level", "into_level", "need", "max_level", "badge"] {
            if let Some(v) = to.get(key) {
                b.insert(key.to_string(), v.clone());
            }
        }
    }
    save_poll(s);
}

fn report(s: &Shared, server: &str, token: &str) {
    let (payload, finished_n) = {
        let t = s.tracker.lock().unwrap();
        let rows: Vec<Value> = t.live.values().map(|v| (v, false))
            .chain(t.finished.iter().map(|v| (v, true)))
            .map(|(v, ended)| json!({
                "sid": v.sid, "source": v.info.source, "id": v.info.id, "name": v.info.name,
                // Заголовок окна нужен только эмуляторам (какая в них игра) и Xbox 360 (имя с приставки).
                "title": if v.info.source == "emu" || v.info.source == xbox360::SOURCE { v.title.clone() } else { None },
                "started_at": v.started, "ended_at": v.last_seen,
                "active_minutes": v.active_secs / 60, "ended": ended, "base_minutes": v.base_minutes,
            }))
            .collect();
        (json!({"sessions": rows}), t.finished.len())
    };
    let Ok(resp) = post_json(server, token, "/api/agent/sessions", &payload) else {
        if finished_n > 0 {
            // Нет связи — итог сыгранного показываем сами, но не с первой
            // неудачи: чаще всего это перезапуск сайта на минуту (наша
            // заглушка nginx отвечает 503), и его итог — с обложкой и
            // полоской HLTB — приедет следующей попыткой. Свой итог, без
            // полоски, идёт только когда сайт молчит LOCAL_SUMMARY_WAIT_SECS
            // после выхода из игры; второй раз итог не выйдет.
            let waited = unix_now() - LOCAL_SUMMARY_WAIT_SECS;
            let mut t = s.tracker.lock().unwrap();
            let waiting = t.finished.iter().filter(|v| v.end_at() > waited).count();
            let local: Vec<Value> = t.finished.iter_mut()
                .filter(|v| v.end_at() <= waited)
                .filter_map(|v| local_summary(v, None))
                .collect();
            drop(t);
            log(&format!(
                "отчёт о кончившихся ({finished_n}) не ушёл — нет связи с сайтом; своих итогов {}{}",
                local.len(),
                if waiting > 0 { format!(", жду сайт ещё по {waiting}") } else { String::new() },
            ));
            let _ = queue_summaries(s, local);
        }
        return;
    };
    let live = resp.get("live").and_then(|l| l.as_array()).cloned().unwrap_or_default();
    let mut sent: Vec<Tracked> = {
        let mut t = s.tracker.lock().unwrap();
        let n = finished_n.min(t.finished.len());
        let sent: Vec<Tracked> = t.finished.drain(..n).collect();
        for g in &live {
            let Some(sid) = g.get("sid").and_then(|x| x.as_str()) else { continue };
            let Some(tr) = t.live.values_mut().find(|tr| tr.sid == sid) else { continue };
            if let Some(base) = g.get("base_minutes").and_then(|x| x.as_i64()) {
                tr.base_minutes.get_or_insert(base);
            }
            tr.shown_name = g.get("name").and_then(|n| n.as_str()).map(str::to_string);
            tr.resolved = g.get("resolved").and_then(|r| r.as_bool());
        }
        sent
    };
    // Игра кончилась — ближайший прирост опыта показать любым (AFTER_GAME_SECS).
    if finished_n > 0 {
        s.after_game_until.store(unix_now() + AFTER_GAME_SECS, Ordering::SeqCst);
        s.wake_poll();
    }
    // Итог сессии — впереди очереди: сперва «сколько сыграл», потом опыт за
    // это (его приносит опрос, разбуженный тут же, а не через 45 с). Плашки
    // идут по одной, так что после полноэкранной игры они не лягут друг на друга.
    let summaries = resp.get("summaries").and_then(|l| l.as_array()).cloned().unwrap_or_default();
    // Итог каждой кончившейся: от сайта, а нет его (сессию короче
    // MIN_SESSION_MINUTES сайт не пишет) — свой, из того, что знает агент:
    // плашка после игры приходит всегда, от минуты.
    let sid_of = |v: &Value| v.get("sid").and_then(|x| x.as_str()).map(str::to_string);
    let mut show: Vec<Value> = Vec::new();
    let mut local_n = 0;
    for v in sent.iter_mut() {
        match summaries.iter().find(|x| sid_of(x).as_deref() == Some(v.sid.as_str())) {
            Some(sm) if !v.summary_shown => show.push(sm.clone()),
            Some(_) => {}
            None => {
                let base = live.iter().find(|g| sid_of(g).as_deref() == Some(v.sid.as_str()))
                    .and_then(|g| g.get("base_minutes")).and_then(|b| b.as_i64());
                if let Some(local) = local_summary(v, base) {
                    show.push(local);
                    local_n += 1;
                }
            }
        }
    }
    if finished_n > 0 || !summaries.is_empty() {
        log(&format!(
            "отчёт: кончилось сессий {finished_n}, итогов от сайта {}, своих {local_n}{}",
            summaries.len(),
            if s.paused.load(Ordering::SeqCst) { " — плашки на паузе, итог не показываю" } else { "" },
        ));
    }
    // Опыт за сессию пришёл с итогом — «ближайший прирост любым» уже не
    // нужен: хвост к выходу из игры входит в эту плашку.
    if queue_summaries(s, show) {
        s.after_game_until.store(0, Ordering::SeqCst);
    }
    let now: Vec<String> = live.iter()
        .filter(|g| !g.get("ended").and_then(|e| e.as_bool()).unwrap_or(false))
        .map(|g| {
            let name = g.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let known = g.get("resolved").and_then(|r| r.as_bool()).unwrap_or(false);
            format!("{name} · {} мин{}", int(g, "minutes"), if known { "" } else { " (не узнана)" })
        })
        .collect();
    s.set_now(&if now.is_empty() { "Сейчас: не играешь".to_string() } else { format!("Сейчас: {}", now.join(", ")) });
}

// ---------- Трей ----------

fn build_tray(app: &AppHandle, s: &Arc<Shared>) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "запуск…", false, None::<&str>)?;
    let now_item = MenuItem::with_id(app, "now", "Сейчас: не играешь", false, None::<&str>)?;
    let tracking = CheckMenuItem::with_id(app, "tracking", "Считать время в играх", true, true, None::<&str>)?;
    // Галочка включена = уведомления показываются (внутри флаг paused наоборот).
    let pause = CheckMenuItem::with_id(app, "pause", "Итоги игр, опыт и квесты", true, true, None::<&str>)?;
    let test_xp = MenuItem::with_id(app, "test_xp", "Тест: опыт", true, None::<&str>)?;
    let test_level = MenuItem::with_id(app, "test_level", "Тест: новый уровень", true, None::<&str>)?;
    let test_quest = MenuItem::with_id(app, "test_quest", "Тест: квест можно сдать", true, None::<&str>)?;
    let test_session = MenuItem::with_id(app, "test_session", "Тест: итог сессии", true, None::<&str>)?;
    let test_summary = MenuItem::with_id(app, "test_summary", "Тест: итог после игры одной плашкой", true, None::<&str>)?;
    let open_site = MenuItem::with_id(app, "open_site", "Открыть Big Backlog", true, None::<&str>)?;
    let open_overlay = MenuItem::with_id(app, "open_overlay", "Открыть оверлей", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Подключение…", true, None::<&str>)?;
    let saves_item = MenuItem::with_id(app, "saves", "Сохранения игр для вкладки «Прогресс»…", true, None::<&str>)?;
    let check_update = MenuItem::with_id(app, "check_update", "Проверить обновления", true, None::<&str>)?;
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(app, "autostart", "Запускать вместе с Windows", true, autostart_on, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    // Строки последних сессий: в меню их вставляет set_recent, когда придёт
    // первый ответ сервера (клик — игра на сайте).
    let recent: Vec<MenuItem<tauri::Wry>> = (0..RECENT_MENU)
        .map(|i| MenuItem::with_id(app, format!("recent{i}"), "", true, None::<&str>))
        .collect::<tauri::Result<_>>()?;
    let (sep1, sep2, sep3) = (
        PredefinedMenuItem::separator(app)?, PredefinedMenuItem::separator(app)?, PredefinedMenuItem::separator(app)?,
    );
    // Сборка для людей без тестовых плашек и оверлея; dev (cargo feature dev) со всем.
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&status, &now_item, &sep1, &tracking, &pause];
    if DEV {
        items.extend([&test_xp as &dyn tauri::menu::IsMenuItem<tauri::Wry>, &test_level, &test_quest, &test_session, &test_summary]);
    }
    items.extend([&sep2 as &dyn tauri::menu::IsMenuItem<tauri::Wry>, &open_site]);
    if DEV {
        items.push(&open_overlay);
    }
    items.extend([&settings as &dyn tauri::menu::IsMenuItem<tauri::Wry>, &saves_item, &autostart, &check_update, &sep3, &quit]);
    let menu = Menu::with_items(app, &items)?;
    *s.connect_item.lock().unwrap() = Some((menu.clone(), settings.clone(), true));
    *s.status_item.lock().unwrap() = Some(status);
    *s.now_item.lock().unwrap() = Some(now_item);
    {
        let mut r = s.recent.lock().unwrap();
        r.menu = Some(menu.clone());
        r.items = recent;
    }

    let shared = s.clone();
    TrayIconBuilder::with_id("tray")
        .icon(app.default_window_icon().cloned().expect("иконка приложения"))
        .tooltip(format!("BigBacklog Companion {}{}", update::VERSION, if DEV { " dev" } else { "" }))
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            let server = server_of(&shared.cfg.lock().unwrap());
            match event.id.as_ref() {
                "pause" => {
                    let now = !shared.paused.load(Ordering::SeqCst);
                    shared.paused.store(now, Ordering::SeqCst);
                    if now {
                        shared.pending.lock().unwrap().clear();
                    }
                    let _ = pause.set_checked(!now);
                }
                "tracking" => {
                    let on = !shared.tracking.load(Ordering::SeqCst);
                    shared.tracking.store(on, Ordering::SeqCst);
                    let _ = tracking.set_checked(on);
                    if !on {
                        shared.set_now("Слежение за играми выключено");
                    }
                }
                "test_xp" => push_test(&shared, "xp"),
                "test_level" => push_test(&shared, "level"),
                "test_quest" => push_test(&shared, "quest"),
                "test_session" => push_test(&shared, "session"),
                "test_summary" => push_test(&shared, "summary"),
                "open_site" => win::open_url(&server),
                "open_overlay" => win::open_url(&format!("{server}/overlay")),
                "settings" => {
                    let app = app.clone();
                    std::thread::spawn(move || open_settings(&app));
                }
                "saves" => {
                    let app = app.clone();
                    std::thread::spawn(move || open_saves(&app));
                }
                "autostart" => {
                    let launcher = app.autolaunch();
                    let on = launcher.is_enabled().unwrap_or(false);
                    let _ = if on { launcher.disable() } else { launcher.enable() };
                    let _ = autostart.set_checked(launcher.is_enabled().unwrap_or(false));
                }
                "check_update" => {
                    let s = shared.clone();
                    std::thread::spawn(move || update::check_manual(s));
                }
                "quit" => app.exit(0),
                id if id.starts_with("recent") => {
                    let i: usize = id["recent".len()..].parse().unwrap_or(usize::MAX);
                    let link = shared.recent.lock().unwrap().links.get(i).cloned().unwrap_or_default();
                    if link.starts_with('/') {
                        win::open_url(&format!("{server}{link}"));
                    }
                }
                _ => {}
            }
        })
        .build(app)?;
    Ok(())
}

/// `--scan`: что агент знает об установленных играх и что запущено прямо
/// сейчас — без сервера и без трея; для проверки опознавания на своей машине.
fn scan_report() {
    use std::io::Write;
    // writeln с игнором ошибки, а не println: обрезанный вывод (`| head`)
    // закрывает трубу, и println паниковал.
    let mut out = std::io::stdout().lock();
    let catalog = scan::Catalog::load();
    let _ = writeln!(out, "каталог: {} папок игр", catalog.roots_count());
    for g in &catalog.installed {
        let _ = writeln!(out, "  установлено  {:<9} {:<28} {}", g.source, g.id, g.name);
    }
    let mut scanner = scan::Scanner::default();
    let running = scanner.scan(&catalog);
    if running.is_empty() {
        let _ = writeln!(out, "запущенных игр нет");
    }
    for r in running {
        let _ = writeln!(
            out,
            "  запущено     {:<9} {:<28} {} | окно: {} | заголовок: {}",
            r.info.source, r.info.id, r.info.name, r.has_window, r.title.as_deref().unwrap_or("—")
        );
    }
    let addr = load_config().xbox360.trim().to_string();
    if !addr.is_empty() {
        let line = match xbox360::probe(&addr) {
            Ok(Some(r)) => format!("запущено     {:<9} {:<28} {}", r.info.source, r.info.id, r.info.name),
            Ok(None) => "дашборд или оболочка, игры нет".to_string(),
            Err(e) => format!("не отвечает: {e}"),
        };
        let _ = writeln!(out, "xbox 360 {addr}: {line}");
    }
}

fn main() {
    if std::env::args().any(|a| a == "--scan") {
        scan_report();
        return;
    }
    // Запуск после самообновления: старый процесс еще выходит, ждем его (иначе single-instance).
    update::wait_pid_from_args();
    // Новый человек: без WebView2 окна не откроются — говорим сразу; из «Загрузок» — переезжаем.
    if !install::webview2_ok() {
        log("WebView2 нет, предлагаю установить");
        install::webview2_prompt();
        return;
    }
    if install::relocate() {
        return;
    }
    let shared = Arc::new(Shared::new(load_config()));
    let setup_shared = shared.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let app = app.clone();
            std::thread::spawn(move || open_settings(&app));
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            toast_data, toast_assets, toast_show, toast_done, get_config, save_config, close_settings, pair_start, pair_poll,
            get_saves, set_save_sync, send_save_now, set_save_dir, open_save_dir, close_saves
        ])
        .setup(move |app| {
            let s = setup_shared;
            *s.app.lock().unwrap() = Some(app.handle().clone());
            build_tray(app.handle(), &s)?;
            // Автозапуск, если включен, всегда на этот exe: после переезда из «Загрузок» (install.rs)
            // запись в реестре указывала бы на старое место.
            let launcher = app.autolaunch();
            if launcher.is_enabled().unwrap_or(false) {
                let _ = launcher.enable();
            }
            let watch = app.handle().clone();
            std::thread::spawn(move || main_watchdog(watch));
            let poll = s.clone();
            std::thread::spawn(move || poll_loop(poll));
            let (handle, present) = (app.handle().clone(), s.clone());
            std::thread::spawn(move || present_loop(handle, present));
            let upd = s.clone();
            std::thread::spawn(move || update::check_loop(upd));
            if let Some(from) = update::updated_from_args() {
                let s2 = s.clone();
                std::thread::spawn(move || notice_updated(s2, from));
            }
            let track = s.clone();
            std::thread::spawn(move || track_loop(track));
            // Прошитый Xbox 360 — только если адрес задан в конфиге и слежение
            // за играми не на паузе.
            let x360 = s.clone();
            let x360_up = s.clone();
            std::thread::spawn(move || xbox360::poll_loop(
                move || {
                    let addr = x360.cfg.lock().unwrap().xbox360.trim().to_string();
                    (!addr.is_empty() && x360.tracking.load(Ordering::SeqCst)).then_some(addr)
                },
                // Достижения из профиля приставки — на сайт (app/freeboot_gpd.py).
                move |body: &Value| {
                    let cfg = x360_up.cfg.lock().unwrap().clone();
                    let token = cfg.token.trim().to_string();
                    if token.is_empty() {
                        return None;
                    }
                    post_json(&server_of(&cfg), &token, "/api/agent/x360/gpd", body).ok()
                },
            ));
            if s.cfg.lock().unwrap().token.trim().is_empty() && !s.demo.load(Ordering::SeqCst) {
                open_settings(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("не удалось запустить приложение")
        .run(|_app, event| {
            // Закрылось последнее окно (плашка, настройки) — агент живёт в
            // трее дальше. Выход только из меню (app.exit даёт code).
            if let RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_like_the_site() {
        assert_eq!(duration(45), "45 мин");
        assert_eq!(duration(120), "2 ч");
        assert_eq!(duration(85), "1 ч 25 мин");
    }

    #[test]
    fn menu_line_escapes_ampersand_and_trims_name() {
        assert_eq!(menu_text("Ratchet & Clank"), "Ratchet && Clank");
        let now = unix_now();
        let line = session_line(&json!({
            "name": "A very long game name that does not fit into the tray menu",
            "minutes": 95, "started_at": now - 95 * 60, "ended_at": now,
        }));
        assert!(line.starts_with("A very long game name that does not f…"), "{line}");
        assert!(line.contains(" · 1 ч 35 мин · "), "{line}");
        assert!(line.contains('–'), "{line}");
    }

    #[test]
    fn local_time_roundtrip() {
        let now = unix_now();
        assert!(win::local_time(now).is_some());
        assert_eq!(day_label(now), "сегодня");
        assert_eq!(day_label(now - 86_400), "вчера");
        // 1 января 2026, полдень UTC — где бы ни стоял часовой пояс, это 1 янв.
        assert_eq!(day_label(1_767_268_800), "1 янв");
    }
}
