const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const esc = (t) => String(t).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

// Small settings live in the backend's config file (loaded before anything else runs, hence the top-level
// await). Big blobs (library cache, playlist covers) stay in localStorage. localStorage can be missing or
// throw, and the backend can be absent in a test page: both fall back quietly.
let cfg = {}, cfgOk = false;
try { cfg = (await invoke("get_settings")).ui || {}; cfgOk = true; } catch (_) {}
const bigKey = (k) => k.startsWith("plcover:");
const legacy = (k, d) => { try { return localStorage.getItem(k) ?? d; } catch (_) { return d; } };
const store = {
  // a key never saved to the config file yet falls back to the old localStorage value
  get(k, d) { return cfgOk && !bigKey(k) && k in cfg ? cfg[k] : legacy(k, d); },
  set(k, v) {
    if (cfgOk && !bigKey(k)) { cfg[k] = v; invoke("set_ui_value", { key: k, value: v }).catch(() => {}); return; }
    try { localStorage.setItem(k, v); } catch (_) {}
  },
};

// =====================================================================
// State
// =====================================================================
let currentTrackList = [];
let currentTrackIndex = -1;
let currentDeviceId = "default";
let deviceChosen = false; // false until the user (or a saved choice) picks an output
let isPlaying = false;
let isPaused = false;
let currentDuration = 0;
let currentPosition = 0;

// =====================================================================
// Views
// =====================================================================
const VIEWS = ["now-playing", "library", "hardware", "themes", "terminal", "playlists"]; // 1-5 shortcuts map to the first five

function showView(name) {
  if (!VIEWS.includes(name)) name = "now-playing";
  document.querySelectorAll(".nav-item").forEach((i) => i.classList.toggle("active", i.dataset.view === name));
  if (name !== "playlists") { openPl = null; paintRail(); }
  document.querySelectorAll(".view-section").forEach((s) => s.classList.toggle("active", s.id === `${name}-view`));
  store.set("view", name);
  if (name === "terminal") { const o = $("log-output"); o.scrollTop = o.scrollHeight; }
}

function initNavigation() {
  document.querySelectorAll(".nav-item").forEach((i) => i.addEventListener("click", () => showView(i.dataset.view)));
  const saved = store.get("view", "now-playing");
  showView(saved === "playlists" ? "now-playing" : saved); // a playlist page cannot be restored without its name
}

// =====================================================================
// Output devices
// =====================================================================
const isMic = (d) => /mic|capture/i.test(d.name);

async function loadAudioDevices() {
  const list = $("hw-devices-grid");
  const sel = $("quick-device-select");
  try {
    const devices = await invoke("list_audio_devices");
    if (!devices || devices.length === 0) {
      list.innerHTML = '<p class="empty">No audio devices found.</p>';
      return;
    }

    if (!deviceChosen) {
      const saved = store.get("device", "");
      const pick = devices.find((d) => d.id === saved)
        || devices.find((d) => d.is_usb_dac && !isMic(d))
        || devices.find((d) => d.id === "default")
        || devices.find((d) => d.is_hardware && !isMic(d))
        || devices.find((d) => !isMic(d))
        || devices[0];
      currentDeviceId = pick.id;
      deviceChosen = true;
    }

    sel.innerHTML = devices.map((d) => `<option value="${esc(d.id)}">${esc(d.name)}</option>`).join("");
    sel.value = currentDeviceId;

    list.innerHTML = devices.map((d) => {
      const rates = d.supported_rates.length ? `${d.supported_rates.map((r) => r / 1000).join(" ")} kHz` : "rates managed by system";
      const depths = [16, 24, 32].filter((b) => d[`supports_${b}bit`]);
      return `
        <div class="dev ${d.id === currentDeviceId ? "selected" : ""}" data-device-id="${esc(d.id)}" tabindex="0">
          <span class="radio"></span>
          <div>
            <div><span class="dev-name">${esc(d.name)}</span><span class="dev-id">${esc(d.id)}</span></div>
            <div class="dev-spec">${rates}${depths.length ? ` · ${depths.join(" ")} bit` : ""}</div>
          </div>
          <span class="dev-kind">${d.id === currentDeviceId ? "active" : d.is_hardware ? "direct" : "shared"}</span>
        </div>`;
    }).join("");

    loadMixerControls().then(refreshVolume);
  } catch (err) {
    list.innerHTML = `<p class="empty">Could not list devices: ${esc(err)}</p>`;
  }
}

async function loadMixerControls() {
  const sel = $("mixer-select");
  let names = [];
  try { names = (await invoke("list_volume_controls", { deviceId: currentDeviceId })) || []; } catch (_) {}
  const saved = store.get(`mixer:${currentDeviceId}`, "");
  sel.innerHTML = '<option value="">Auto</option>' + names.map((n) => `<option>${esc(n)}</option>`).join("");
  sel.value = names.includes(saved) ? saved : "";
  sel.disabled = !names.length;
  await invoke("set_volume_control", { name: sel.value || null }).catch(() => {});
}

// Output buffer / period (ms): applied the next time the sound card is opened.
async function initBufferSettings() {
  const buf = $("buf-select"), per = $("per-select");
  try {
    const c = await invoke("get_settings");
    buf.value = String(c.buffer_ms);
    per.value = String(c.period_ms);
  } catch (_) {}
  const rb = $("reserve-btn");
  let viaDbus = false;
  const paintReserve = () => { rb.textContent = `Card release: ${viaDbus ? "D-Bus (experimental)" : "pactl"}`; rb.classList.toggle("ghost", !viaDbus); };
  try { viaDbus = !!(await invoke("get_settings")).reserve_dbus; } catch (_) {}
  paintReserve();
  rb.addEventListener("click", () => { viaDbus = !viaDbus; paintReserve(); invoke("set_reserve_dbus", { enabled: viaDbus }).catch(() => {}); });
  const send = () => invoke("set_audio_buffer", { bufferMs: Number(buf.value), periodMs: Number(per.value) })
    .then((c) => { per.value = String(c.period_ms); }).catch(() => {});
  buf.addEventListener("change", send);
  per.addEventListener("change", send);
}

function initOutputTools() {
  initBufferSettings();
  $("mixer-select").addEventListener("change", async (e) => {
    store.set(`mixer:${currentDeviceId}`, e.target.value);
    await invoke("set_volume_control", { name: e.target.value || null });
    refreshVolume(); // the slider now shows the picked control's level
  });
  $("hwdump-btn").addEventListener("click", async () => {
    const pre = $("hwdump");
    if (!pre.hidden) { pre.hidden = true; return; }
    pre.textContent = "Reading /proc/asound…";
    pre.hidden = false;
    try { pre.textContent = await invoke("get_kernel_hw_params"); } catch (err) { pre.textContent = String(err); }
  });
}

function chooseDevice(id) {
  currentDeviceId = id;
  deviceChosen = true;
  store.set("device", id);
  loadAudioDevices();
}

function initDevices() {
  $("quick-device-select").addEventListener("change", (e) => chooseDevice(e.target.value));
  $("rescan-hw-btn").addEventListener("click", loadAudioDevices);
  const list = $("hw-devices-grid");
  const pick = (e) => { const r = e.target.closest(".dev"); if (r) chooseDevice(r.dataset.deviceId); };
  list.addEventListener("click", pick);
  list.addEventListener("keydown", (e) => { if (e.key === "Enter") pick(e); });
}

// =====================================================================
// Themes
// =====================================================================
const THEME_VARS = ["--bg-app", "--bg-card", "--text-primary", "--accent"];

function syncPickers() {
  const cs = getComputedStyle(document.documentElement);
  const get = (v) => cs.getPropertyValue(v).trim();
  $("picker-bg-app").value = get("--bg-app");
  $("picker-bg-card").value = get("--bg-card");
  $("picker-accent").value = get("--accent");
  $("picker-text").value = get("--text-primary");
}

function applyTheme(name) {
  const root = document.documentElement;
  THEME_VARS.forEach((v) => root.style.removeProperty(v)); // drop customizer overrides
  root.setAttribute("data-theme", name);
  store.set("rex-music-theme", name);
  document.querySelectorAll(".theme-card").forEach((c) => c.classList.toggle("active", c.dataset.themeName === name));
  syncPickers();
}

