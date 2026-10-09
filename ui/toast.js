// Окно одной плашки. Агент (src-tauri/src/main.rs) создаёт его на показ,
// отдаёт данные (toast_data) и вид плашек (toast_assets — static/popups.js и
// popups.css с сервера, копия на диске), по toast_show выводит окно без
// активации и сносит его по toast_done. Кадры — те же функции, что у сайта
// (buildXpPopup и компания в popups.js): правится вид на сайте — через
// несколько минут меняется и здесь, без пересборки агента.
"use strict";

const { invoke } = window.__TAURI__.core;
const root = document.getElementById("meta-popup-root");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function preload(urls) {
  return Promise.all(urls.map((src) => new Promise((resolve) => {
    const img = new Image();
    img.onload = img.onerror = resolve;
    img.src = src;
  })));
}

function done() {
  root.innerHTML = "";
  invoke("toast_done");
}

(async () => {
  const [d, assets] = await Promise.all([invoke("toast_data"), invoke("toast_assets")]);
  if (!d || !d.type || !assets || !assets.js || !assets.css) return done();
  const style = document.createElement("style");
  style.textContent = assets.css;
  document.head.appendChild(style);
  // Классический <script> с текстом: его объявления верхнего уровня ложатся
  // в общую область видимости страницы — как у сайта, где popups.js стоит
  // перед app.js.
  const script = document.createElement("script");
  script.textContent = assets.js;
  document.head.appendChild(script);
  const build = {
    xp: window.buildXpPopup, quest: window.buildQuestReadyPopup, session: window.buildSessionPopup,
    notice: window.buildNoticePopup, summary: window.buildSummaryPopup,
  }[d.type];
  if (typeof build !== "function") return done();
  let start;
  try {
    window.popupSetAssetBase(d.server);
    const data = d.type === "quest" ? d.quest || {} : d.type === "session" ? d.session || {}
      : d.type === "notice" ? d.notice || {} : d;
    start = build(root, data, { onDone: () => invoke("toast_done") });
  } catch {
    return done();
  }
  // Окно выводится, только когда шрифты и картинки уже в памяти: иначе
  // первым кадром мелькнула бы пустая плашка.
  const images = [...window.popupImageUrls(root), ...(start.images || [])];
  await Promise.race([Promise.all([document.fonts.ready, preload(images)]), sleep(1500)]);
  // Не успевшая картинка — как не нашедшаяся: обложка сессии уходит в орб
  // платформы (обработчик ошибки — в кадре), а не висит пустой рамкой. Steam
  // CDN бывает медленным, а прокси сайта (/api/img) агенту без куки закрыт.
  root.querySelectorAll("img").forEach((img) => {
    if (!img.complete || !img.naturalWidth) img.dispatchEvent(new Event("error"));
  });
  await invoke("toast_show");
  requestAnimationFrame(() => start());
})();
