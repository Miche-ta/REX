const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const esc = (t) => String(t).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

// localStorage can be missing or throw (private mode, blocked data): never depend on it.
const store = {
  get(k, d) { try { return localStorage.getItem(k) ?? d; } catch (_) { return d; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch (_) {} },
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
const VIEWS = ["now-playing", "library", "hardware", "themes", "terminal"];

function showView(name) {
  if (!VIEWS.includes(name)) name = "now-playing";
  document.querySelectorAll(".nav-item").forEach((i) => i.classList.toggle("active", i.dataset.view === name));
  document.querySelectorAll(".view-section").forEach((s) => s.classList.toggle("active", s.id === `${name}-view`));
  store.set("view", name);
  if (name === "terminal") { const o = $("log-output"); o.scrollTop = o.scrollHeight; }
}

function initNavigation() {
  document.querySelectorAll(".nav-item").forEach((i) => i.addEventListener("click", () => showView(i.dataset.view)));
  showView(store.get("view", "now-playing"));
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

    refreshVolume();
  } catch (err) {
    list.innerHTML = `<p class="empty">Could not list devices: ${esc(err)}</p>`;
  }
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

function renderLibrary() {
  const tbody = $("library-tbody");
  const empty = (msg) => { tbody.innerHTML = `<tr><td colspan="7" class="empty">${msg}</td></tr>`; };
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
    <tr class="track-row ${i === currentTrackIndex ? "playing" : ""}" data-index="${i}" tabindex="0">
      <td class="n">${n}</td>
      <td>${esc(t.title)}</td>
      <td class="dim">${esc(t.artist)}</td>
      <td class="dim">${esc(t.album)}</td>
      <td class="f dim">${esc(t.format)}</td>
      <td class="r">${t.bit_depth}/${+(t.sample_rate / 1000).toFixed(1)}</td>
      <td class="r">${formatDuration(t.duration_seconds)}</td>
    </tr>`;

  if (lib.view === "albums") {
    const groups = new Map();
    rows.forEach((r) => {
      const key = `${r.t.album}\u0000${r.t.artist}`;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(r);
    });
    tbody.innerHTML = [...groups].map(([key, g]) => {
      const mins = Math.round(g.reduce((s, r) => s + r.t.duration_seconds, 0) / 60);
      const head = `
        <tr class="group" data-group="${esc(key)}">
          <td colspan="7">${lib.collapsed.has(key) ? "+" : "−"} ${esc(g[0].t.album)}
            <span class="g-meta">${esc(g[0].t.artist)} · ${g.length} ${g.length === 1 ? "track" : "tracks"} · ${mins} min</span></td>
        </tr>`;
      return head + (lib.collapsed.has(key) ? "" : g.map((r, n) => row(r, n + 1)).join(""));
    }).join("");
  } else {
    tbody.innerHTML = rows.map((r, n) => row(r, n + 1)).join("");
  }
}

async function scan(dir) {
  $("library-tbody").innerHTML = '<tr><td colspan="7" class="empty">Scanning…</td></tr>';
  const playingPath = currentTrackList[currentTrackIndex]?.file_path;
  try {
    currentTrackList = (await invoke("scan_music_directory", { dirPath: dir })) || [];
  } catch (_) {
    currentTrackList = [];
  }
  // keep the queue pointing at whatever is playing
  currentTrackIndex = playingPath ? currentTrackList.findIndex((t) => t.file_path === playingPath) : -1;
  buildOrder(currentTrackIndex >= 0 ? currentTrackIndex : undefined);
  if (currentTrackIndex >= 0) renderUpNext();
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
  $("scan-demo-btn").addEventListener("click", () => scan("demo"));

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
    if (r) playTrackByIndex(Number(r.dataset.index));
  };
  tbody.addEventListener("click", activate);
  tbody.addEventListener("keydown", (e) => { if (e.key === "Enter") activate(e); });

  // open with the last folder, or the bundled demo files on first run
  if (savedDir) scan(savedDir);
  else {
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

function buildOrder(first) {
  orderFor = currentTrackList;
  order = currentTrackList.map((_, i) => i);
  if (!shuffleOn) return;
  for (let i = order.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [order[i], order[j]] = [order[j], order[i]];
  }
  if (first !== undefined) order = [first, ...order.filter((i) => i !== first)];
}
const orderPos = () => order.indexOf(currentTrackIndex);

// index of the track after the current one, or -1 at the end of the queue
function nextIndex(auto) {
  if (auto && repeatMode === "one") return currentTrackIndex;
  const pos = orderPos();
  if (pos >= 0 && pos < order.length - 1) return order[pos + 1];
  if (repeatMode === "all" && order.length) {
    if (shuffleOn) buildOrder();
    return order[0];
  }
  return -1;
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

  const row = (t, idx, label, cls = "") => `
    <div class="up-next-row ${cls}" data-index="${idx}">
      <span class="un-idx"><span class="un-num">${label}</span><span class="un-play"></span></span>
      <span class="un-main"><span class="un-title">${esc(t.title)}</span><span class="un-artist">${esc(t.artist)}</span></span>
      <span class="un-dur">${formatDuration(t.duration_seconds)}</span>
    </div>`;

  box.innerHTML = row(cur, currentTrackIndex, "now", "now") +
    upcoming.slice(0, 8).map((idx, i) => row(currentTrackList[idx], idx, i + 1)).join("") +
    (upcoming.length ? "" : '<p class="up-next-empty">End of queue.</p>');
  box.querySelectorAll(".up-next-row:not(.now)").forEach((r) =>
    r.addEventListener("click", () => playTrackByIndex(Number(r.dataset.index))));
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
  if (orderFor !== currentTrackList || !order.length) buildOrder(idx);
  const track = currentTrackList[idx];

  renderLibrary();
  renderUpNext();
  loadCover(track.file_path);

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

async function stopPlayback() {
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
  if (currentTrackIndex >= 0) { buildOrder(currentTrackIndex); renderUpNext(); }
  paintModes();
}

function cycleRepeat() {
  repeatMode = { off: "all", all: "one", one: "off" }[repeatMode];
  store.set("repeat", repeatMode);
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
  const resampled = s.sample_rate !== hz;
  $("signal-dot").className = `mark ${s.is_bit_perfect ? "ok" : resampled ? "info" : "warn"}`;
  $("signal-hud-text").textContent = s.is_bit_perfect
    ? "Bit-perfect · direct hardware"
    : resampled
      ? `In-engine resampled · ${k(s.sample_rate)} → ${k(hz)} kHz`
      : `Shared device · ${s.active_device_id}`;
}

// =====================================================================
// Volume: DAC hardware mixer (disabled for shared devices, where it would not be bit-perfect)
// =====================================================================
async function refreshVolume() {
  const slider = $("volume-slider");
  let v = null;
  try { v = await invoke("get_volume", { deviceId: currentDeviceId }); } catch (_) {}
  slider.disabled = v === null;
  slider.title = v === null ? "No hardware volume for this device" : "";
  if (v !== null) slider.value = v;
  $("vol-label").textContent = v === null ? "--" : `${v}%`;
}

let volTimer;
function setVolume(pct) {
  const slider = $("volume-slider");
  slider.value = pct;
  $("vol-label").textContent = `${slider.value}%`;
  // throttle: one mixer call per ~60ms, not one per slider pixel
  clearTimeout(volTimer);
  volTimer = setTimeout(() => invoke("set_volume", { deviceId: currentDeviceId, percent: Number(slider.value) }), 60);
}

function initVolume() {
  const slider = $("volume-slider");
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
  if (isPlaying && currentDuration > 0) {
    const pos = Math.min(currentDuration, livePos());
    $("progress-fill").style.width = `${(pos / currentDuration) * 100}%`;
    const t = formatDuration(pos);
    if (t !== lastTimeText) {
      lastTimeText = t;
      $("time-current").textContent = t;
    }
  }
  requestAnimationFrame(animateProgress);
}

let endedHandled = false;

function startStatusPolling() {
  setInterval(async () => {
    try {
      const status = await invoke("get_playback_status");
      if (!status) return;

      // Autoplay: the engine says the track ran to its natural end
      if (status.ended && !endedHandled) {
        endedHandled = true;
        const n = nextIndex(true);
        if (n >= 0) playTrackByIndex(n);
      } else if (!status.ended) endedHandled = false;

      isPlaying = status.is_playing;
      isPaused = status.is_paused;
      currentDuration = status.duration_seconds;
      currentPosition = status.position_seconds;

      // right after a seek, ignore polled positions until the engine has applied it
      if (performance.now() - lastSeekAt > 400) {
        lastPollPos = currentPosition;
        lastPollAt = performance.now();
      }

      updatePlayPauseButton();

      if (!isPlaying) return setIdleStatus();

      $("time-duration").textContent = formatDuration(currentDuration);
      $("np-title").textContent = status.title;
      $("np-artist").textContent = status.artist;
      $("np-album").textContent = status.album;
      $("mini-title").textContent = status.title;
      $("mini-artist").textContent = status.artist;
      showStatus(status);
    } catch (_) {}
  }, 250);
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
      "/": () => { showView("library"); $("library-search").focus(); },
      "?": () => dlg.showModal(),
    };
    const act = actions[e.key];
    if (act) { e.preventDefault(); act(); }
    else if (e.key >= "1" && e.key <= "5") { e.preventDefault(); showView(VIEWS[Number(e.key) - 1]); }
  });
}

// =====================================================================
// Bootstrap
// =====================================================================
window.addEventListener("DOMContentLoaded", () => {
  initNavigation();
  initThemeEngine();
  initDevices();
  loadAudioDevices();
  initLibrary();
  initPlaybackControls();
  initVolume();
  initArtToggle();
  initLogPanel();
  initShortcuts();
  startStatusPolling();
  requestAnimationFrame(animateProgress);
});