function initThemeEngine() {
  document.querySelectorAll(".theme-card").forEach((c) => c.addEventListener("click", () => applyTheme(c.dataset.themeName)));
  applyTheme(store.get("rex-music-theme", "oled"));

  const bind = (id, v) => $(id).addEventListener("input", (e) => document.documentElement.style.setProperty(v, e.target.value));
  bind("picker-bg-app", "--bg-app");
  bind("picker-bg-card", "--bg-card");
  bind("picker-accent", "--accent");
  bind("picker-text", "--text-primary");

  $("export-theme-btn").addEventListener("click", () => {
    const theme = {
      name: "Custom theme",
      version: "1.0.0",
      colors: {
        "--bg-app": $("picker-bg-app").value,
        "--bg-card": $("picker-bg-card").value,
        "--accent": $("picker-accent").value,
        "--text-primary": $("picker-text").value,
      },
    };
    const url = URL.createObjectURL(new Blob([JSON.stringify(theme, null, 2)], { type: "application/json" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = "rex-theme.json";
    a.click();
    URL.revokeObjectURL(url);
  });
}

// =====================================================================
// Library
// =====================================================================
const lib = { q: "", filter: "all", view: "tracks", sortKey: null, sortDir: 1, collapsed: new Set() };
const isHiRes = (t) => t.bit_depth > 16 || t.sample_rate > 48000;
const isCD = (t) => t.bit_depth === 16 && t.sample_rate === 44100;
const sortValue = {
  title: (t) => t.title.toLowerCase(),
  artist: (t) => t.artist.toLowerCase(),
  album: (t) => t.album.toLowerCase(),
  format: (t) => t.format,
  quality: (t) => t.bit_depth * 1e6 + t.sample_rate,
  duration: (t) => t.duration_seconds,
};

function formatDuration(sec) {
  if (!sec || isNaN(sec)) return "0:00";
  const m = Math.floor(sec / 60);
  const s = Math.floor(sec % 60);
  return `${m}:${s < 10 ? "0" : ""}${s}`;
}

// ---- album grid: one cover tile per album, covers load lazily as tiles scroll into view ----
const albumArt = new Map();
let gridGroups = new Map();
const tileObserver = new IntersectionObserver((entries) => {
  for (const e of entries) {
    if (!e.isIntersecting) continue;
    tileObserver.unobserve(e.target);
    const el = e.target, path = el.dataset.path;
    const apply = (u) => { if (u) { el.style.backgroundImage = `url("${u}")`; el.classList.add("has"); } };
    if (albumArt.has(path)) apply(albumArt.get(path));
    else invoke("get_cover", { filePath: path }).catch(() => null).then((u) => { albumArt.set(path, u); apply(u); });
  }
}, { rootMargin: "200px" });

function renderAlbumGrid(rows) {
  gridGroups = new Map();
  rows.forEach((r) => {
    const key = `${r.t.album}\u0000${r.t.artist}`;
    if (!gridGroups.has(key)) gridGroups.set(key, []);
    gridGroups.get(key).push(r);
  });
  const box = $("lib-grid");
  box.innerHTML = [...gridGroups].map(([key, g]) => `
    <button class="tile" data-key="${esc(key)}" title="Play album">
      <span class="tile-art" data-path="${esc(g[0].t.file_path)}" data-letter="${esc((g[0].t.album || "?").trim().charAt(0).toUpperCase())}"></span>
      <span class="tile-title">${esc(g[0].t.album)}</span>
      <span class="tile-sub">${esc(g[0].t.artist)} · ${g.length} ${g.length === 1 ? "track" : "tracks"}</span>
    </button>`).join("");
  box.querySelectorAll(".tile-art").forEach((el) => tileObserver.observe(el));
}

// Queue exactly these library indices and start the first one (shuffled when shuffle is on).
function playIndices(idxs) {
  if (!idxs.length) return;
  orderFor = currentTrackList;
  queueSrc = [...idxs];
  order = [...idxs];
  if (shuffleOn) {
    for (let i = order.length - 1; i > 0; i--) {
      const j = Math.floor(Math.random() * (i + 1));
      [order[i], order[j]] = [order[j], order[i]];
    }
  }
  playTrackByIndex(order[0]);
}

// Big libraries: put the first rows in the DOM and add more as you scroll near the end.
const ROW_CHUNK = 300;
let pendingRows = [];
const moreObserver = new IntersectionObserver((entries) => { if (entries.some((e) => e.isIntersecting)) appendRows(); }, { rootMargin: "600px" });

function paintRows(parts) {
  pendingRows = parts.slice(ROW_CHUNK);
  $("library-tbody").innerHTML = parts.slice(0, ROW_CHUNK).join("") +
    (pendingRows.length ? '<tr id="lib-more"><td colspan="7" class="empty">…</td></tr>' : "");
  moreObserver.disconnect();
  const more = $("lib-more");
  if (more) moreObserver.observe(more);
}

function appendRows() {
  const more = $("lib-more");
  if (!more) return;
  more.insertAdjacentHTML("beforebegin", pendingRows.splice(0, ROW_CHUNK).join(""));
  moreObserver.unobserve(more);
  if (pendingRows.length) moreObserver.observe(more); // re-check: still in view means load the next chunk
  else more.remove();
}

function renderLibrary() {
  const tbody = $("library-tbody");
  const grid = lib.view === "grid" && currentTrackList.length > 0;
  $("lib-grid").hidden = !grid;
  document.querySelector("table.tracks").hidden = grid;
  const empty = (msg) => {
    if (grid) $("lib-grid").innerHTML = `<p class="pl-empty">${msg}</p>`;
    else tbody.innerHTML = `<tr><td colspan="7" class="empty">${msg}</td></tr>`;
  };
  if (!currentTrackList.length) return empty("No tracks yet. Enter a folder and press Scan.");

  // rows keep their index into currentTrackList, so sorting/filtering never changes what plays
  let rows = currentTrackList.map((t, i) => ({ t, i }));
  const q = lib.q.trim().toLowerCase();
  if (q) rows = rows.filter(({ t }) => `${t.title} ${t.artist} ${t.album}`.toLowerCase().includes(q));
  if (lib.filter === "hires") rows = rows.filter(({ t }) => isHiRes(t));
  if (lib.filter === "cd") rows = rows.filter(({ t }) => isCD(t));
  if (lib.sortKey) {
    const f = sortValue[lib.sortKey];
    rows.sort((a, b) => (f(a.t) > f(b.t) ? 1 : f(a.t) < f(b.t) ? -1 : 0) * lib.sortDir);
  }
  if (!rows.length) return empty("Nothing matches.");

  const row = ({ t, i }, n) => `
    <tr class="track-row ${i === currentTrackIndex ? "playing" : ""}" data-index="${i}" tabindex="0" draggable="true">
      <td class="n">${n}</td>
      <td>${esc(t.title)}</td>
      <td class="dim">${esc(t.artist)}</td>
      <td class="dim">${esc(t.album)}</td>
      <td class="f dim">${esc(t.format)}</td>
      <td class="r">${t.bit_depth}/${+(t.sample_rate / 1000).toFixed(1)}</td>
      <td class="r">${formatDuration(t.duration_seconds)}</td>
    </tr>`;

  if (grid) return renderAlbumGrid(rows);

  if (lib.view === "folders") {
    const dirOf = (p) => p.slice(0, p.lastIndexOf("/"));
    const byDir = new Map();
    rows.forEach((r) => {
      const d = dirOf(r.t.file_path);
      byDir.set(d, [...(byDir.get(d) || []), r]);
    });
    const parts = [...byDir.keys()].map((d) => d.split("/"));
    // folders shared by every track are not worth showing: start below the common root
    let root = parts[0].length;
    for (const p of parts) { let k = 0; while (k < root && p[k] === parts[0][k]) k++; root = k; }
    root = Math.min(root, Math.min(...parts.map((p) => p.length)) - 1);
    // every folder on the way to a track gets a header, so whole branches can collapse
    const nodes = new Set();
    parts.forEach((p) => { for (let n = root + 1; n <= p.length; n++) nodes.add(p.slice(0, n).join("/")); });
    const out = [];
    [...nodes].sort((a, b) => a.localeCompare(b)).forEach((node) => {
      const segs = node.split("/");
      const hidden = segs.slice(root + 1, -1).some((_, i) => lib.collapsed.has(`dir:${segs.slice(0, root + 2 + i).join("/")}`));
      if (hidden) return;
      const key = `dir:${node}`;
      const own = byDir.get(node) || [];
      const total = [...byDir].filter(([d]) => d === node || d.startsWith(node + "/")).reduce((n, [, g]) => n + g.length, 0);
      const closed = lib.collapsed.has(key);
      out.push(`<tr class="group" data-group="${esc(key)}"><td colspan="7" style="padding-left:${14 + (segs.length - root - 1) * 18}px">${closed ? "+" : "\u2212"} ${esc(segs[segs.length - 1])}<span class="g-meta">${total} ${total === 1 ? "track" : "tracks"}</span></td></tr>`);
      if (!closed) own.forEach((r, n) => out.push(row(r, n + 1)));
    });
    paintRows(out);
  } else if (lib.view === "albums") {
    const groups = new Map();
    rows.forEach((r) => {
      const key = `${r.t.album}\u0000${r.t.artist}`;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(r);
    });
    paintRows([...groups].map(([key, g]) => {
      const mins = Math.round(g.reduce((s, r) => s + r.t.duration_seconds, 0) / 60);
      const head = `
        <tr class="group" data-group="${esc(key)}">
          <td colspan="7">${lib.collapsed.has(key) ? "+" : "−"} ${esc(g[0].t.album)}
            <span class="g-meta">${esc(g[0].t.artist)} · ${g.length} ${g.length === 1 ? "track" : "tracks"} · ${mins} min</span></td>
        </tr>`;
      return head + (lib.collapsed.has(key) ? "" : g.map((r, n) => row(r, n + 1)).join(""));
    }));
  } else {
    paintRows(rows.map((r, n) => row(r, n + 1)));
  }
}

// The backend keeps the last scan of each folder on disk, so the library shows instantly on launch
// while a fresh scan (which only re-reads changed files) runs behind it.

async function scan(dir, background = false) {
  if (!background) $("library-tbody").innerHTML = '<tr><td colspan="7" class="empty">Scanning…</td></tr>';
  const playingPath = currentTrackList[currentTrackIndex]?.file_path;
  try {
    const fresh = (await invoke("scan_music_directory", { dirPath: dir })) || [];
    currentTrackList = fresh;
    queueSrc = null; // indices changed
  } catch (_) {
    if (!background) currentTrackList = [];
  }
  // keep the queue pointing at whatever is playing
  currentTrackIndex = playingPath ? currentTrackList.findIndex((t) => t.file_path === playingPath) : -1;
  buildOrder(currentTrackIndex >= 0 ? currentTrackIndex : undefined);
  renderRecent();
  if (currentTrackIndex >= 0) { renderUpNext(); syncNextHint(); }
  renderLibrary();
}

async function initLibrary() {
  const scanInput = $("scan-path-input");
  const savedDir = store.get("scanDir", "");

  try { scanInput.value = savedDir || (await invoke("get_default_music_dir")); } catch (_) {}

  const scanNow = () => {
    const dir = scanInput.value.trim();
    if (!dir) return;
    store.set("scanDir", dir);
    scan(dir);
  };
  $("scan-folder-btn").addEventListener("click", scanNow);
  scanInput.addEventListener("keydown", (e) => { if (e.key === "Enter") scanNow(); });

  $("library-search").addEventListener("input", (e) => { lib.q = e.target.value; renderLibrary(); });

  const segment = (id, attr, onPick) => {
    const box = $(id);
    box.addEventListener("click", (e) => {
      const b = e.target.closest("button");
      if (!b) return;
      box.querySelectorAll("button").forEach((x) => x.classList.toggle("on", x === b));
      onPick(b.dataset[attr]);
    });
  };
  segment("lib-filter", "filter", (v) => { lib.filter = v; renderLibrary(); });
  segment("lib-view", "view", (v) => { lib.view = v; renderLibrary(); });

  document.querySelector(".tracks thead").addEventListener("click", (e) => {
    const th = e.target.closest("th[data-sort]");
    if (!th) return;
    lib.sortDir = lib.sortKey === th.dataset.sort ? -lib.sortDir : 1;
    lib.sortKey = th.dataset.sort;
    document.querySelectorAll(".tracks th[data-sort]").forEach((h) =>
      h.setAttribute("aria-sort", h === th ? (lib.sortDir === 1 ? "ascending" : "descending") : "none"));
    renderLibrary();
  });

  const tbody = $("library-tbody");
  const activate = (e) => {
    const g = e.target.closest("tr.group");
    if (g) {
      const k = g.dataset.group;
      lib.collapsed.has(k) ? lib.collapsed.delete(k) : lib.collapsed.add(k);
      return renderLibrary();
    }
    const r = e.target.closest("tr.track-row");
    if (!r) return;
    const idx = Number(r.dataset.index);
    // queue what the table shows right now, in its current order
    queueSrc = [...tbody.querySelectorAll("tr.track-row")].map((x) => Number(x.dataset.index));
    buildOrder(idx);
    playTrackByIndex(idx);
  };
  $("lib-grid").addEventListener("click", (e) => {
    const t = e.target.closest(".tile");
    if (t && gridGroups.has(t.dataset.key)) playIndices(gridGroups.get(t.dataset.key).map((r) => r.i));
  });
  tbody.addEventListener("click", activate);
  tbody.addEventListener("keydown", (e) => { if (e.key === "Enter") activate(e); });

  // open with the last folder, or the bundled demo files on first run
  if (savedDir) {
    const cached = await invoke("get_cached_library", { dirPath: savedDir }).catch(() => []);
    if (cached?.length) { currentTrackList = cached; buildOrder(); renderLibrary(); renderRecent(); }
    scan(savedDir, !!cached?.length);
  } else {
    try {
      const tracks = await invoke("scan_music_directory", { dirPath: "demo" });
      if (tracks?.length) { currentTrackList = tracks; renderLibrary(); }
    } catch (_) {}
  }
}

// =====================================================================
// Queue: shuffle / repeat
// =====================================================================
let shuffleOn = store.get("shuffle", "0") === "1";
let repeatMode = store.get("repeat", "off"); // off | all | one
let order = [], orderFor = null;

// The queue is drawn from the list you started playing from (what the Library shows, an album, a playlist),
// so sorting/filtering/shuffle all stay inside it. null = the whole library.
let queueSrc = null;

function buildOrder(first) {
  orderFor = currentTrackList;
  order = queueSrc ? [...queueSrc] : currentTrackList.map((_, i) => i);
  if (!shuffleOn) return;
  for (let i = order.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [order[i], order[j]] = [order[j], order[i]];
  }
  if (first !== undefined) order = [first, ...order.filter((i) => i !== first)];
}
const orderPos = () => order.indexOf(currentTrackIndex);

// index of the track after the current one, or -1 at the end of the queue
function nextIndex(auto, from = currentTrackIndex) {
  if (auto && repeatMode === "one") return from;
  const pos = order.indexOf(from);
  if (pos >= 0 && pos < order.length - 1) return order[pos + 1];
  if (repeatMode === "all" && order.length) {
    if (shuffleOn) buildOrder();
    return order[0];
  }
  return -1;
}

// Name the track that follows the current one, so the engine can hand over gaplessly.
// Call whenever the current track, shuffle, repeat or the library changes.
// The engine decodes a little ahead of what you hear, so the "next track" hint is about the track the
// decoder is on (`decodingPath`), not the one on the speakers.
let decodingPath = null;
function syncNextHint(fromPath = decodingPath || currentTrackList[currentTrackIndex]?.file_path) {
  const from = currentTrackList.findIndex((t) => t.file_path === fromPath);
  const n = from >= 0 && !sleepAfterTrack ? nextIndex(true, from) : -1;
  const filePath = n >= 0 ? currentTrackList[n]?.file_path ?? null : null;
  invoke("queue_next", { filePath }).catch(() => {});
}

const hasNext = () => { const p = orderPos(); return (p >= 0 && p < order.length - 1) || (repeatMode === "all" && order.length > 0); };
const hasPrev = () => orderPos() > 0;

// ---- queue editing: the queue is `order`, the list of library indices after the current one ----
function queuePlace(idx, where) {
  if (currentTrackIndex < 0) return playTrackByIndex(idx); // nothing playing yet: just start it
  if (idx === currentTrackIndex) return;
  if (orderPos() < 0) buildOrder(currentTrackIndex);
  order = order.filter((i) => i !== idx);
  if (where === "next") order.splice(orderPos() + 1, 0, idx); else order.push(idx);
  renderUpNext(); syncNextHint();
}

function queueRemove(idx) {
  if (idx === currentTrackIndex) return;
  order = order.filter((i) => i !== idx);
  renderUpNext(); syncNextHint();
}

function queueReorder(from, to) { // positions within the upcoming list
  const base = orderPos() + 1;
  const [moved] = order.splice(base + from, 1);
  order.splice(base + to, 0, moved);
  renderUpNext(); syncNextHint();
}

function renderUpNext() {
  const box = $("up-next-list");
  const sum = $("up-next-summary");
  const pos = orderPos();
  const upcoming = pos >= 0 ? order.slice(pos + 1) : [];
  const cur = currentTrackList[currentTrackIndex];
  if (!cur) return;

  const mins = Math.round(upcoming.reduce((s, i) => s + currentTrackList[i].duration_seconds, 0) / 60);
  sum.textContent = upcoming.length ? `${upcoming.length} more · ${mins} min` : "";
  $("mini-next").textContent = upcoming.length ? `Next: ${currentTrackList[upcoming[0]].title}` : "";

  const row = (t, idx, label, cls = "") => `
    <div class="up-next-row ${cls}" data-index="${idx}" ${cls ? "" : `data-pos="${label - 1}" draggable="true"`}>
      <span class="un-idx"><span class="un-num">${label}</span><span class="un-play"></span></span>
      <span class="un-main"><span class="un-title">${esc(t.title)}</span><span class="un-artist">${esc(t.artist)}</span></span>
      <span class="un-dur">${formatDuration(t.duration_seconds)}</span>
      ${cls ? "" : '<button class="un-x" title="Remove from queue" aria-label="Remove from queue">×</button>'}
    </div>`;

  box.innerHTML = row(cur, currentTrackIndex, "now", "now") +
    upcoming.slice(0, 8).map((idx, i) => row(currentTrackList[idx], idx, i + 1)).join("") +
    (upcoming.length ? "" : '<p class="up-next-empty">End of queue.</p>');
  box.querySelectorAll(".up-next-row:not(.now)").forEach((r) => {
    r.addEventListener("click", (e) => {
      if (e.target.closest(".un-x")) return queueRemove(Number(r.dataset.index));
      playTrackByIndex(Number(r.dataset.index));
    });
    r.addEventListener("dragstart", (e) => { e.dataTransfer.setData("text/plain", r.dataset.pos); e.dataTransfer.effectAllowed = "move"; });
    r.addEventListener("dragover", (e) => { e.preventDefault(); r.classList.add("drop"); });
    r.addEventListener("dragleave", () => r.classList.remove("drop"));
    r.addEventListener("drop", (e) => {
      e.preventDefault();
      const from = Number(e.dataTransfer.getData("text/plain"));
      const to = Number(r.dataset.pos);
      if (Number.isInteger(from) && from !== to) queueReorder(from, to);
    });
  });
}

// =====================================================================
// Cover art: disc label, or square cover (click the artwork to switch)
// =====================================================================
async function loadCover(filePath) {
  let url = null;
  try { url = await invoke("get_cover", { filePath }); } catch (_) {}
  const img = $("cover-img");
  const center = document.querySelector(".vinyl-center");
  const wrap = $("art-wrapper");
  img.hidden = !url;
  if (url) img.src = url;
  center.style.backgroundImage = url ? `url("${url}")` : "";
  center.classList.toggle("has-cover", !!url);
  wrap.classList.toggle("has-cover", !!url);
}

let smoothVuL = 0;
let smoothVuR = 0;
let targetVuL = 0;
let targetVuR = 0;
let currentLyrics = [];
let currentLyricIdx = -1;

let currentVisMode = "art";
let visCaps = new Array(32).fill(0);
let visSmooth = new Array(32).fill(0);

function initVisualTabs() {
  const tabs = document.querySelectorAll("#vtabs button");
  const art = $("art-wrapper");
  const vu = $("vu-panel");
  const spectrum = $("spectrum-panel");
  const scope = $("scope-panel");
  const lyrics = $("lyrics-panel");

  function showVis(mode) {
    currentVisMode = mode;
    tabs.forEach((b) => b.classList.toggle("on", b.dataset.vis === mode));
    art.hidden = mode !== "art";
    vu.hidden = mode !== "vu";
    spectrum.hidden = mode !== "spectrum";
    scope.hidden = mode !== "scope";
    lyrics.hidden = mode !== "lyrics";
    store.set("visualMode", mode);
  }

  tabs.forEach((b) => b.addEventListener("click", () => showVis(b.dataset.vis)));
  showVis(store.get("visualMode", "art"));
  startVisLoop();
}

function drawSpectrum(frame) {
  const canvas = $("spectrum-canvas");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const w = canvas.width, h = canvas.height;
  const cs = getComputedStyle(document.documentElement);
  const accent = cs.getPropertyValue("--accent").trim() || "#4fd1c5";
  const line = cs.getPropertyValue("--line").trim() || "rgba(255,255,255,0.08)";
  const faint = cs.getPropertyValue("--text-faint").trim() || "#888";

  ctx.clearRect(0, 0, w, h);
  // Subtle dB grid lines
  ctx.strokeStyle = line;
  ctx.lineWidth = 1;
  for (let i = 1; i < 4; i++) {
    const y = Math.round((h * i) / 4) + 0.5;
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
  }

  const bands = frame.bands || [];
  const n = bands.length || 32;
  const padX = 10, padY = 12;
  const plotW = w - padX * 2, plotH = h - padY * 2;
  const step = plotW / n;
  const barW = Math.max(2, step - 3);

  for (let i = 0; i < n; i++) {
    const target = bands[i] || 0;
    visSmooth[i] += (target - visSmooth[i]) * (target > visSmooth[i] ? 0.5 : 0.2);
    visCaps[i] = Math.max(visSmooth[i], visCaps[i] - 0.012);
    const bh = visSmooth[i] * plotH;
    const x = padX + i * step + 1;
    const y = h - padY - bh;

    ctx.fillStyle = accent;
    ctx.fillRect(x, y, barW, bh);

    const capY = h - padY - visCaps[i] * plotH;
    ctx.fillStyle = faint;
    ctx.fillRect(x, capY - 2, barW, 2);
  }

  const pL = frame.peak_l_db > -59.5 ? frame.peak_l_db.toFixed(1) : "-∞";
  const pR = frame.peak_r_db > -59.5 ? frame.peak_r_db.toFixed(1) : "-∞";
  $("spectrum-meta").textContent = `32-band FFT · Peak ${pL} / ${pR} dBFS`;
}

function drawScope(frame) {
  const canvas = $("scope-canvas");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const w = canvas.width, h = canvas.height;
  const cs = getComputedStyle(document.documentElement);
  const accent = cs.getPropertyValue("--accent").trim() || "#4fd1c5";
  const line = cs.getPropertyValue("--line-strong").trim() || "rgba(255,255,255,0.12)";

  // Phosphor trail decay
  ctx.fillStyle = "rgba(0, 0, 0, 0.25)";
  ctx.fillRect(0, 0, w, h);

  const cx = w / 2, cy = h / 2;
  const r = Math.min(w, h) * 0.42;

  ctx.strokeStyle = line;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(cx - r, cy); ctx.lineTo(cx + r, cy);
  ctx.moveTo(cx, cy - r); ctx.lineTo(cx, cy + r);
  ctx.moveTo(cx - r * 0.707, cy - r * 0.707); ctx.lineTo(cx + r * 0.707, cy + r * 0.707);
  ctx.moveTo(cx - r * 0.707, cy + r * 0.707); ctx.lineTo(cx + r * 0.707, cy - r * 0.707);
  ctx.stroke();

  const pts = frame.scope || [];
  if (pts.length) {
    ctx.strokeStyle = accent;
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    for (let i = 0; i < pts.length; i++) {
      const [l, rv] = pts[i];
      // Goniometer rotation: X = (L - R) / sqrt(2), Y = (L + R) / sqrt(2)
      const x = cx + ((l - rv) * 0.707) * r;
      const y = cy - ((l + rv) * 0.707) * r;
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }

  const corr = (frame.correlation ?? 1.0).toFixed(2);
  const pL = frame.peak_l_db > -59.5 ? frame.peak_l_db.toFixed(1) : "-∞";
  const pR = frame.peak_r_db > -59.5 ? frame.peak_r_db.toFixed(1) : "-∞";
  $("scope-meta").textContent = `Phase ${frame.correlation >= 0 ? "+" : ""}${corr} · Peak ${pL} / ${pR} dB`;
}

function startVisLoop() {
  const tick = async () => {
    if ((currentVisMode === "spectrum" || currentVisMode === "scope") && $("now-playing-view").classList.contains("active")) {
      try {
        const frame = await invoke("get_visualizer_frame");
        if (currentVisMode === "spectrum") drawSpectrum(frame);
        else drawScope(frame);
      } catch (_) {}
      setTimeout(tick, 33);
    } else {
      setTimeout(tick, 250);
    }
  };
  tick();
}

function parseLRC(text) {
  if (!text) return [];
  const lines = text.split(/\r?\n/);
  const out = [];
  const timeRe = /\[(\d{1,2}):(\d{2})(?:\.(\d{2,3}))?\]/g;
  for (const line of lines) {
    timeRe.lastIndex = 0;
    const matches = [...line.matchAll(timeRe)];
    if (!matches.length) continue;
    const lyric = line.replace(timeRe, "").trim();
    if (!lyric) continue;
    for (const m of matches) {
      const min = Number(m[1]);
      const sec = Number(m[2]);
      const ms = m[3] ? Number(m[3].padEnd(3, "0").slice(0, 3)) : 0;
      const time = min * 60 + sec + ms / 1000;
      out.push({ time, text: lyric });
    }
  }
  return out.sort((a, b) => a.time - b.time);
}

// Show lyric text: time-synced lines (clickable, followed by the clock) or plain paragraphs.
function renderLyrics(raw, box) {
  const parsed = parseLRC(raw);
  if (!parsed.length) {
    box.innerHTML = raw.split(/\r?\n/).filter(Boolean).map(l => `<div class="lyric-line">${esc(l)}</div>`).join("");
    return;
  }
  currentLyrics = parsed;
  box.innerHTML = parsed.map((l, i) => `<div class="lyric-line" data-idx="${i}" data-time="${l.time}">${esc(l.text)}</div>`).join("");
  box.querySelectorAll(".lyric-line").forEach((el) => {
    el.addEventListener("click", () => {
      const t = Number(el.dataset.time);
      if (!isNaN(t)) seekTo(t);
    });
  });
}

// Online lyrics (lrclib.net) are opt-in: the button, or the Auto-fetch switch (off by default).
let autoLyrics = store.get("autoLyrics", "0") === "1";
let lyricsToken = 0; // a lookup that finishes after the song changed must not paint over the new one

function lyricsArgs(filePath) {
  const t = currentTrackList.find((x) => x.file_path === filePath);
  return t && { title: t.title, artist: t.artist, album: t.album, durationSecs: Math.round(t.duration_seconds || 0) };
}

async function fetchOnlineLyrics(filePath, force) {
  const box = $("lyrics-content");
  const args = lyricsArgs(filePath);
  if (!args) return;
  const token = ++lyricsToken;
  box.innerHTML = '<p class="lyrics-empty">Asking lrclib.net…</p>';
  try {
    const text = await invoke("fetch_online_lyrics", { ...args, force });
    if (token !== lyricsToken) return;
    if (text) renderLyrics(text, box);
    else box.innerHTML = '<p class="lyrics-empty">lrclib.net has no lyrics for this song.</p>';
  } catch (err) {
    if (token === lyricsToken) box.innerHTML = `<p class="lyrics-empty">${esc(String(err))}</p>`;
  }
}

async function loadLyrics(filePath) {
  currentLyrics = [];
  currentLyricIdx = -1;
  const token = ++lyricsToken;
  const box = $("lyrics-content");
  box.innerHTML = '<p class="lyrics-empty">Searching lyrics...</p>';
  try {
    const raw = await invoke("get_lyrics", { filePath }); // .lrc files and embedded tags first
    if (token !== lyricsToken) return;
    if (raw) return renderLyrics(raw, box);

    const args = lyricsArgs(filePath);
    const cached = args && (await invoke("get_online_lyrics_cached", args).catch(() => null)); // cache only, no network
    if (token !== lyricsToken) return;
    if (cached) return renderLyrics(cached, box);
    if (autoLyrics && args) return fetchOnlineLyrics(filePath, false);

    box.innerHTML = '<p class="lyrics-empty">No lyrics found for this song (.lrc file or tags).' +
      '<span class="lyrics-note">Use Fetch online below to look it up on lrclib.net.</span></p>';
  } catch (_) {
    if (token === lyricsToken) box.innerHTML = '<p class="lyrics-empty">No lyrics available</p>';
  }
}

function initLyricsControls() {
  const auto = $("lyrics-auto-btn");
  const paint = () => { auto.textContent = `Auto-fetch: ${autoLyrics ? "on" : "off"}`; auto.classList.toggle("ghost", !autoLyrics); };
  paint();
  auto.addEventListener("click", () => {
    autoLyrics = !autoLyrics;
    store.set("autoLyrics", autoLyrics ? "1" : "0");
    paint();
  });
  $("lyrics-fetch-btn").addEventListener("click", () => {
    const path = currentTrackList[currentTrackIndex]?.file_path;
    if (path) fetchOnlineLyrics(path, true);
  });
}

function updateActiveLyric(pos) {
  if (!currentLyrics.length) return;
  let idx = -1;
  for (let i = 0; i < currentLyrics.length; i++) {
    if (currentLyrics[i].time <= pos) idx = i;
    else break;
  }
  if (idx !== currentLyricIdx) {
    currentLyricIdx = idx;
    const box = $("lyrics-content");
    const lines = box.querySelectorAll(".lyric-line");
    lines.forEach((l, i) => l.classList.toggle("active", i === idx));
    if (idx >= 0 && lines[idx]) {
      const activeEl = lines[idx];
      const panel = $("lyrics-panel");
      const offset = activeEl.offsetTop - panel.clientHeight / 2 + activeEl.clientHeight / 2;
      panel.scrollTo({ top: offset, behavior: "smooth" });
    }
  }
}

function initArtToggle() {
  const wrap = $("art-wrapper");
  wrap.classList.toggle("square", store.get("artSquare", "0") === "1");
  wrap.addEventListener("click", () => {
    if (!wrap.classList.contains("has-cover")) return; // nothing to show as a square
    const square = wrap.classList.toggle("square");
    store.set("artSquare", square ? "1" : "0");
  });
}

// =====================================================================
// Playback
// =====================================================================
let isStartingTrack = false;
async function playTrackByIndex(idx) {
  if (idx < 0 || idx >= currentTrackList.length) return;
  if (isStartingTrack && currentTrackIndex === idx) return;
  isStartingTrack = true;
  currentTrackIndex = idx;
  if (queueSrc && !queueSrc.includes(idx)) queueSrc = null;
  if (orderFor !== currentTrackList || !order.length || !order.includes(idx)) buildOrder(idx);
  stopRequested = false;
  const track = currentTrackList[idx];

  renderLibrary();
  renderUpNext();
  loadCover(track.file_path);
  loadLyrics(track.file_path);
  decodingPath = track.file_path;
  syncNextHint(track.file_path);

  try {
    await invoke("play_track", { filePath: track.file_path, deviceId: currentDeviceId });
    isPlaying = true;
    isPaused = false;
    updatePlayPauseButton();
  } catch (err) {
    console.error("Playback error:", err);
  } finally {
    setTimeout(() => { isStartingTrack = false; }, 300);
  }
}

async function togglePlay() {
  if (!isPlaying && currentTrackList.length > 0) {
    playTrackByIndex(currentTrackIndex >= 0 ? currentTrackIndex : 0);
  } else if (isPlaying && !isPaused) {
    await invoke("pause_track");
    isPaused = true;
    updatePlayPauseButton();
  } else if (isPlaying && isPaused) {
    await invoke("resume_track");
    isPaused = false;
    updatePlayPauseButton();
  }
}

let stopRequested = false;
async function stopPlayback() {
  stopRequested = true;
  await invoke("stop_track");
  isPlaying = false;
  isPaused = false;
  updatePlayPauseButton();
  resetNowPlayingUI();
}

function prevTrack() {
  const pos = orderPos();
  if (currentPosition > 3 || pos <= 0) {
    seekTo(0); // restart current track
  } else {
    playTrackByIndex(order[pos - 1]);
  }
}

function nextTrack() {
  const n = nextIndex(false);
  if (n >= 0) playTrackByIndex(n);
}

function paintModes() {
  $("btn-shuffle").classList.toggle("on", shuffleOn);
  const rep = $("btn-repeat");
  rep.classList.toggle("on", repeatMode !== "off");
  rep.dataset.mode = repeatMode;
  rep.title = `Repeat: ${repeatMode} (r)`;
}

function toggleShuffle() {
  shuffleOn = !shuffleOn;
  store.set("shuffle", shuffleOn ? "1" : "0");
  if (currentTrackIndex >= 0) { buildOrder(currentTrackIndex); renderUpNext(); syncNextHint(); }
  paintModes();
}

function cycleRepeat() {
  repeatMode = { off: "all", all: "one", one: "off" }[repeatMode];
  store.set("repeat", repeatMode);
  syncNextHint();
  paintModes();
}

// Seeking. The UI position is extrapolated between 250ms polls, so seeks update it immediately.
let lastPollPos = 0, lastPollAt = 0, lastTimeText = "", lastSeekAt = 0;
const livePos = () => lastPollPos + (isPaused ? 0 : (performance.now() - lastPollAt) / 1000);

function seekTo(seconds) {
  if (isPaused) return; // the engine ignores seeks while paused
  lastSeekAt = performance.now();
  lastPollPos = seconds;
  lastPollAt = lastSeekAt;
  return invoke("seek_track", { positionSeconds: seconds });
}

function seekBy(delta) {
  if (!currentDuration) return;
  seekTo(Math.max(0, Math.min(currentDuration - 0.5, livePos() + delta)));
}

function initPlaybackControls() {
  $("btn-play").addEventListener("click", togglePlay);
  $("btn-stop").addEventListener("click", stopPlayback);
  $("btn-prev").addEventListener("click", prevTrack);
  $("btn-next").addEventListener("click", nextTrack);
  $("btn-shuffle").addEventListener("click", toggleShuffle);
  $("btn-repeat").addEventListener("click", cycleRepeat);
  paintModes();

  const bar = $("progress-bar");
  bar.addEventListener("click", (e) => {
    if (!currentDuration) return;
    const rect = bar.getBoundingClientRect();
    seekTo(Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width)) * currentDuration);
  });
}

function updatePlayPauseButton() {
  const playing = isPlaying && !isPaused;
  $("btn-play").classList.toggle("is-playing", playing);
  $("vinyl-disc").classList.toggle("spinning", playing);
}

// =====================================================================
// Readout (Now playing) + transport status
// =====================================================================
const READOUT = ["source", "output", "rate", "format", "buffer", "resample", "mixer"];
const setReadout = (vals) => READOUT.forEach((k, i) => { $(`ro-${k}`).textContent = vals ? vals[i] : "—"; });

function setIdleStatus() {
  $("signal-dot").className = "mark";
  $("signal-hud-text").textContent = "Idle";
  $("tp-rate").textContent = "—";
  $("sigpath").hidden = true;
  setReadout(null);
}

function resetNowPlayingUI() {
  $("time-current").textContent = "0:00";
  $("progress-fill").style.width = "0%";
  setIdleStatus();
}

function showStatus(s) {
  const k = (n) => +(n / 1000).toFixed(1);
  const hz = s.active_hardware_rate;
  const fmt = (s.active_hardware_format || "").replace(/\s*\(.*\)/, "");
  const ms = (frames) => (hz ? Math.round((frames / hz) * 1000) : 0);
  const src = `${(s.file_path || "").split(".").pop().toUpperCase()} ${s.bit_depth}-bit / ${k(s.sample_rate)} kHz / ${s.channels === 1 ? "mono" : "stereo"}`;
  const resample = s.is_bit_perfect ? "none" : s.sample_rate !== hz ? `${k(s.sample_rate)} → ${k(hz)} kHz` : "possible (shared device)";

  setReadout([
    src,
    s.active_device_id,
    `${hz} Hz`,
    fmt,
    s.active_buffer_frames ? `${ms(s.active_buffer_frames)} ms / ${ms(s.active_period_frames)} ms` : "—",
    resample,
    $("volume-slider").disabled ? "system" : "DAC hardware",
  ]);
  $("tp-rate").textContent = `${k(hz)} kHz · ${fmt}`;

  // signal path: source -> processing -> output -> volume; accent when untouched, amber when anything alters it
  const ext = (s.file_path || "").split(".").pop().toUpperCase();
  const rgActive = rgOn && rgGain < 0;
  const notes = [];
  if (s.sample_rate !== hz) notes.push(`resampled ${k(s.sample_rate)}\u2192${k(hz)} kHz`);
  else if (!s.is_bit_perfect) notes.push("shared mixer");
  if (rgActive) notes.push(`ReplayGain ${rgGain.toFixed(1)} dB`);
  $("sp-src").textContent = `${ext} ${s.bit_depth}/${k(s.sample_rate)}`;
  const mid = $("sp-proc");
  mid.textContent = notes.length ? notes.join(" + ") : "bit-perfect, no DSP";
  mid.className = `sp-node sp-mid ${notes.length ? "warn" : "ok"}`;
  $("sp-out").textContent = `${s.active_device_id} ${fmt}`;
  $("sp-vol").textContent = $("volume-slider").disabled ? "system volume" : "DAC volume";
  $("sigpath").hidden = false;
  const resampled = s.sample_rate !== hz;
  $("signal-dot").className = `mark ${s.device_disconnected ? "warn" : s.is_bit_perfect ? "ok" : resampled ? "info" : "warn"}`;
  $("signal-hud-text").textContent = s.device_disconnected
    ? `DAC disconnected · waiting for ${s.active_device_id}…`
    : s.signal_mode && s.signal_mode !== "Idle"
      ? `${s.signal_mode} · ${k(hz)} kHz`
      : s.is_bit_perfect
        ? "Bit-perfect · direct hardware"
        : resampled
          ? `In-engine resampled · ${k(s.sample_rate)} → ${k(hz)} kHz`
          : `Shared device · ${s.active_device_id}`;
}

// =====================================================================
// Volume: DAC hardware mixer (disabled for shared devices, where it would not be bit-perfect)
// =====================================================================
let bpLock = false;

async function refreshVolume() {
  const slider = $("volume-slider");
  let v = null;
  try { v = await invoke("get_volume", { deviceId: currentDeviceId }); } catch (_) {}
  slider.disabled = v === null || bpLock;
  slider.title = bpLock ? "Locked at 0 dBFS (Bit-Perfect Lock active)" : v === null ? "No hardware volume for this device" : "";
  if (v !== null) slider.value = bpLock ? 100 : v;
  $("vol-label").textContent = bpLock ? "0dB" : v === null ? "--" : `${v}%`;
}

// ReplayGain: lowers the DAC's own volume for loud masters (never raises it), so the stream stays untouched.
let rgOn = store.get("rg", "0") === "1";
let rgGain = 0;

let volTimer;
function sendVolume() {
  if (bpLock) return;
  // throttle: one mixer call per ~60ms, not one per slider pixel
  clearTimeout(volTimer);
  volTimer = setTimeout(() => invoke("set_volume", {
    deviceId: currentDeviceId, percent: Number($("volume-slider").value), gainDb: rgOn ? rgGain : 0,
  }), 60);
}

function setVolume(pct) {
  if (bpLock) return;
  const slider = $("volume-slider");
  slider.value = pct;
  $("vol-label").textContent = `${slider.value}%`;
  sendVolume();
}

function paintRg() {
  const b = $("rg-btn");
  b.textContent = `ReplayGain: ${rgOn ? "on" : "off"}`;
  b.classList.toggle("ghost", !rgOn);
  b.disabled = bpLock;
}

function paintBpLock() {
  const b = $("bp-lock-btn");
  b.textContent = `0 dBFS Lock: ${bpLock ? "on" : "off"}`;
  b.classList.toggle("ghost", !bpLock);
}

async function initVolume() {
  const slider = $("volume-slider");
  try { bpLock = !!(await invoke("get_settings")).bit_perfect_lock; } catch (_) {}
  paintBpLock();
  paintRg();
  $("bp-lock-btn").addEventListener("click", async () => {
    bpLock = !bpLock;
    paintBpLock();
    paintRg();
    await invoke("set_bit_perfect_lock", { enabled: bpLock, deviceId: currentDeviceId }).catch(() => {});
    refreshVolume();
  });
  $("rg-btn").addEventListener("click", () => {
    rgOn = !rgOn;
    store.set("rg", rgOn ? "1" : "0");
    paintRg();
    sendVolume();
  });
  slider.addEventListener("input", () => setVolume(Number(slider.value)));
}

function nudgeVolume(delta) {
  const slider = $("volume-slider");
  if (!slider.disabled) setVolume(Math.max(0, Math.min(100, Number(slider.value) + delta)));
}

// =====================================================================
// Smooth progress + status polling
// =====================================================================
function animateProgress() {
  // Analog VU needle ballistics (fast attack, smooth mechanical decay)
  smoothVuL += (targetVuL - smoothVuL) * (targetVuL > smoothVuL ? 0.35 : 0.08);
  smoothVuR += (targetVuR - smoothVuR) * (targetVuR > smoothVuR ? 0.35 : 0.08);
  const degL = -42 + Math.min(1, Math.max(0, smoothVuL)) * 84;
  const degR = -42 + Math.min(1, Math.max(0, smoothVuR)) * 84;
  const nL = $("vu-needle-left");
  const nR = $("vu-needle-right");
  if (nL) nL.style.transform = `rotate(${degL.toFixed(1)}deg)`;
  if (nR) nR.style.transform = `rotate(${degR.toFixed(1)}deg)`;

  if (isPlaying && currentDuration > 0) {
    const pos = Math.min(currentDuration, livePos());
    $("progress-fill").style.width = `${(pos / currentDuration) * 100}%`;
    const t = formatDuration(pos);
    if (t !== lastTimeText) {
      lastTimeText = t;
      $("time-current").textContent = t;
    }
    updateActiveLyric(pos);
  }
  requestAnimationFrame(animateProgress);
}

// ---- sleep timer: off -> 15 -> 30 -> 60 min -> after this track ----
const SLEEP_STEPS = [0, 15, 30, 60, "track"];
let sleepIdx = 0, sleepAt = 0, sleepAfterTrack = false;

function paintSleep() {
  const v = SLEEP_STEPS[sleepIdx];
  $("sleep-btn").querySelector("kbd").textContent =
    v === 0 ? "off" : v === "track" ? "track" : `${Math.max(1, Math.ceil((sleepAt - Date.now()) / 60000))}m`;
}

function initSleep() {
  $("sleep-btn").addEventListener("click", () => {
    sleepIdx = (sleepIdx + 1) % SLEEP_STEPS.length;
    const v = SLEEP_STEPS[sleepIdx];
    sleepAfterTrack = v === "track";
    sleepAt = typeof v === "number" && v > 0 ? Date.now() + v * 60000 : 0;
    syncNextHint(); // after-track: tell the engine there is no next track so it ends cleanly
    paintSleep();
  });
  setInterval(() => {
    if (sleepAt && Date.now() >= sleepAt) {
      sleepAt = 0; sleepIdx = 0;
      stopPlayback();
      toast("Sleep timer: playback stopped");
    }
    paintSleep();
  }, 1000);
}

// ---- recently played (last 8, kept between launches) ----
let recent = [];
try { recent = JSON.parse(store.get("recent", "[]")); } catch (_) {}

function noteRecent(path) {
  if (!path) return;
  recent = [path, ...recent.filter((p) => p !== path)].slice(0, 8);
  store.set("recent", JSON.stringify(recent));
  renderRecent();
}

function renderRecent() {
  const byPath = new Map(currentTrackList.map((t, i) => [t.file_path, i]));
  const playing = currentTrackList[currentTrackIndex]?.file_path;
  const items = recent.filter((p) => p !== playing && byPath.has(p));
  $("recent-box").hidden = !items.length;
  $("recent-list").innerHTML = items.map((p, n) => {
    const t = currentTrackList[byPath.get(p)];
    return `<div class="up-next-row" data-index="${byPath.get(p)}" tabindex="0">
      <span class="un-idx"><span class="un-num">${n + 1}</span><span class="un-play"></span></span>
      <span class="un-main"><span class="un-title">${esc(t.title)}</span><span class="un-artist">${esc(t.artist)}</span></span>
      <span class="un-dur">${formatDuration(t.duration_seconds)}</span>
    </div>`;
  }).join("");
}

let endedHandled = false;
let wasPlaying = false;
let lastSerial = 0;
let lastDecoderSerial = 0;

// Tell the desktop (media widgets, lock screen) what is playing; only when something changed.
let lastMpris = "";
function pushMpris(status) {
  const playing = status.is_playing;
  const state = !playing ? "Stopped" : status.is_paused ? "Paused" : "Playing";
  const canNext = hasNext(), canPrev = hasPrev();
  const key = [state, status.file_path, status.title, status.artist, status.album, canNext, canPrev].join("|");
  if (key === lastMpris) return;
  lastMpris = key;
  invoke("mpris_update", {
    title: playing ? status.title || "" : "", artist: status.artist || "", album: status.album || "",
    lengthSecs: status.duration_seconds || 0, filePath: playing ? status.file_path : null,
    canNext, canPrev, status: state,
  }).catch(() => {});
}

async function pollStatus() {
    try {
      const status = await invoke("get_playback_status");
      if (!status) return;

      // Autoplay: the engine says the track ran to its natural end
      if (status.ended && !endedHandled) {
        endedHandled = true;
        if (sleepAfterTrack) { sleepAfterTrack = false; sleepIdx = 0; paintSleep(); }
        else { const n = nextIndex(true); if (n >= 0) playTrackByIndex(n); }
      } else if (!status.ended) endedHandled = false;

      pushMpris(status);

      // Playing -> stopped without a natural end or a Stop press: say so instead of going silent.
      if (wasPlaying && !status.is_playing && !status.ended && !stopRequested) {
        toast("Playback stopped unexpectedly. The Terminal tab has the details.", "Open", () => showView("terminal"));
      }
      wasPlaying = status.is_playing;

      // The engine moved to the next track by itself (gapless): follow it in the UI.
      if (status.track_serial !== lastSerial) {
        lastSerial = status.track_serial;
        rgGain = status.replay_gain_db || 0;
        if (rgOn) sendVolume();
        const cur = currentTrackList[currentTrackIndex]?.file_path;
        if (status.file_path && status.file_path !== cur) {
          const idx = currentTrackList.findIndex((t) => t.file_path === status.file_path);
          if (idx >= 0) {
            currentTrackIndex = idx;
            renderLibrary();
            loadCover(status.file_path);
            loadLyrics(status.file_path);
          }
        }
        noteRecent(status.file_path);
        renderUpNext();
      }

      // The decoder started a track (maybe ahead of what is audible): name the one after it.
      if (status.decoder_serial !== lastDecoderSerial) {
        lastDecoderSerial = status.decoder_serial;
        decodingPath = status.decoding_path;
        syncNextHint(decodingPath);
      }
      isPlaying = status.is_playing;
      isPaused = status.is_paused;
      currentDuration = status.duration_seconds;
      currentPosition = status.position_seconds;
      targetVuL = isPlaying && !isPaused ? (status.vu_left || 0) : 0;
      targetVuR = isPlaying && !isPaused ? (status.vu_right || 0) : 0;

      // right after a seek, ignore polled positions until the engine has applied it
      if (performance.now() - lastSeekAt > 400) {
        lastPollPos = currentPosition;
        lastPollAt = performance.now();
      }

      updatePlayPauseButton();

      if (!isPlaying) {
        targetVuL = 0;
        targetVuR = 0;
        return setIdleStatus();
      }

      $("time-duration").textContent = formatDuration(currentDuration);
      $("np-title").textContent = status.title;
      $("np-artist").textContent = status.artist;
      $("np-album").textContent = status.album;
      $("mini-title").textContent = status.title;
      $("mini-artist").textContent = status.artist;
      showStatus(status);
    } catch (_) {}
}

// The engine announces track starts, pauses and ends ("player-event"), so the UI reacts at once.
// The timer is only a safety net and the source of the VU peaks: fast while the VU meter is on screen.
function startStatusPolling() {
  const tick = async () => {
    await pollStatus();
    setTimeout(tick, $("vu-panel").hidden ? 500 : 100);
  };
  tick();
  if (window.__TAURI__?.event?.listen) window.__TAURI__.event.listen("player-event", pollStatus);
}

// =====================================================================
// Terminal
// =====================================================================
let allLogs = [], logSig = "";
const logLevels = new Set(["info", "alsa", "warn", "error"]);
const visibleLogs = () => allLogs.filter((l) => logLevels.has(l.level.toLowerCase()));

function renderLogs() {
  const out = $("log-output");
  const stick = out.scrollTop + out.clientHeight >= out.scrollHeight - 20;
  out.replaceChildren(...visibleLogs().map((l) => {
    const div = document.createElement("div");
    div.className = `log-line ${l.level.toLowerCase()}`;
    const t = document.createElement("span");
    t.className = "log-time";
    t.textContent = l.timestamp;
    div.append(t, document.createTextNode(`[${l.level}] ${l.message}`));
    return div;
  }));
  if (stick) out.scrollTop = out.scrollHeight;
}

function initLogPanel() {
  $("clear-logs-btn").addEventListener("click", async () => {
    await invoke("clear_engine_logs");
    allLogs = [];
    logSig = "";
    renderLogs();
  });

  $("copy-report-btn").addEventListener("click", async (e) => {
    const btn = e.currentTarget;
    try {
      await navigator.clipboard.writeText(await invoke("get_diagnostics"));
      btn.textContent = "Copied";
    } catch (_) {
      btn.textContent = "Copy failed";
    }
    setTimeout(() => { btn.textContent = "Copy report"; }, 1500);
  });

  $("copy-logs-btn").addEventListener("click", async (e) => {
    const text = visibleLogs().map((l) => `${l.timestamp} [${l.level}] ${l.message}`).join("\n");
    const btn = e.currentTarget;
    try {
      await navigator.clipboard.writeText(text);
      btn.textContent = "Copied";
    } catch (_) {
      btn.textContent = "Copy failed";
    }
    setTimeout(() => { btn.textContent = "Copy"; }, 1200);
  });

  $("log-filter").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    const on = b.classList.toggle("on");
    on ? logLevels.add(b.dataset.level) : logLevels.delete(b.dataset.level);
    renderLogs();
  });

  setInterval(async () => {
    try {
      const logs = await invoke("get_engine_logs");
      const last = logs[logs.length - 1];
      const sig = `${logs.length}|${last?.timestamp}|${last?.message}`;
      if (sig === logSig) return;
      logSig = sig;
      allLogs = logs;
      renderLogs();
    } catch (_) { /* backend not ready */ }
  }, 500);
}

