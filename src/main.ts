// Ozan - Ana arayüz mantığı (TypeScript, harici UI kütüphanesi yok).
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  api,
  ApoHealthReport,
  AudioStatus,
  CapturedKey,
  COLOR_PALETTE,
  DEFAULT_DEVICE,
  DeviceList,
  errMsg,
  HotkeyBinding,
  ImportProgress,
  LibraryData,
  Sound,
  SUPPORTED_EXTS,
} from "./api";
import { attachCardDrag } from "./dnd";
import { $, confirmModal, h, icons, iconSpan, isModalOpen, promptModal, showModal, toast } from "./ui";

const ALL_VIEW = "__all__";

let lib: LibraryData = {
  version: 2,
  sounds: [],
  folders: [],
  settings: {
    volume: 0.8,
    localPreview: false,
    micDevice: null,
    broadcastDevice: null,
    monitorDevice: null,
    normalizeChoice: null,
    panicHotkey: { code: "VK:35", label: "End", passthrough: true },
    onboardingDone: false,
    pttEnabled: false,
    pttHotkey: null,
    muteMicOnPlay: false,
  },
};
let activeFolder: string = ALL_VIEW;
let query = "";
const selected = new Set<string>();
let importing = false;
let captureCallback: ((k: CapturedKey) => void) | null = null;

// ---------------------------------------------------------------- Başlatma

async function init() {
  bindControls();
  attachCardDrag($("#grid"), {
    canReorder: () => query.length === 0,
    onClick: handleCardClick,
    onReorder: handleReorder,
    onDropToFolder: handleDropToFolder,
    dragCount: (id) => (selected.has(id) ? selected.size : 1),
  });
  await bindFileDrop();
  await listen<ImportProgress>("import-progress", (e) => renderProgress(e.payload));
  await listen<CapturedKey>("hotkey-captured", (e) => {
    if (captureCallback) {
      const cb = captureCallback;
      captureCallback = null;
      cb(e.payload);
    }
  });
  await listen<string>("hotkey-played", (e) => flashCard(e.payload));
  await listen<void>("panic-triggered", () => {
    toast("Panic tuşu: Çalan tüm sesler durduruldu", "info", 1600);
  });

  try {
    lib = await api.getLibrary();
  } catch (e) {
    toast(`Kütüphane yüklenemedi: ${errMsg(e)}`, "error");
  }
  syncTopbar();
  renderAll();
  await refreshDevices();
  await refreshApoStatus();

  // Gerçek zamanlı ilerleme çubuğu ve mikrofon durumu için 80 ms periyot
  setInterval(pollStatus, 80);
  // APO / IMMNotificationClient cihaz değişimi ve bütünlük kontrolü
  setInterval(refreshApoStatus, 5000);

  if (!lib.settings.onboardingDone) {
    void showOnboardingModal();
  }
}

// ---------------------------------------------------------------- Görünüm verisi

function soundMap(): Map<string, Sound> {
  return new Map(lib.sounds.map((s) => [s.id, s]));
}

/** Aktif sekmedeki sesleri, favoriler en üstte olacak şekilde döndürür. */
function visibleSounds(): Sound[] {
  const map = soundMap();
  let list: Sound[];
  if (activeFolder === ALL_VIEW) {
    list = [...lib.sounds];
  } else {
    const f = lib.folders.find((x) => x.id === activeFolder);
    if (!f) {
      activeFolder = ALL_VIEW;
      list = [...lib.sounds];
    } else {
      list = f.sounds.map((id) => map.get(id)).filter((s): s is Sound => Boolean(s));
    }
  }
  const q = query.trim().toLocaleLowerCase("tr");
  if (q) {
    list = list.filter((s) => s.name.toLocaleLowerCase("tr").includes(q));
  }
  const favs = list.filter((s) => s.favorite);
  const rest = list.filter((s) => !s.favorite);
  return [...favs, ...rest];
}

// ---------------------------------------------------------------- Çizim

function renderAll() {
  renderSidebar();
  renderGrid();
  renderSelectionBar();
  renderSettingsState();
}

function renderSidebar() {
  const navAll = $("#nav-all");
  navAll.innerHTML = "";
  navAll.append(
    h(
      "div",
      {
        class: `nav-item ${activeFolder === ALL_VIEW ? "active" : ""}`,
        onclick: () => switchFolder(ALL_VIEW),
      },
      iconSpan(icons.all),
      h("span", { class: "nav-name" }, "Tüm Sesler"),
      h("span", { class: "nav-count" }, String(lib.sounds.length)),
    ),
  );

  const foldersEl = $("#folders");
  foldersEl.innerHTML = "";
  for (const f of lib.folders) {
    const delBtn = h(
      "span",
      {
        class: "nav-del",
        title: "Klasörü sil (sesler silinmez)",
        onclick: (e: MouseEvent) => {
          e.stopPropagation();
          void removeFolder(f.id, f.name);
        },
      },
      iconSpan(icons.close),
    );
    const item = h(
      "div",
      {
        class: `nav-item ${activeFolder === f.id ? "active" : ""}`,
        dataset: { folder: f.id },
        onclick: () => switchFolder(f.id),
        ondblclick: () => void renameFolderPrompt(f.id, f.name),
        title: "Çift tıkla: yeniden adlandır · Kartları buraya sürükleyebilirsiniz",
      },
      iconSpan(icons.folder),
      h("span", { class: "nav-name" }, f.name),
      h("span", { class: "nav-count has-del" }, String(f.sounds.length)),
      delBtn,
    );
    foldersEl.append(item);
  }
}

function renderGrid() {
  const titleEl = $("#view-title");
  const countEl = $("#view-count");
  const grid = $("#grid");
  const empty = $("#empty");

  const folder = lib.folders.find((f) => f.id === activeFolder);
  titleEl.textContent = folder ? folder.name : "Tüm Sesler";

  const list = visibleSounds();
  countEl.textContent = `${list.length} ses`;

  const visIds = new Set(list.map((s) => s.id));
  for (const id of [...selected]) {
    if (!visIds.has(id)) selected.delete(id);
  }

  grid.innerHTML = "";
  if (list.length === 0) {
    empty.classList.remove("hidden");
    empty.innerHTML = "";
    const msg = query
      ? `"${query}" için eşleşen ses yok.`
      : folder
        ? "Bu klasör boş. Ses kartlarını buraya sürükleyin veya Ctrl+Tık ile seçip ekleyin."
        : "Henüz ses yok. Ses dosyalarını (.opus, .mp3, .wav, .ogg, .flac) pencereye sürükleyip bırakın.";
    empty.append(h("div", { class: "big" }, "♪"), h("div", {}, msg));
    return;
  }
  empty.classList.add("hidden");

  for (const s of list) {
    grid.append(buildSoundCard(s, Boolean(folder)));
  }
}