// =====================================================================
// Keyboard
// =====================================================================
// j/k move focus through whatever list the current page shows (Enter plays), g g / G jump to the ends.
const listItems = () => [...document.querySelectorAll(
  ".view-section.active tr.track-row, .view-section.active #pl-tracks .up-next-row:not(.missing), .view-section.active .tile")];

function focusItem(items, i) {
  if (!items.length) return;
  items[i].focus();
  items[i].scrollIntoView({ block: "nearest" });
}

function stepList(d) {
  const items = listItems();
  const cur = items.indexOf(document.activeElement);
  focusItem(items, cur < 0 ? (d > 0 ? 0 : items.length - 1) : Math.max(0, Math.min(items.length - 1, cur + d)));
}

let lastG = 0;
function gChord() {
  const now = Date.now();
  if (now - lastG < 600) { lastG = 0; focusItem(listItems(), 0); } else lastG = now;
}

function initShortcuts() {
  const dlg = $("shortcuts");
  $("help-btn").addEventListener("click", () => dlg.showModal());

  const typing = (el) => el.matches?.("input:not([type=range]):not([type=color]), textarea, select");

  // Space activates a focused button on keyup; swallow it so it only toggles playback once
  addEventListener("keyup", (e) => { if (e.key === " " && !typing(e.target)) e.preventDefault(); });

  addEventListener("keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey || dlg.open) return;
    if (typing(e.target)) { if (e.key === "Escape") e.target.blur(); return; }
    if (e.target.type === "range") return; // the volume slider handles its own arrows

    const actions = {
      " ": togglePlay,
      ArrowLeft: () => seekBy(-5),
      ArrowRight: () => seekBy(5),
      ArrowUp: () => nudgeVolume(5),
      ArrowDown: () => nudgeVolume(-5),
      n: nextTrack,
      p: prevTrack,
      s: toggleShuffle,
      r: cycleRepeat,
      j: () => stepList(1),
      k: () => stepList(-1),
      g: gChord,
      G: () => { while ($("lib-more")) appendRows(); const it = listItems(); focusItem(it, it.length - 1); },
      "/": () => { showView("library"); $("library-search").focus(); },
      "?": () => dlg.showModal(),
    };
    const act = actions[e.key];
    if (act) { e.preventDefault(); act(); }
    else if (e.key >= "1" && e.key <= "5") { e.preventDefault(); showView(VIEWS[Number(e.key) - 1]); }
  });
}

function initMpris() {
  if (window.__TAURI__?.event?.listen) {
    window.__TAURI__.event.listen("mpris-command", (e) => {
      switch (e.payload) {
        case "play-pause": togglePlay(); break;
        case "next": nextTrack(); break;
        case "prev": prevTrack(); break;
        case "stop": stopPlayback(); break;
        case "play": if (!isPlaying || isPaused) togglePlay(); break;
        case "pause": if (isPlaying && !isPaused) togglePlay(); break;
      }
    });
  }
}

// =====================================================================
// Bootstrap
// =====================================================================
// =====================================================================
// Playlists (plain .m3u files) and the library right-click menu
// =====================================================================
// Smart playlists: rules evaluated live against the library, never stored.
const SMART = [
  { id: "hires", name: "Hi-res (96 kHz and up)", test: (t) => t.sample_rate >= 96000 },
  { id: "bit24", name: "24-bit and up", test: (t) => t.bit_depth >= 24 },
  { id: "cd", name: "CD quality (16/44.1)", test: (t) => t.bit_depth === 16 && t.sample_rate === 44100 },
];

function openSmart(id) {
  const sp = SMART.find((x) => x.id === id);
  if (!sp) return;
  openPl = { name: sp.name, smart: true, paths: currentTrackList.filter(sp.test).map((t) => t.file_path) };
  showView("playlists");
  $("pl-add").value = "";
  $("pl-results").innerHTML = "";
  paintRail();
  renderPlaylistDetail();
}