function buildSoundCard(s: Sound, inCustomFolder: boolean): HTMLElement {
  const cardColor = s.color || (s.favorite ? "#ffc94d" : "");
  const gainPct = Math.round((s.userGain ?? 1) * 100);

  // 1. Üst satır: İsim + Araç butonları (Renk Etiketi, Ses Kırpma, Favori Yıldızı, Klasörden Çıkar)
  const colorBtn = h("button", {
    class: "color-dot-btn",
    title: "Özel renk etiketi seç",
    onclick: (e: MouseEvent) => {
      e.stopPropagation();
      void openColorModal(s);
    },
  });
  colorBtn.style.setProperty("--dot-color", s.color || "#4a5166");

  const trimBtn = h(
    "button",
    {
      class: "card-tool-btn",
      title: "Sesi kırp (Başlangıç / Bitiş seç, 1 Kerelik Çal veya Kalıcı Kaydet)",
      onclick: (e: MouseEvent) => {
        e.stopPropagation();
        void openTrimModal(s);
      },
    },
    iconSpan(icons.scissors),
  );

  const starBtn = h(
    "button",
    {
      class: `card-tool-btn star ${s.favorite ? "on" : ""}`,
      title: s.favorite ? "Favorilerden çıkar" : "Favorilere sabitle (en üste)",
      onclick: (e: MouseEvent) => {
        e.stopPropagation();
        void toggleFavorite(s);
      },
    },
    iconSpan(icons.star),
  );

  const unlinkBtn =
    inCustomFolder &&
    h(
      "button",
      {
        class: "card-tool-btn unlink",
        title: "Bu klasörden çıkar (Tüm Sesler'den silinmez)",
        onclick: (e: MouseEvent) => {
          e.stopPropagation();
          void unlinkFromCurrentFolder([s.id]);
        },
      },
      iconSpan(icons.close),
    );

  const topRow = h(
    "div",
    { class: "card-top" },
    h("div", { class: "card-name" }, s.name),
    h("div", { class: "card-tools" }, colorBtn, trimBtn, starBtn, unlinkBtn),
  );

  // 2. Orta satır: %0 - %200 Bireysel Ses Kazanç Çubuğu
  const gainValEl = h(
    "span",
    {
      class: "card-gain-val no-drag",
      title: "Bireysel kazanç (%0 - %200). Çift tıkla: %100 yap",
      ondblclick: (e: MouseEvent) => {
        e.stopPropagation();
        s.userGain = 1.0;
        gainSlider.value = "100";
        gainSlider.style.setProperty("--gain-fill", "50%");
        gainValEl.textContent = "100%";
        void api.setSoundGain(s.id, 1.0, true);
      },
    },
    `${gainPct}%`,
  );

  const gainSlider = h("input", {
    class: "card-gain-slider no-drag",
    type: "range",
    min: "0",
    max: "200",
    step: "1",
    value: String(gainPct),
    title: `Bireysel ses kazancı: %${gainPct}`,
  }) as HTMLInputElement;
  gainSlider.style.setProperty("--gain-fill", `${gainPct / 2}%`);

  gainSlider.addEventListener("input", (e) => {
    e.stopPropagation();
    const pct = Number(gainSlider.value);
    s.userGain = pct / 100;
    gainSlider.style.setProperty("--gain-fill", `${pct / 2}%`);
    gainValEl.textContent = `${pct}%`;
    void api.setSoundGain(s.id, s.userGain, false);
  });
  gainSlider.addEventListener("change", (e) => {
    e.stopPropagation();
    const pct = Number(gainSlider.value);
    s.userGain = pct / 100;
    void api.setSoundGain(s.id, s.userGain, true);
  });

  const gainRow = h("div", { class: "card-gain-row no-drag" }, gainSlider, gainValEl);

  // 3. Alt satır: Dinamik Süre Etiketi + Kısayol/Passthrough + Tekrar Modu
  const ext = s.file.split(".").pop()?.toUpperCase() ?? "";
  const durLabel = h(
    "span",
    {
      class: "duration-label",
      dataset: { role: "dur", total: String(s.duration) },
      title: "Dosyanın gerçek süresi",
    },
    formatDuration(s.duration),
  );

  const hk = s.hotkey;
  const hkBtn = h(
    "button",
    {
      class: `chip-btn ${hk ? (hk.passthrough ? "has-key" : "has-key block-key") : ""}`,
      title: hk
        ? `Kısayol: ${hk.label} (${hk.passthrough ? "Passthrough: Oyuna iletilir" : "Passthrough Kapalı: Oyundan gizlenir"})\nDeğiştirmek için tıklayın`
        : "Kısayol ata (Klavye, XBUTTON1/2 veya MMO fare tuşu)",
      onclick: (e: MouseEvent) => {
        e.stopPropagation();
        void openSoundHotkeyModal(s);
      },
    },
    iconSpan(icons.key),
    h("span", {}, hk ? hk.label : "Tuş"),
  );

  const isOverlap = s.replayMode === "overlap";
  const replayBtn = h(
    "button",
    {
      class: `chip-btn ${isOverlap ? "mode-overlap" : ""}`,
      title: isOverlap
        ? "Tekrar Modu: Üst Üste Bindir (Aynı tuşa tekrar basıldığında üst üste çalar, maks. 5)\nDeğiştirmek için tıklayın"
        : "Tekrar Modu: Baştan Başlat (Aynı tuşa tekrar basıldığında baştan başlar)\nDeğiştirmek için tıklayın",
      onclick: (e: MouseEvent) => {
        e.stopPropagation();
        void toggleReplayMode(s);
      },
    },
    iconSpan(isOverlap ? icons.overlap : icons.restart),
    h("span", {}, isOverlap ? "Üst Üste" : "Baştan"),
  );

  const bottomRow = h(
    "div",
    { class: "card-bottom" },
    h(
      "div",
      { class: "card-meta-left" },
      h("span", { class: "badge" }, ext),
      durLabel,
      s.normalized && h("span", { class: "badge", title: "-1 dB tepe seviyesine dengelendi" }, "-1dB"),
    ),
    h("div", { class: "card-meta-right" }, hkBtn, replayBtn),
  );

  // 4. Kart altı gerçek zamanlı ilerleme çubuğu
  const progressBar = h(
    "div",
    { class: "card-progress" },
    h("div", { class: "card-progress-fill", dataset: { role: "prog" } }),
  );

  const card = h(
    "div",
    {
      class: `card ${selected.has(s.id) ? "selected" : ""}`,
      dataset: { id: s.id },
      title: `${s.name}\nSol tık: Oynat · Ctrl+Sol tık: Çoklu seç · Sürükle: Sırala · Sağ tık / F2: Adlandır`,
      oncontextmenu: (e: MouseEvent) => {
        e.preventDefault();
        void renameSoundPrompt(s);
      },
    },
    topRow,
    gainRow,
    bottomRow,
    progressBar,
  );
  if (cardColor) {
    card.style.setProperty("--card-color", cardColor);
  }
  return card;
}

function renderSelectionBar() {
  const bar = $("#selection-bar");
  const n = selected.size;
  if (n === 0) {
    bar.classList.add("hidden");
    $("#sel-add-menu").classList.add("hidden");
    return;
  }
  bar.classList.remove("hidden");
  $("#sel-count").textContent = `${n} ses seçildi`;
  const removeBtn = $("#sel-remove");
  if (activeFolder === ALL_VIEW) removeBtn.classList.add("hidden");
  else removeBtn.classList.remove("hidden");
}