let playlistNames = [];
let openPl = null; // { name, paths } while a playlist page is open

async function refreshNames() {
  try { playlistNames = (await invoke("list_playlists")) || []; } catch (_) { playlistNames = []; }
  paintRail();
}

// The sidebar list: like any music app, playlists live here and a click opens one.
function paintRail() {
  const box = $("rail-playlists");
  if (!box) return;
  box.innerHTML = (playlistNames.length
    ? playlistNames.map((n) => `<button class="pl-item ${openPl && !openPl.smart && openPl.name === n ? "active" : ""}" data-name="${esc(n)}" title="${esc(n)}">${esc(n)}</button>`).join("")
    : '<p class="rail-empty">None yet. Press + to create one.</p>')
    + SMART.map((x) => `<button class="pl-item smart ${openPl?.smart && openPl.name === x.name ? "active" : ""}" data-smart="${x.id}" title="${esc(x.name)}">${esc(x.name)}</button>`).join("");
}

function toast(text, actionLabel, onAction) {
  document.querySelector(".toast")?.remove();
  const t = document.createElement("div");
  t.className = "toast";
  t.append(text);
  if (actionLabel) {
    const b = document.createElement("button");
    b.textContent = actionLabel;
    b.addEventListener("click", () => { t.remove(); onAction(); });
    t.append(b);
  }
  document.body.appendChild(t);
  setTimeout(() => t.remove(), 5000);
}

async function openPlaylist(name) {
  try {
    openPl = { name, paths: await invoke("load_playlist", { name }) };
  } catch (err) { return toast(String(err)); }
  showView("playlists");
  $("pl-add").value = "";
  $("pl-results").innerHTML = "";
  paintRail();
  renderPlaylistDetail();
}

function renderPlaylistDetail() {
  const byPath = new Map(currentTrackList.map((t, i) => [t.file_path, i]));
  const smart = !!openPl.smart; // smart lists are read-only: no rename, delete, add or remove
  $("pl-detail").classList.toggle("smart", smart);
  $("pl-del").hidden = smart;
  document.querySelector(".pl-add").hidden = smart;
  $("pl-title").textContent = openPl.name;
  $("pl-title").title = smart ? "Smart playlist: built from your library" : "Click to rename";
  const known = openPl.paths.map((p) => currentTrackList[byPath.get(p)]).filter(Boolean);
  const mins = Math.round(known.reduce((sum, t) => sum + t.duration_seconds, 0) / 60);
  const n = openPl.paths.length;
  $("pl-count").textContent = `${n} ${n === 1 ? "track" : "tracks"}` + (known.length ? ` · ${mins} min` : "") +
    (known.length < n ? ` · ${n - known.length} not in the scanned library` : "");
  $("pl-tracks").innerHTML = n ? openPl.paths.map((p, k) => {
    const t = currentTrackList[byPath.get(p)];
    return `<div class="up-next-row ${t ? "" : "missing"}" data-path="${esc(p)}" tabindex="0">
      <span class="un-idx"><span class="un-num">${k + 1}</span><span class="un-play"></span></span>
      <span class="un-main"><span class="un-title">${esc(t ? t.title : p.split("/").pop())}</span><span class="un-artist">${t ? esc(t.artist) : "not in the scanned library"}</span></span>
      <span class="un-dur">${t ? formatDuration(t.duration_seconds) : ""}</span>
      <button class="un-x" title="Remove from playlist" aria-label="Remove from playlist">×</button>
    </div>`;
  }).join("") : '<p class="pl-empty">Nothing here yet. Search above to add songs, or drag them in from the Library.</p>';
  paintPlaylistCover(openPl.paths);
}