function renderSettingsState() {
  const s = lib.settings;
  const c = s.normalizeChoice;
  $("#normalize-pref").textContent =
    c === null
      ? "Normalize: Her seferinde sor"
      : c
        ? "Normalize: Her zaman (-1 dB)"
        : "Normalize: Her zaman ham bırak";

  ($("#mute-mic-on-play") as HTMLInputElement).checked = s.muteMicOnPlay;
  ($("#ptt-enabled") as HTMLInputElement).checked = s.pttEnabled;
  $("#ptt-key-btn").textContent = s.pttHotkey ? s.pttHotkey.label : "Tuş Ata…";

  const panic = s.panicHotkey ?? { code: "VK:35", label: "End", passthrough: true };
  $("#panic-key-btn").textContent = panic.label;
  $("#panic-badge").textContent = panic.label;
  ($("#panic-passthrough") as HTMLInputElement).checked = panic.passthrough;
}

function renderProgress(p: ImportProgress) {
  const box = $("#progress");
  if (p.total === 0 || p.done >= p.total) {
    box.classList.add("hidden");
    return;
  }
  box.classList.remove("hidden");
  box.querySelector(".progress-text")!.textContent = `İçe aktarılıyor (${p.done + 1}/${p.total}): ${p.name}`;
  const pct = Math.round((p.done / p.total) * 100);
  (box.querySelector(".progress-fill") as HTMLElement).style.width = `${pct}%`;
}

// ---------------------------------------------------------------- Kart İşlemleri

function switchFolder(id: string) {
  if (activeFolder === id) return;
  activeFolder = id;
  selected.clear();
  renderAll();
}

function flashCard(id: string) {
  const card = document.querySelector<HTMLElement>(`.card[data-id="${id}"]`);
  if (!card) return;
  card.classList.remove("flash");
  void card.offsetWidth;
  card.classList.add("flash");
}

async function handleCardClick(id: string, e: PointerEvent) {
  if (e.ctrlKey || e.metaKey) {
    if (selected.has(id)) selected.delete(id);
    else selected.add(id);
    const card = document.querySelector<HTMLElement>(`.card[data-id="${id}"]`);
    card?.classList.toggle("selected", selected.has(id));
    renderSelectionBar();
    return;
  }
  if (selected.size > 0) {
    selected.clear();
    document.querySelectorAll(".card.selected").forEach((c) => c.classList.remove("selected"));
    renderSelectionBar();
  }
  flashCard(id);
  try {
    await api.playSound(id);
  } catch (err) {
    toast(`Oynatılamadı: ${errMsg(err)}`, "error");
  }
}

async function toggleFavorite(s: Sound) {
  s.favorite = !s.favorite;
  renderGrid();
  try {
    await api.setFavorite(s.id, s.favorite);
  } catch (e) {
    s.favorite = !s.favorite;
    renderGrid();
    toast(errMsg(e), "error");
  }
}