// Cover: up to four distinct album covers as a 2x2 mosaic, one if fewer, a dashed square when there are none.
const plCoverCache = new Map();
let plCoverToken = 0;
const coverKey = (name) => `plcover:${name}`;

async function paintPlaylistCover(paths) {
  const token = ++plCoverToken;
  const box = $("pl-cover");
  const custom = openPl.smart ? "" : store.get(coverKey(openPl.name), "");
  box.classList.toggle("locked", !!openPl.smart);
  $("pl-cover-rm").hidden = !custom;
  box.title = openPl.smart ? "" : "Click, or drop an image, to set the cover";
  if (custom) { // a picked image beats the automatic mosaic
    box.className = "pl-cover one";
    box.innerHTML = `<img src="${custom}" alt="">`;
    return;
  }
  const urls = [];
  for (const p of paths.slice(0, 16)) {
    if (urls.length >= 4) break;
    if (!plCoverCache.has(p)) plCoverCache.set(p, await invoke("get_cover", { filePath: p }).catch(() => null));
    const u = plCoverCache.get(p);
    if (u && !urls.includes(u)) urls.push(u);
  }
  if (token !== plCoverToken) return; // the page changed while covers were loading
  box.className = "pl-cover" + (!urls.length ? " empty" : urls.length < 4 ? " one" : "") + (openPl.smart ? " locked" : "");
  box.innerHTML = urls.length
    ? (urls.length < 4 ? urls.slice(0, 1) : urls).map((u) => `<img src="${u}" alt="">`).join("")
    : openPl.smart ? "" : '<span class="pl-cover-hint">Add cover<small>click or drop an image</small></span>';
}

// Inline song search: type, click a result, it is added; the field keeps focus for the next one.
function renderPlaylistResults() {
  const q = $("pl-add").value.trim().toLowerCase();
  const box = $("pl-results");
  if (!q || !openPl) { box.innerHTML = ""; return; }
  const have = new Set(openPl.paths);
  const hits = currentTrackList.filter((t) => !have.has(t.file_path) && `${t.title} ${t.artist} ${t.album}`.toLowerCase().includes(q)).slice(0, 8);
  box.innerHTML = hits.length ? hits.map((t) => `<div class="up-next-row" data-path="${esc(t.file_path)}">
      <span class="un-idx"><span class="un-num">+</span></span>
      <span class="un-main"><span class="un-title">${esc(t.title)}</span><span class="un-artist">${esc(t.artist)} · ${esc(t.album)}</span></span>
      <span class="un-dur">${formatDuration(t.duration_seconds)}</span>
    </div>`).join("") : '<p class="pl-empty">No matching songs in the scanned library.</p>';
}

function playPlaylist(startPath) {
  const byPath = new Map(currentTrackList.map((t, i) => [t.file_path, i]));
  const idxs = openPl.paths.map((p) => byPath.get(p)).filter((i) => i !== undefined);
  if (!idxs.length) return toast("None of these tracks are in the scanned library");
  orderFor = currentTrackList;
  queueSrc = [...idxs];
  order = idxs;
  let first = startPath !== undefined ? byPath.get(startPath) : idxs[0];
  if (shuffleOn && startPath === undefined) {
    for (let i = order.length - 1; i > 0; i--) {
      const j = Math.floor(Math.random() * (i + 1));
      [order[i], order[j]] = [order[j], order[i]];
    }
    first = order[0];
  }
  playTrackByIndex(first);
}

async function addToPlaylist(name, path) {
  try {
    await invoke("add_to_playlist", { name, path });
    await refreshNames();
    if (openPl?.name === name) await openPlaylist(name);
    toast(`Added to “${name}”`, "Open", () => openPlaylist(name));
  } catch (err) { toast(String(err)); }
}

// "+" creates an empty playlist straight away and opens it with the title ready to rename.
async function createPlaylist() {
  let name = "New playlist", k = 2;
  while (playlistNames.includes(name)) name = `New playlist ${k++}`;
  try { await invoke("save_playlist", { name, paths: [] }); } catch (err) { return toast(String(err)); }
  await refreshNames();
  await openPlaylist(name);
  renameTitle();
}

// Crop an image file to a square, shrink it, and keep it as this playlist's cover.
function setCoverFromFile(file) {
  if (!file || !file.type.startsWith("image/") || !openPl || openPl.smart) return;
  const url = URL.createObjectURL(file);
  const img = new Image();
  img.onload = () => {
    const side = Math.min(img.width, img.height), size = Math.min(480, side);
    const cv = document.createElement("canvas");
    cv.width = cv.height = size;
    cv.getContext("2d").drawImage(img, (img.width - side) / 2, (img.height - side) / 2, side, side, 0, 0, size, size);
    URL.revokeObjectURL(url);
    const data = cv.toDataURL("image/jpeg", 0.85);
    store.set(coverKey(openPl.name), data);
    if (store.get(coverKey(openPl.name), "") !== data) toast("Could not save the cover (storage is full or blocked)");
    paintPlaylistCover(openPl.paths);
  };
  img.onerror = () => { URL.revokeObjectURL(url); toast("That file is not an image the app can read"); };
  img.src = url;
}

function renameTitle() {
  if (openPl.smart) return;
  const h = $("pl-title");
  const old = openPl.name;
  const input = document.createElement("input");
  input.type = "text";
  input.className = "field";
  input.value = old;
  h.hidden = true;
  h.after(input);
  input.focus();
  input.select();
  let done = false;
  const finish = async (commit) => {
    if (done) return;
    done = true;
    const name = input.value.trim();
    input.remove();
    h.hidden = false;
    if (!commit || !name || name === old) return;
    try {
      await invoke("rename_playlist", { old, new: name });
      const cover = store.get(coverKey(old), ""); // the cover follows the new name
      if (cover) { store.set(coverKey(name), cover); try { localStorage.removeItem(coverKey(old)); } catch (_) {} }
      await refreshNames();
      await openPlaylist(name);
    } catch (err) { toast(String(err)); }
  };
  input.addEventListener("keydown", (e) => {
    e.stopPropagation(); // typing must not trigger player shortcuts
    if (e.key === "Enter") finish(true);
    if (e.key === "Escape") finish(false);
  });
  input.addEventListener("blur", () => finish(true));
}

// Swap `el` for a small text field; Enter submits, Escape or clicking away cancels.
function askName(el, placeholder, onDone) {
  const input = document.createElement("input");
  input.type = "text";
  input.className = "field";
  input.placeholder = placeholder;
  el.hidden = true;
  el.after(input);
  input.focus();
  const end = () => { input.remove(); el.hidden = false; };
  input.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Escape") end();
    if (e.key === "Enter" && input.value.trim()) { const v = input.value.trim(); end(); onDone(v); }
  });
  input.addEventListener("blur", end);
}

// Ctrl+K: one box that finds playlists, albums and songs from any page.
function initPalette() {
  const dlg = $("palette"), input = $("palette-input"), box = $("palette-results");
  let items = [], sel = 0;
  const has = (q, ...parts) => parts.join(" ").toLowerCase().includes(q);

  const paint = () => {
    box.innerHTML = items.map((it, n) => `<div class="pal-row ${n === sel ? "sel" : ""}" data-n="${n}">
      <span class="pal-kind">${it.kind}</span><span class="pal-title">${esc(it.title)}</span><span class="pal-sub">${esc(it.sub)}</span></div>`).join("")
      || (input.value.trim() ? '<p class="pl-empty">Nothing found.</p>' : "");
    box.querySelector(".sel")?.scrollIntoView({ block: "nearest" });
  };

  const build = () => {
    const q = input.value.trim().toLowerCase();
    items = [];
    if (q) {
      playlistNames.filter((n) => has(q, n)).slice(0, 4)
        .forEach((n) => items.push({ kind: "Playlist", title: n, sub: "", go: () => openPlaylist(n) }));
      const albums = new Map();
      currentTrackList.forEach((t, i) => {
        if (!has(q, t.album, t.artist)) return;
        const k = `${t.album}\u0000${t.artist}`;
        albums.set(k, [...(albums.get(k) || []), i]);
      });
      [...albums.values()].slice(0, 4).forEach((idxs) => {
        const t = currentTrackList[idxs[0]];
        items.push({ kind: "Album", title: t.album, sub: t.artist, go: () => playIndices(idxs) });
      });
      let songs = 0;
      for (let i = 0; i < currentTrackList.length && songs < 8; i++) {
        const t = currentTrackList[i];
        if (has(q, t.title, t.artist, t.album)) { songs++; items.push({ kind: "Song", title: t.title, sub: t.artist, go: () => playTrackByIndex(i) }); }
      }
    }
    sel = 0;
    paint();
  };

  const run = (n) => { const it = items[n]; if (it) { dlg.close(); it.go(); } };

  addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      if (dlg.open) return dlg.close();
      input.value = "";
      build();
      dlg.showModal();
      input.focus();
    }
  });
  input.addEventListener("input", build);
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") { e.preventDefault(); sel = Math.min(items.length - 1, sel + 1); paint(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); sel = Math.max(0, sel - 1); paint(); }
    else if (e.key === "Enter") run(sel);
  });
  box.addEventListener("click", (e) => { const r = e.target.closest(".pal-row"); if (r) run(Number(r.dataset.n)); });
  dlg.addEventListener("click", (e) => { if (e.target === dlg) dlg.close(); }); // click on the backdrop
}

function initContextMenu() {
  const menu = document.createElement("div");
  menu.className = "ctx";
  menu.hidden = true;
  document.body.appendChild(menu);
  let target = -1;
  const close = () => { menu.hidden = true; };

  $("library-tbody").addEventListener("contextmenu", (e) => {
    const r = e.target.closest("tr.track-row");
    if (!r) return;
    e.preventDefault();
    target = Number(r.dataset.index);
    menu.innerHTML = [
      '<button data-act="next">Play next</button>',
      '<button data-act="end">Add to queue</button>',
      '<div class="ctx-sep"></div>',
      '<div class="ctx-label">Add to playlist</div>',
      ...playlistNames.map((n) => `<button data-pl="${esc(n)}">${esc(n)}</button>`),
      '<button data-act="newpl">New playlist…</button>',
    ].join("");
    menu.hidden = false;
    menu.style.left = `${Math.min(e.clientX, innerWidth - 200)}px`;
    menu.style.top = `${Math.max(8, Math.min(e.clientY, innerHeight - menu.offsetHeight - 8))}px`;
  });

  menu.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b || target < 0) return;
    const track = currentTrackList[target];
    if (b.dataset.pl) { addToPlaylist(b.dataset.pl, track.file_path); return close(); }
    if (b.dataset.act === "newpl") // keep the menu open and ask for a name right here
      return askName(b, "Playlist name", (name) => { addToPlaylist(name, track.file_path); close(); });
    queuePlace(target, b.dataset.act);
    close();
  });
  document.addEventListener("click", (e) => { if (!menu.contains(e.target)) close(); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") close(); });
  window.addEventListener("blur", close);
}

function initPlaylists() {
  $("pl-new").addEventListener("click", createPlaylist);
  $("recent-list").addEventListener("click", (e) => {
    const r = e.target.closest(".up-next-row");
    if (r) playTrackByIndex(Number(r.dataset.index));
  });
  $("rail-playlists").addEventListener("click", (e) => {
    const b = e.target.closest(".pl-item");
    if (!b) return;
    if (b.dataset.smart) openSmart(b.dataset.smart); else openPlaylist(b.dataset.name);
  });

  // drag a song from the library onto a playlist in the sidebar
  const rail = $("rail-playlists");
  rail.addEventListener("dragover", (e) => {
    const b = e.target.closest(".pl-item");
    if (!b) return;
    e.preventDefault();
    rail.querySelectorAll(".drop").forEach((x) => x.classList.remove("drop"));
    b.classList.add("drop");
  });
  rail.addEventListener("dragleave", (e) => e.target.closest(".pl-item")?.classList.remove("drop"));
  rail.addEventListener("drop", (e) => {
    const b = e.target.closest(".pl-item");
    if (!b) return;
    e.preventDefault();
    b.classList.remove("drop");
    const t = currentTrackList[Number(e.dataTransfer.getData("text/rex-track"))];
    if (t) addToPlaylist(b.dataset.name, t.file_path);
  });
  $("library-tbody").addEventListener("dragstart", (e) => {
    const r = e.target.closest("tr.track-row");
    if (!r) return;
    e.dataTransfer.setData("text/rex-track", r.dataset.index);
    e.dataTransfer.effectAllowed = "copy";
  });

  $("pl-title").addEventListener("click", renameTitle);

  const cover = $("pl-cover"), picker = $("pl-cover-file");
  const choose = () => { if (openPl && !openPl.smart) picker.click(); };
  cover.addEventListener("click", choose);
  cover.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.stopPropagation(); choose(); } });
  picker.addEventListener("change", () => { setCoverFromFile(picker.files[0]); picker.value = ""; });
  cover.addEventListener("dragover", (e) => { if (openPl && !openPl.smart) { e.preventDefault(); cover.classList.add("over"); } });
  cover.addEventListener("dragleave", () => cover.classList.remove("over"));
  cover.addEventListener("drop", (e) => { e.preventDefault(); cover.classList.remove("over"); setCoverFromFile(e.dataTransfer.files[0]); });
  $("pl-cover-rm").addEventListener("click", () => {
    try { localStorage.removeItem(coverKey(openPl.name)); } catch (_) {}
    paintPlaylistCover(openPl.paths);
  });
  $("pl-add").addEventListener("input", renderPlaylistResults);
  $("pl-results").addEventListener("click", async (e) => {
    const row = e.target.closest(".up-next-row");
    if (!row || !openPl) return;
    const path = row.dataset.path;
    try {
      await invoke("add_to_playlist", { name: openPl.name, path });
      openPl.paths.push(path);
      renderPlaylistDetail();
      renderPlaylistResults();
      refreshNames();
    } catch (err) { toast(String(err)); }
  });
  $("pl-play").addEventListener("click", () => playPlaylist());

  $("pl-tracks").addEventListener("keydown", (e) => {
    const row = e.target.closest(".up-next-row");
    if (e.key === "Enter" && row && !row.classList.contains("missing")) playPlaylist(row.dataset.path);
  });
  $("pl-tracks").addEventListener("click", async (e) => {
    const row = e.target.closest(".up-next-row");
    if (!row) return;
    const path = row.dataset.path;
    if (e.target.closest(".un-x") && !openPl.smart) {
      const paths = openPl.paths.filter((p) => p !== path);
      await invoke("save_playlist", { name: openPl.name, paths });
      openPl.paths = paths;
      return renderPlaylistDetail();
    }
    if (!row.classList.contains("missing")) playPlaylist(path);
  });

  let delTimer;
  $("pl-del").addEventListener("click", async (e) => {
    const b = e.currentTarget;
    if (b.dataset.sure !== "1") { // two clicks to delete, no dialog
      b.dataset.sure = "1"; b.textContent = "Click again to delete";
      delTimer = setTimeout(() => { b.dataset.sure = ""; b.textContent = "Delete playlist"; }, 3000);
      return;
    }
    clearTimeout(delTimer);
    b.dataset.sure = ""; b.textContent = "Delete playlist";
    try {
      await invoke("delete_playlist", { name: openPl.name });
      try { localStorage.removeItem(coverKey(openPl.name)); } catch (_) {}
    } catch (err) { toast(String(err)); }
    showView("library");
    refreshNames();
  });

  $("queue-save").addEventListener("click", (e) => {
    const pos = orderPos();
    const idxs = pos >= 0 ? order.slice(pos) : [];
    if (!idxs.length) return toast("Play something first: the queue is empty");
    askName(e.currentTarget, "Playlist name", async (name) => {
      try {
        await invoke("save_playlist", { name, paths: idxs.map((i) => currentTrackList[i].file_path) });
        await refreshNames();
        toast(`Saved ${idxs.length} tracks as “${name}”`, "Open", () => openPlaylist(name));
      } catch (err) { toast(String(err)); }
    });
  });

  refreshNames();
}

function boot() {
  initNavigation();
  initThemeEngine();
  initVisualTabs();
  initLyricsControls();
  initDevices();
  initOutputTools();
  loadAudioDevices();
  initLibrary();
  initContextMenu();
  initPalette();
  initSleep();
  initPlaylists();
  initPlaybackControls();
  initVolume();
  initArtToggle();
  initLogPanel();
  initShortcuts();
  initMpris();
  startStatusPolling();
  requestAnimationFrame(animateProgress);
}

// the module may finish evaluating after DOMContentLoaded (top-level await above)
if (document.readyState === "loading") addEventListener("DOMContentLoaded", boot); else boot();