async function toggleReplayMode(s: Sound) {
  const next = s.replayMode === "overlap" ? "restart" : "overlap";
  s.replayMode = next;
  renderGrid();
  try {
    await api.setSoundReplayMode(s.id, next);
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

async function openColorModal(s: Sound) {
  let chosen = s.color || "";
  const customInput = h("input", {
    type: "color",
    value: chosen || "#7c6cff",
    title: "Özel renk seç",
    onchange: () => {
      chosen = customInput.value;
      updateSwatches();
    },
  }) as HTMLInputElement;

  const swatches = COLOR_PALETTE.map((c) =>
    h("button", {
      class: `color-swatch ${chosen === c.value ? "active" : ""}`,
      title: c.label,
      style: `background: ${c.value || "#2a2e3a"}`,
      onclick: () => {
        chosen = c.value;
        if (c.value) customInput.value = c.value;
        updateSwatches();
      },
    }),
  );

  function updateSwatches() {
    swatches.forEach((el, idx) => {
      el.classList.toggle("active", COLOR_PALETTE[idx].value === chosen);
    });
  }

  const r = await showModal({
    title: `"${s.name}" Renk Etiketi`,
    body: [
      h("div", { class: "color-palette-grid" }, ...swatches),
      h(
        "div",
        { class: "field-row" },
        h("span", { class: "small muted" }, "Veya özel bir renk seçin:"),
        customInput,
      ),
    ],
    buttons: [
      { id: "cancel", label: "Vazgeç" },
      { id: "ok", label: "Uygula", kind: "primary" },
    ],
  });
  if (r !== "ok") return;
  s.color = chosen;
  renderGrid();
  try {
    await api.setSoundColor(s.id, chosen);
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

// ---------------------------------------------------------------- Kısayol Yakalama Penceresi

async function openSoundHotkeyModal(s: Sound) {
  let current: HotkeyBinding | null = s.hotkey ? { ...s.hotkey } : null;
  const passChk = h("input", {
    type: "checkbox",
    checked: current ? current.passthrough : true,
  }) as HTMLInputElement;

  const box = h(
    "div",
    { class: "capture-box waiting" },
    current ? `Atanan Tuş: ${current.label}` : "Herhangi bir klavye veya fare (MMO dahil) tuşuna basın…",
  );

  const startListen = async () => {
    box.classList.add("waiting");
    box.textContent = "Dinleniyor… Klavye veya fare üzerindeki istediğiniz tuşa basın";
    captureCallback = (cap) => {
      current = {
        code: cap.code,
        label: cap.label,
        passthrough: passChk.checked,
      };
      box.classList.remove("waiting");
      box.textContent = `Algılandı: ${cap.label}`;
    };
    await api.startHotkeyCapture();
  };

  const r = await showModal({
    title: `"${s.name}" Kısayol Ataması`,
    body: [
      box,
      h(
        "p",
        { class: "muted small" },
        "Tüm klavye tuşları, XBUTTON1/XBUTTON2 ve çok tuşlu MMO farelerinin tüm ek tuşları (Raw Input / HID) desteklenir.",
      ),
      h(
        "label",
        { class: "check" },
        passChk,
        h("span", {}, "Passthrough (Tuş basımını aktif oyuna/uygulamaya da ilet)"),
      ),
      h(
        "div",
        { class: "field-row" },
        h(
          "button",
          {
            class: "btn btn-ghost small",
            onclick: () => void startListen(),
          },
          "Yeniden Tuş Dinle",
        ),
        h(
          "button",
          {
            class: "btn btn-danger small",
            onclick: () => {
              current = null;
              captureCallback = null;
              void api.cancelHotkeyCapture();
              box.classList.remove("waiting");
              box.textContent = "Kısayol kaldırıldı";
            },
          },
          "Kısayolu Kaldır",
        ),
      ),
    ],
    buttons: [
      { id: "cancel", label: "Vazgeç" },
      { id: "ok", label: "Kaydet", kind: "primary" },
    ],
    onOpen: () => {
      void startListen();
    },
    onClose: () => {
      captureCallback = null;
      void api.cancelHotkeyCapture();
    },
  });

  if (r !== "ok") return;
  if (current) {
    current.passthrough = passChk.checked;
  }
  s.hotkey = current;
  renderGrid();
  try {
    await api.setSoundHotkey(s.id, current);
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

async function captureSingleKeyModal(title: string, currentLabel: string): Promise<CapturedKey | null> {
  let captured: CapturedKey | null = null;
  const box = h(
    "div",
    { class: "capture-box waiting" },
    `Dinleniyor… (Mevcut: ${currentLabel || "Yok"})`,
  );

  const r = await showModal({
    title,
    body: [
      box,
      h(
        "p",
        { class: "muted small" },
        "Atamak istediğiniz klavye tuşuna, XBUTTON1/XBUTTON2 veya MMO fare tuşuna basın.",
      ),
    ],
    buttons: [
      { id: "cancel", label: "Vazgeç" },
      { id: "ok", label: "Onayla", kind: "primary" },
    ],
    onOpen: (_, close) => {
      captureCallback = (cap) => {
        captured = cap;
        box.classList.remove("waiting");
        box.textContent = `Seçilen Tuş: ${cap.label}`;
        setTimeout(() => close("ok"), 180);
      };
      void api.startHotkeyCapture();
    },
    onClose: () => {
      captureCallback = null;
      void api.cancelHotkeyCapture();
    },
  });
  return r === "ok" ? captured : null;
}

// ---------------------------------------------------------------- Ses Kırpma Modülü (GÖREV 4)

async function openTrimModal(s: Sound) {
  let wf: { peaks: number[]; duration: number };
  try {
    wf = await api.getWaveform(s.id);
  } catch (e) {
    toast(`Dalga formu okunamadı: ${errMsg(e)}`, "error");
    return;
  }
  const total = Math.max(0.05, wf.duration || s.duration || 1);
  s.duration = total;

  let startSec = 0;
  let endSec = total;

  const canvas = h("canvas", { class: "trim-canvas", width: "580", height: "130" }) as HTMLCanvasElement;
  const shadeLeft = h("div", { class: "trim-shade" });
  const shadeRight = h("div", { class: "trim-shade" });
  const handleStart = h("div", { class: "trim-handle", title: "Başlangıç noktası" });
  const handleEnd = h("div", { class: "trim-handle", title: "Bitiş noktası" });
  const stage = h("div", { class: "trim-stage" }, canvas, shadeLeft, shadeRight, handleStart, handleEnd);

  const startInput = h("input", {
    class: "input",
    type: "number",
    step: "0.05",
    min: "0",
    max: String(total),
    value: "0.00",
  }) as HTMLInputElement;

  const endInput = h("input", {
    class: "input",
    type: "number",
    step: "0.05",
    min: "0",
    max: String(total),
    value: total.toFixed(2),
  }) as HTMLInputElement;

  const selLenEl = h("strong", {}, formatDuration(total));

  const drawWaveform = () => {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const w = canvas.width;
    const ht = canvas.height;
    ctx.clearRect(0, 0, w, ht);
    const peaks = wf.peaks;
    const barW = w / peaks.length;
    const mid = ht / 2;
    for (let i = 0; i < peaks.length; i++) {
      const t = (i / peaks.length) * total;
      const inRange = t >= startSec && t <= endSec;
      ctx.fillStyle = inRange ? s.color || "#7c6cff" : "#3a3f50";
      const bh = Math.max(3, peaks[i] * (ht * 0.84));
      ctx.fillRect(i * barW + 0.5, mid - bh / 2, Math.max(1.5, barW - 1), bh);
    }
  };

  const syncVisuals = (fromInputs = false) => {
    if (!fromInputs) {
      startInput.value = startSec.toFixed(2);
      endInput.value = endSec.toFixed(2);
    }
    const leftPct = (startSec / total) * 100;
    const rightPct = (endSec / total) * 100;
    shadeLeft.style.left = "0%";
    shadeLeft.style.width = `${leftPct}%`;
    shadeRight.style.left = `${rightPct}%`;
    shadeRight.style.width = `${Math.max(0, 100 - rightPct)}%`;
    handleStart.style.left = `calc(${leftPct}% - 2px)`;
    handleEnd.style.left = `calc(${rightPct}% - 2px)`;
    selLenEl.textContent = `${formatDuration(endSec - startSec)} (${(endSec - startSec).toFixed(2)} sn)`;
    drawWaveform();
  };

  startInput.addEventListener("input", () => {
    const v = parseFloat(startInput.value);
    if (!isNaN(v)) {
      startSec = Math.max(0, Math.min(v, endSec - 0.05));
      syncVisuals(true);
    }
  });
  endInput.addEventListener("input", () => {
    const v = parseFloat(endInput.value);
    if (!isNaN(v)) {
      endSec = Math.min(total, Math.max(v, startSec + 0.05));
      syncVisuals(true);
    }
  });

  // Dalga formu üzerinde fareyle tutamaç sürükleme
  let draggingWhich: "start" | "end" | null = null;
  stage.addEventListener("pointerdown", (e: PointerEvent) => {
    const r = stage.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
    const clickedSec = ratio * total;
    draggingWhich = Math.abs(clickedSec - startSec) <= Math.abs(clickedSec - endSec) ? "start" : "end";
    stage.setPointerCapture(e.pointerId);
    updateDrag(e);
  });
  const updateDrag = (e: PointerEvent) => {
    if (!draggingWhich) return;
    const r = stage.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
    const sec = ratio * total;
    if (draggingWhich === "start") {
      startSec = Math.max(0, Math.min(sec, endSec - 0.05));
    } else {
      endSec = Math.min(total, Math.max(sec, startSec + 0.05));
    }
    syncVisuals();
  };
  stage.addEventListener("pointermove", updateDrag);
  stage.addEventListener("pointerup", () => {
    draggingWhich = null;
  });

  await showModal({
    title: `Ses Kırpma Modülü — ${s.name}`,
    wide: true,
    body: [
      stage,
      h(
        "div",
        { class: "trim-inputs" },
        h("label", { class: "trim-stat" }, h("span", { class: "muted" }, "Başlangıç (sn)"), startInput),
        h("label", { class: "trim-stat" }, h("span", { class: "muted" }, "Bitiş (sn)"), endInput),
        h("div", { class: "trim-stat" }, h("span", { class: "muted" }, "Seçili Süre"), selLenEl),
      ),
      h(
        "p",
        { class: "muted small" },
        "Başlangıç ve bitiş çizgilerini fareyle sürükleyerek veya saniye kutularından aralığı belirleyin.",
      ),
    ],
    buttons: [
      { id: "cancel", label: "Kapat" },
      {
        id: "play-once",
        label: "▶ 1 Kerelik Çal",
        kind: "ghost",
        onClick: async () => {
          try {
            await api.playSoundRange(s.id, startSec, endSec);
          } catch (e) {
            toast(`Oynatılamadı: ${errMsg(e)}`, "error");
          }
        },
      },
      {
        id: "save-trimmed",
        label: "✂ Kırpılmış Şekilde Kaydet",
        kind: "primary",
        onClick: async (close) => {
          try {
            const updated = await api.saveTrimmedSound(s.id, startSec, endSec);
            const idx = lib.sounds.findIndex((x) => x.id === s.id);
            if (idx >= 0) lib.sounds[idx] = updated;
            renderGrid();
            toast(`"${s.name}" kırpıldı ve kalıcı olarak kaydedildi (${formatDuration(updated.duration)})`, "ok");
            close("save-trimmed");
          } catch (e) {
            toast(`Kırpma hatası: ${errMsg(e)}`, "error");
          }
        },
      },
    ],
    onOpen: () => {
      syncVisuals();
    },
  });
}

// ---------------------------------------------------------------- İlk Açılış Kurulum Ekranı (GÖREV 5)

async function showOnboardingModal() {
  let pttEnabled = lib.settings.pttEnabled;
  let pttKey: HotkeyBinding | null = lib.settings.pttHotkey ? { ...lib.settings.pttHotkey } : null;
  let muteMic = lib.settings.muteMicOnPlay;

  const pttChk = h("input", {
    type: "checkbox",
    checked: pttEnabled,
    onchange: () => {
      pttEnabled = pttChk.checked;
      pttAssignRow.classList.toggle("hidden", !pttEnabled);
    },
  }) as HTMLInputElement;

  const pttKeyBtn = h(
    "button",
    {
      class: "btn btn-primary small",
      onclick: async () => {
        pttKeyBtn.textContent = "Tuşa basın…";
        captureCallback = (cap) => {
          pttKey = { code: cap.code, label: cap.label, passthrough: true };
          pttKeyBtn.textContent = `Atanan Tuş: ${cap.label}`;
        };
        await api.startHotkeyCapture();
      },
    },
    pttKey ? `Atanan Tuş: ${pttKey.label}` : "Bas-Konuş Tuşu Ata…",
  );

  const pttAssignRow = h(
    "div",
    { class: `field-row ${pttEnabled ? "" : "hidden"}` },
    h("span", { class: "small muted" }, "Ses çaldığı sürece SendInput ile basılı tutulacak tuş:"),
    pttKeyBtn,
  );

  const muteChk = h("input", {
    type: "checkbox",
    checked: muteMic,
    onchange: () => {
      muteMic = muteChk.checked;
    },
  }) as HTMLInputElement;

  await showModal({
    title: "Ozan — İlk Açılış Kurulumu",
    wide: true,
    cancelId: "save",
    body: [
      h(
        "p",
        { class: "muted" },
        "Ozan'ı kullanım şeklinize göre yapılandırmak için aşağıdaki iki soruyu yanıtlayın. Her iki ayarı da istediğiniz zaman Ayarlar (⚙) menüsünden değiştirebilirsiniz.",
      ),
      h(
        "div",
        { class: "onboard-card" },
        h(
          "label",
          { class: "check" },
          pttChk,
          h("strong", {}, "1) Bas-Konuş (Push-to-Talk) kullanılıyor mu?"),
        ),
        h(
          "p",
          { class: "muted small" },
          "Aktif edilirse, bir ses efekti çaldığı sürece arka planda SendInput ile belirlediğiniz Bas-Konuş tuşuna otomatik basılı tutulur ve ses bitince bırakılır.",
        ),
        pttAssignRow,
        h("div", {}, h("span", { class: "onboard-location" }, "Değişim konumu: Ayarlar > Bas-Konuş")),
      ),
      h(
        "div",
        { class: "onboard-card" },
        h(
          "label",
          { class: "check" },
          muteChk,
          h("strong", {}, "2) Ses efekti çalarken fiziksel mikrofon geçici olarak susturulsun mu?"),
        ),
        h(
          "p",
          { class: "muted small" },
          "Aktif edilirse, ses efekti çaldığı sürece fiziksel mikrofonunuz geçici olarak susturulur (ortam gürültüsü karışmaz) ve efekt bittiği anda otomatik olarak geri açılır.",
        ),
        h("div", {}, h("span", { class: "onboard-location" }, "Değişim konumu: Ayarlar > Ses ve Mikrofon")),
      ),
    ],
    buttons: [
      {
        id: "save",
        label: "Kurulumu Tamamla ve Başla",
        kind: "primary",
        onClick: async (close) => {
          if (pttEnabled && !pttKey) {
            toast("Bas-Konuş aktif edildi; lütfen bir Bas-Konuş tuşu atayın veya kutunun işaretini kaldırın.", "info");
            return;
          }
          lib.settings.pttEnabled = pttEnabled;
          lib.settings.pttHotkey = pttKey;
          lib.settings.muteMicOnPlay = muteMic;
          lib.settings.onboardingDone = true;
          renderSettingsState();
          try {
            await api.completeOnboarding(pttEnabled, pttKey, muteMic);
          } catch (e) {
            toast(errMsg(e), "error");
          }
          close("save");
        },
      },
    ],
    onClose: () => {
      captureCallback = null;
      void api.cancelHotkeyCapture();
    },
  });
}

// ---------------------------------------------------------------- Sıralama ve Klasörler

async function handleReorder(ids: string[]) {
  const map = soundMap();
  const favs = ids.filter((id) => map.get(id)?.favorite);
  const rest = ids.filter((id) => !map.get(id)?.favorite);
  const ordered = [...favs, ...rest];

  if (activeFolder === ALL_VIEW) {
    lib.sounds = ordered.map((id) => map.get(id)!).filter(Boolean);
  } else {
    const f = lib.folders.find((x) => x.id === activeFolder);
    if (f) f.sounds = ordered;
  }
  renderGrid();
  try {
    await api.reorderSounds(activeFolder === ALL_VIEW ? null : activeFolder, ordered);
  } catch (e) {
    toast(`Sıralama kaydedilemedi: ${errMsg(e)}`, "error");
  }
}

async function handleDropToFolder(folderId: string, draggedId: string) {
  const ids = selected.has(draggedId) ? [...selected] : [draggedId];
  await addIdsToFolder(folderId, ids);
  renderGrid();
}

async function addIdsToFolder(folderId: string, ids: string[]) {
  const f = lib.folders.find((x) => x.id === folderId);
  if (!f || ids.length === 0) return;
  try {
    await api.addToFolder(folderId, ids);
    for (const id of ids) {
      if (!f.sounds.includes(id)) f.sounds.push(id);
    }
    selected.clear();
    renderAll();
    toast(`${ids.length} ses "${f.name}" klasörüne eklendi`, "ok", 2200);
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

async function unlinkFromCurrentFolder(ids: string[]) {
  if (activeFolder === ALL_VIEW || ids.length === 0) return;
  const f = lib.folders.find((x) => x.id === activeFolder);
  if (!f) return;
  try {
    await api.removeFromFolder(f.id, ids);
    const set = new Set(ids);
    f.sounds = f.sounds.filter((id) => !set.has(id));
    ids.forEach((id) => selected.delete(id));
    renderAll();
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

async function deleteSelectedSounds() {
  const ids = [...selected];
  if (ids.length === 0) return;
  const ok = await confirmModal(
    "Sesleri Sil",
    `Seçili ${ids.length} ses kütüphaneden ve diskten kalıcı olarak silinsin mi?`,
    "Kalıcı Olarak Sil",
    true,
  );
  if (!ok) return;
  try {
    lib = await api.deleteSounds(ids);
    selected.clear();
    renderAll();
  } catch (e) {
    toast(errMsg(e), "error");
    lib = await api.getLibrary();
    renderAll();
  }
}

async function createFolderPrompt(): Promise<string | null> {
  const name = await promptModal("Yeni Klasör", "", "Klasör adı");
  if (!name) return null;
  try {
    const f = await api.createFolder(name);
    lib.folders.push(f);
    renderSidebar();
    return f.id;
  } catch (e) {
    toast(errMsg(e), "error");
    return null;
  }
}

async function renameFolderPrompt(id: string, current: string) {
  const name = await promptModal("Klasörü Yeniden Adlandır", current);
  if (!name || name === current) return;
  try {
    await api.renameFolder(id, name);
    const f = lib.folders.find((x) => x.id === id);
    if (f) f.name = name;
    renderAll();
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

async function removeFolder(id: string, name: string) {
  const ok = await confirmModal(
    "Klasörü Sil",
    `"${name}" klasörü silinsin mi? İçindeki sesler "Tüm Sesler" listesinde kalmaya devam eder.`,
    "Klasörü Sil",
    true,
  );
  if (!ok) return;
  try {
    await api.deleteFolder(id);
    lib.folders = lib.folders.filter((f) => f.id !== id);
    if (activeFolder === id) activeFolder = ALL_VIEW;
    renderAll();
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

async function renameSoundPrompt(s: Sound) {
  const name = await promptModal("Sesi Yeniden Adlandır", s.name);
  if (!name || name === s.name) return;
  try {
    await api.renameSound(s.id, name);
    s.name = name;
    renderGrid();
  } catch (e) {
    toast(errMsg(e), "error");
  }
}

// ---------------------------------------------------------------- Dosya sürükle-bırak ve içe aktarma

async function bindFileDrop() {
  const overlay = $("#drop-overlay");
  await getCurrentWebview().onDragDropEvent((ev) => {
    const t = ev.payload.type;
    if (t === "enter" || t === "over") {
      overlay.classList.remove("hidden");
    } else if (t === "leave") {
      overlay.classList.add("hidden");
    } else if (t === "drop") {
      overlay.classList.add("hidden");
      void handleFileDrop(ev.payload.paths);
    }
  });
}

async function handleFileDrop(rawPaths: string[]) {
  if (importing) {
    toast("Devam eden bir içe aktarma işlemi var, lütfen bekleyin.", "info");
    return;
  }
  const supported = rawPaths.filter((p) => {
    const ext = p.split(".").pop()?.toLowerCase() ?? "";
    return SUPPORTED_EXTS.includes(ext);
  });
  const unsupported = rawPaths.length - supported.length;
  if (supported.length === 0) {
    if (unsupported > 0) {
      toast("Yalnızca .opus, .mp3, .wav, .ogg ve .flac dosyaları desteklenir.", "error");
    }
    return;
  }

  let normalize: boolean;
  let remember = false;
  if (lib.settings.normalizeChoice !== null) {
    normalize = lib.settings.normalizeChoice;
  } else {
    const decision = await askNormalize(supported);
    if (decision === null) return;
    normalize = decision.normalize;
    remember = decision.remember;
  }

  importing = true;
  try {
    const res = await api.importFiles(supported, normalize, remember);
    if (remember) lib.settings.normalizeChoice = normalize;
    lib.sounds.push(...res.added);

    if (activeFolder !== ALL_VIEW && res.added.length > 0) {
      const ids = res.added.map((s) => s.id);
      await api.addToFolder(activeFolder, ids);
      const f = lib.folders.find((x) => x.id === activeFolder);
      if (f) f.sounds.push(...ids);
    }

    renderAll();
    if (res.errors.length > 0) {
      toast(`Bazı dosyalar eklenemedi: ${res.errors.join(" · ")}`, "error", 5000);
    } else if (res.added.length > 0) {
      toast(`${res.added.length} ses eklendi`, "ok", 2200);
    }
    if (unsupported > 0) {
      toast(`${unsupported} desteklenmeyen dosya atlandı`, "info");
    }
  } catch (e) {
    toast(`İçe aktarma hatası: ${errMsg(e)}`, "error");
  } finally {
    importing = false;
  }
}

async function askNormalize(paths: string[]): Promise<{ normalize: boolean; remember: boolean } | null> {
  const names = paths.map((p) => p.split(/[\\/]/).pop() ?? p);
  const chk = h("input", { type: "checkbox" });
  const list = h(
    "ul",
    { class: "file-list" },
    ...names.slice(0, 20).map((n) => h("li", {}, n)),
    names.length > 20 && h("li", {}, `…ve ${names.length - 20} dosya daha`),
  );
  const sub =
    paths.length === 1
      ? `"${names[0]}" eklenecek.`
      : `${paths.length} ses dosyası tek seferde eklenecek:`;

  const r = await showModal({
    title: "Ses seviyesi otomatik dengelensin mi?",
    body: [
      h("p", {}, sub),
      paths.length > 1 && list,
      h(
        "p",
        { class: "muted small" },
        "Evet seçilirse sesler -1 dB tepe seviyesine normalize edilir; Hayır seçilirse ham haliyle bırakılır.",
      ),
      h("label", { class: "check" }, chk, h("span", {}, "Seçimimi hatırla")),
    ],
    buttons: [
      { id: "cancel", label: "İptal" },
      { id: "no", label: "Hayır (Ham Bırak)" },
      { id: "yes", label: "Evet (-1 dB)", kind: "primary" },
    ],
  });
  if (r !== "yes" && r !== "no") return null;
  return { normalize: r === "yes", remember: chk.checked };
}

// ---------------------------------------------------------------- Üst çubuk, Ayarlar & Cihazlar

function syncTopbar() {
  const vol = Math.round(lib.settings.volume * 100);
  const slider = $("#volume") as HTMLInputElement;
  slider.value = String(vol);
  slider.style.setProperty("--fill", `${vol}%`);
  $("#volume-val").textContent = `${vol}%`;
  ($("#local-preview") as HTMLInputElement).checked = lib.settings.localPreview;
}

function bindControls() {
  const search = $("#search") as HTMLInputElement;
  search.addEventListener("input", () => {
    query = search.value;
    renderGrid();
  });

  const slider = $("#volume") as HTMLInputElement;
  slider.addEventListener("input", () => {
    const v = Number(slider.value);
    slider.style.setProperty("--fill", `${v}%`);
    $("#volume-val").textContent = `${v}%`;
    void api.setVolume(v / 100, false);
  });
  slider.addEventListener("change", () => {
    const v = Number(slider.value) / 100;
    lib.settings.volume = v;
    void api.setVolume(v, true);
  });

  const preview = $("#local-preview") as HTMLInputElement;
  preview.addEventListener("change", () => {
    lib.settings.localPreview = preview.checked;
    void api.setLocalPreview(preview.checked);
  });

  $("#stop-all").addEventListener("click", () => void api.stopAll());
  $("#new-folder").addEventListener("click", () => void createFolderPrompt());

  // Seçim çubuğu
  $("#sel-clear").addEventListener("click", () => {
    selected.clear();
    renderAll();
  });
  $("#sel-remove").addEventListener("click", () => void unlinkFromCurrentFolder([...selected]));
  $("#sel-delete").addEventListener("click", () => void deleteSelectedSounds());
  $("#sel-add").addEventListener("click", (e) => {
    e.stopPropagation();
    toggleAddToFolderMenu();
  });

  // Ayarlar açılır paneli
  const panel = $("#settings-panel");
  $("#settings-btn").addEventListener("click", (e) => {
    e.stopPropagation();
    panel.classList.toggle("hidden");
  });
  panel.addEventListener("click", (e) => e.stopPropagation());
  document.addEventListener("click", () => {
    panel.classList.add("hidden");
    $("#sel-add-menu").classList.add("hidden");
  });

  // Ayarlar > Ses ve Mikrofon: Efekt çalarken mikrofonu geçici sustur
  const muteMicChk = $("#mute-mic-on-play") as HTMLInputElement;
  muteMicChk.addEventListener("change", async () => {
    lib.settings.muteMicOnPlay = muteMicChk.checked;
    await api.setMuteMicOnPlay(muteMicChk.checked);
  });

  // Ayarlar > Bas-Konuş (Push-to-Talk)
  const pttChk = $("#ptt-enabled") as HTMLInputElement;
  pttChk.addEventListener("change", async () => {
    lib.settings.pttEnabled = pttChk.checked;
    await api.setPttConfig(lib.settings.pttEnabled, lib.settings.pttHotkey);
  });
  $("#ptt-key-btn").addEventListener("click", async () => {
    const cap = await captureSingleKeyModal(
      "Bas-Konuş (Push-to-Talk) Tuşu Ata",
      lib.settings.pttHotkey?.label ?? "",
    );
    if (!cap) return;
    lib.settings.pttHotkey = { code: cap.code, label: cap.label, passthrough: true };
    renderSettingsState();
    await api.setPttConfig(lib.settings.pttEnabled, lib.settings.pttHotkey);
  });
  $("#ptt-key-clear").addEventListener("click", async () => {
    lib.settings.pttHotkey = null;
    lib.settings.pttEnabled = false;
    renderSettingsState();
    await api.setPttConfig(false, null);
  });

  // Ayarlar > Acil Durdurma (Panic Tuşu)
  $("#panic-key-btn").addEventListener("click", async () => {
    const cap = await captureSingleKeyModal(
      "Acil Durdurma (Panic) Tuşu Ata",
      lib.settings.panicHotkey.label,
    );
    if (!cap) return;
    lib.settings.panicHotkey = {
      code: cap.code,
      label: cap.label,
      passthrough: lib.settings.panicHotkey.passthrough,
    };
    renderSettingsState();
    await api.setPanicHotkey(lib.settings.panicHotkey);
  });
  $("#panic-key-reset").addEventListener("click", async () => {
    lib.settings.panicHotkey = {
      code: "VK:35",
      label: "End",
      passthrough: lib.settings.panicHotkey.passthrough,
    };
    renderSettingsState();
    await api.setPanicHotkey(lib.settings.panicHotkey);
  });
  const panicPassChk = $("#panic-passthrough") as HTMLInputElement;
  panicPassChk.addEventListener("change", async () => {
    lib.settings.panicHotkey.passthrough = panicPassChk.checked;
    await api.setPanicHotkey(lib.settings.panicHotkey);
  });

  // Ayarlar > İçe Aktarma & Klasör
  $("#normalize-reset").addEventListener("click", async () => {
    lib.settings.normalizeChoice = null;
    await api.setNormalizeChoice(null);
    renderSettingsState();
  });
  $("#open-folder").addEventListener("click", () => void api.openSoundsFolder());
  $("#refresh-devices").addEventListener("click", () => {
    void refreshDevices();
    void refreshApoStatus();
  });

  // APO Bütünlük Denetimi ve Tek Tıkla Otomatik Onarım (Self-Healing)
  $("#apo-repair-btn").addEventListener("click", () => void runApoRepair());
  $("#apo-repair-top").addEventListener("click", () => void runApoRepair());
  $("#apo-check-btn").addEventListener("click", async () => {
    await refreshApoStatus();
    toast("APO SHA-256 ve FxProperties zinciri denetlendi.", "info", 2200);
  });

  // Ayarlar > Profil Yedekle / Geri Yükle (.zip)
  $("#backup-export").addEventListener("click", async () => {
    try {
      const path = await api.exportProfileBackup();
      if (path) {
        toast(`Profil başarıyla yedeklendi: ${path}`, "ok", 4000);
      }
    } catch (e) {
      toast(`Yedekleme hatası: ${errMsg(e)}`, "error");
    }
  });
  $("#backup-import").addEventListener("click", async () => {
    try {
      const restored = await api.importProfileBackup();
      if (restored) {
        lib = restored;
        selected.clear();
        syncTopbar();
        renderAll();
        await refreshDevices();
        toast("Profil yedeği başarıyla geri yüklendi!", "ok", 3500);
      }
    } catch (e) {
      toast(`Geri yükleme hatası: ${errMsg(e)}`, "error");
    }
  });

  // Cihaz seçiciler
  const micSel = $("#mic-select") as HTMLSelectElement;
  micSel.addEventListener("change", async () => {
    const v = micSel.value || null;
    lib.settings.micDevice = v;
    await api.setDevice("mic", v);
    await pollStatus();
  });
  const monSel = $("#monitor-select") as HTMLSelectElement;
  monSel.addEventListener("change", async () => {
    const v = monSel.value || null;
    lib.settings.monitorDevice = v;
    await api.setDevice("monitor", v);
  });
  const brSel = $("#broadcast-select") as HTMLSelectElement;
  brSel.addEventListener("change", async () => {
    const v = brSel.value || null;
    lib.settings.broadcastDevice = v;
    await api.setDevice("broadcast", v);
    await pollStatus();
  });

  // Klavye kısayolları (Uygulama içi)
  window.addEventListener("keydown", (e) => {
    if (isModalOpen()) return;
    const inInput = e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement;
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "f") {
      e.preventDefault();
      search.focus();
      search.select();
    } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a" && !inInput) {
      e.preventDefault();
      visibleSounds().forEach((s) => selected.add(s.id));
      renderAll();
    } else if (e.key === "Delete" && !inInput && selected.size > 0) {
      e.preventDefault();
      void deleteSelectedSounds();
    } else if (e.key === "F2" && !inInput && selected.size === 1) {
      const id = [...selected][0];
      const s = lib.sounds.find((x) => x.id === id);
      if (s) void renameSoundPrompt(s);
    } else if (e.key === "Escape" && selected.size > 0) {
      selected.clear();
      renderAll();
    }
  });
}

function toggleAddToFolderMenu() {
  const menu = $("#sel-add-menu");
  if (!menu.classList.contains("hidden")) {
    menu.classList.add("hidden");
    return;
  }
  menu.innerHTML = "";
  const ids = [...selected];
  for (const f of lib.folders) {
    menu.append(
      h(
        "div",
        {
          class: "dropdown-item",
          onclick: () => {
            menu.classList.add("hidden");
            void addIdsToFolder(f.id, ids);
          },
        },
        f.name,
      ),
    );
  }
  menu.append(
    h(
      "div",
      {
        class: "dropdown-item new",
        onclick: async () => {
          menu.classList.add("hidden");
          const fid = await createFolderPrompt();
          if (fid) await addIdsToFolder(fid, ids);
        },
      },
      "＋ Yeni klasör oluştur ve ekle…",
    ),
  );
  menu.classList.remove("hidden");
}

async function refreshDevices() {
  let d: DeviceList;
  try {
    d = await api.listDevices();
  } catch {
    return;
  }
  fillSelect(
    $("#mic-select") as HTMLSelectElement,
    [
      { value: "", label: "Mikrofon: Kapalı" },
      { value: DEFAULT_DEVICE, label: `Varsayılan (${d.defaultInput ?? "yok"})` },
      ...d.inputs.map((n) => ({ value: n, label: n })),
    ],
    lib.settings.micDevice ?? "",
  );
  fillSelect(
    $("#monitor-select") as HTMLSelectElement,
    [
      { value: "", label: `Varsayılan (${d.defaultOutput ?? "yok"})` },
      ...d.outputs.map((n) => ({ value: n, label: n })),
    ],
    lib.settings.monitorDevice ?? "",
  );
  fillSelect(
    $("#broadcast-select") as HTMLSelectElement,
    [
      { value: "", label: "Yok (yalnızca kulaklık)" },
      ...d.outputs.map((n) => ({ value: n, label: n })),
    ],
    lib.settings.broadcastDevice ?? "",
  );
}

function fillSelect(sel: HTMLSelectElement, items: { value: string; label: string }[], current: string) {
  sel.innerHTML = "";
  const values = new Set(items.map((i) => i.value));
  if (current && !values.has(current)) {
    items.push({ value: current, label: `${current} (bağlı değil)` });
  }
  for (const it of items) {
    sel.append(h("option", { value: it.value }, it.label));
  }
  sel.value = current;
}

function applyApoReport(rep: ApoHealthReport) {
  const isOk = rep.mode === "apoActive";
  const topBadge = $("#apo-mode-badge");
  topBadge.className = `apo-pill ${isOk ? "apo-pill-ok" : "apo-pill-warn"}`;
  topBadge.textContent = isOk ? "APO Aktif" : "Fallback";
  topBadge.title = rep.statusMessage;

  const topRepairBtn = $("#apo-repair-top");
  topRepairBtn.classList.toggle("hidden", !rep.needsRepair);

  const modeEl = $("#apo-status-mode");
  modeEl.className = `apo-pill ${isOk ? "apo-pill-ok" : "apo-pill-warn"}`;
  modeEl.textContent = rep.modeLabel;

  const shaShort = rep.dllSha256 ? `${rep.dllSha256.slice(0, 12)}…` : "Bulunamadı";
  $("#apo-status-sha").textContent = rep.sha256Valid
    ? `Doğrulandı (${shaShort})`
    : `Hatalı / Eksik (${shaShort})`;

  const chainCount = rep.originalChainedClsids.length;
  $("#apo-status-chain").textContent = rep.fxChainIntact
    ? `Aktif (${chainCount} üretici APO zincirlendi)`
    : `Sıfırlanmış / Yetki Bekliyor (${chainCount} yedekli)`;

  $("#apo-status-format").textContent = `${rep.apoSampleRate} Hz · ${rep.apoChannels} Kanal (${rep.shmNamespace})`;
  $("#apo-status-detail").textContent = rep.statusMessage;
}

async function refreshApoStatus() {
  try {
    const rep = await api.getApoStatus();
    applyApoReport(rep);
  } catch {
    // Sessizce geç
  }
}

async function runApoRepair() {
  try {
    const rep = await api.repairApo();
    applyApoReport(rep);
    if (rep.mode === "apoActive") {
      toast("ozan-apo.dll SHA-256 ve FxProperties APO zinciri başarıyla onarıldı!", "ok", 3200);
    } else {
      toast(
        `Otomatik onarım tamamlandı; sistem politikası/yetki nedeniyle "${rep.modeLabel}" modunda çalışılıyor.`,
        "info",
        4000,
      );
    }
  } catch (e) {
    toast(`APO onarım hatası: ${errMsg(e)}`, "error");
  }
}

async function pollStatus() {
  if (document.hidden) return;
  let st: AudioStatus;
  try {
    st = await api.audioStatus();
  } catch {
    return;
  }
  const light = $("#mic-light");
  light.className = `light light-${st.mic}`;
  light.style.setProperty("--lvl", String(Math.min(1, st.micLevel * 3)));
  const titles: Record<AudioStatus["mic"], string> = {
    off: "Mikrofon kapalı",
    active: "Mikrofon aktif (yayın çıkışına aktarılıyor)",
    muted: "Ses efekti çaldığı için fiziksel mikrofon geçici olarak susturuldu",
    waiting: "Mikrofon seçili · Aktarım için Ayarlar (⚙) menüsünden bir Yayın Çıkışı seçin",
    error: "Mikrofon veya yayın cihazı açılamadı",
  };
  light.title = titles[st.mic];

  // Aktif çalan ses sayacı (maks. 5)
  const badge = $("#active-voices-badge");
  if (st.playing > 0) {
    badge.classList.remove("hidden");
    badge.textContent = `${st.playing} / 5 aktif ses`;
  } else {
    badge.classList.add("hidden");
  }

  // Kartlar üzerindeki gerçek zamanlı ilerleme çubuğu ve süre güncellemesi
  const byId = new Map(st.voices.map((v) => [v.id, v]));
  document.querySelectorAll<HTMLElement>(".card").forEach((card) => {
    const id = card.dataset.id!;
    const v = byId.get(id);
    const progEl = card.querySelector<HTMLElement>('[data-role="prog"]');
    const durEl = card.querySelector<HTMLElement>('[data-role="dur"]');
    const totalDur = Number(durEl?.dataset.total ?? 0);
    if (v) {
      card.classList.add("playing");
      if (progEl) progEl.style.width = `${Math.round(v.progress * 100)}%`;
      if (durEl) durEl.textContent = `${formatDuration(v.position)} / ${formatDuration(v.duration || totalDur)}`;
    } else {
      card.classList.remove("playing");
      if (progEl) progEl.style.width = "0%";
      if (durEl) durEl.textContent = formatDuration(totalDur);
    }
  });
}

/** Dinamik süre etiketi: 1 saatin altında dakika:saniye (MM:SS), 1 saat ve üzerinde saat:dakika:saniye (HH:MM:SS). */
function formatDuration(sec: number): string {
  const s = Math.max(0, Math.round(sec));
  const hrs = Math.floor(s / 3600);
  const mins = Math.floor((s % 3600) / 60);
  const rem = s % 60;
  if (hrs > 0) {
    return `${hrs}:${mins.toString().padStart(2, "0")}:${rem.toString().padStart(2, "0")}`;
  }
  return `${mins}:${rem.toString().padStart(2, "0")}`;
}

void init();
