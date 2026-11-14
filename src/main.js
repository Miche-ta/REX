const { invoke } = window.__TAURI__.core;

// =====================================================================
// Global State
// =====================================================================
let currentTrackList = [];
let currentTrackIndex = -1;
let currentDeviceId = "default";
let isPlaying = false;
let isPaused = false;
let currentDuration = 0;
let currentPosition = 0;

// =====================================================================
// View Navigation
// =====================================================================
function initNavigation() {
  const navItems = document.querySelectorAll(".nav-item");
  const viewSections = document.querySelectorAll(".view-section");

  navItems.forEach((item) => {
    item.addEventListener("click", () => {
      const viewName = item.getAttribute("data-view");

      navItems.forEach((i) => i.classList.remove("active"));
      item.classList.add("active");

      viewSections.forEach((sec) => sec.classList.remove("active"));
      const targetSec = document.getElementById(`${viewName}-view`);
      if (targetSec) targetSec.classList.add("active");
    });
  });
}

// =====================================================================
// Hardware Audio Devices (ALSA Direct)
// =====================================================================
async function loadAudioDevices() {
  const grid = document.getElementById("hw-devices-grid");
  const quickSelect = document.getElementById("quick-device-select");

  try {
    const devices = await invoke("list_audio_devices");
    if (!devices || devices.length === 0) {
      grid.innerHTML = "<p>No audio devices found.</p>";
      return;
    }

    // If currentDeviceId is still "default", pick the first USB DAC (e.g. hw:2,0 or plughw:2,0)
    if (currentDeviceId === "default") {
      const bestHw = devices.find(d => d.id.startsWith("plughw:") || d.id.startsWith("hw:")) || devices[0];
      if (bestHw) currentDeviceId = bestHw.id;
    }

    // Populate topbar dropdown
    quickSelect.innerHTML = devices.map(d => `
      <option value="${d.id}" ${d.id === currentDeviceId ? 'selected' : ''}>
        ${d.name} ${d.is_hardware ? '⚡ [Bit-Perfect]' : ''}
      </option>
    `).join("");

    // Populate Hardware setup grid
    grid.innerHTML = devices.map(d => `
      <div class="hw-card ${d.id === currentDeviceId ? 'selected' : ''}" data-device-id="${d.id}">
        <div class="hw-card-header">
          <div>
            <div class="hw-card-name">${d.name}</div>
            <div class="hw-card-id">${d.id}</div>
          </div>
          ${d.is_hardware ? '<span class="badge highlight">Direct HW</span>' : '<span class="badge">Shared</span>'}
        </div>
        <p style="font-size: 0.78rem; color: var(--text-secondary);">${d.description || 'Hardware playback device'}</p>
        
        <div>
          <span style="font-size: 0.7rem; color: var(--text-muted); font-weight: 700;">SUPPORTED RATES:</span>
          <div class="pill-row" style="margin-top: 4px;">
            ${d.supported_rates.length > 0
              ? d.supported_rates.map(r => `<span class="pill">${r / 1000} kHz</span>`).join('')
              : '<span class="pill">System Managed (Up to 192 kHz)</span>'}
          </div>
        </div>

        <div>
          <span style="font-size: 0.7rem; color: var(--text-muted); font-weight: 700;">BIT TRANSPARENCY:</span>
          <div class="pill-row" style="margin-top: 4px;">
            ${d.supports_16bit ? '<span class="pill">16-bit Integer</span>' : ''}
            ${d.supports_24bit ? '<span class="pill">24-bit Integer</span>' : ''}
            ${d.supports_32bit ? '<span class="pill">32-bit Integer</span>' : ''}
          </div>
        </div>

        <button class="btn-action select-hw-btn" style="margin-top: 8px; width: 100%;" data-device-id="${d.id}">
          ${d.id === currentDeviceId ? '✓ Active Output' : 'Select for Bit-Perfect Output'}
        </button>
      </div>
    `).join("");

    // Setup card selection
    document.querySelectorAll(".select-hw-btn").forEach(btn => {
      btn.addEventListener("click", () => {
        currentDeviceId = btn.getAttribute("data-device-id");
        quickSelect.value = currentDeviceId;
        loadAudioDevices();
      });
    });

    refreshVolume();
  } catch (err) {
    grid.innerHTML = `<p style="color: #ff5555;">Error scanning ALSA devices: ${err}</p>`;
  }
}

// =====================================================================
// Theme Engine (Community CSS Variables)
// =====================================================================
function initThemeEngine() {
  const quickThemeSelect = document.getElementById("quick-theme-select");
  const themeCards = document.querySelectorAll(".theme-card");

  const savedTheme = localStorage.getItem("rex-music-theme") || "oled";
  applyTheme(savedTheme);
  quickThemeSelect.value = savedTheme;

  quickThemeSelect.addEventListener("change", (e) => {
    applyTheme(e.target.value);
  });

  themeCards.forEach(card => {
    card.addEventListener("click", () => {
      const theme = card.getAttribute("data-theme-name");
      applyTheme(theme);
      quickThemeSelect.value = theme;
    });
  });

  // Color Pickers for live customization
  const pickBg = document.getElementById("picker-bg-app");
  const pickCard = document.getElementById("picker-bg-card");
  const pickAccent = document.getElementById("picker-accent");
  const pickText = document.getElementById("picker-text");

  pickBg.addEventListener("input", (e) => document.documentElement.style.setProperty("--bg-app", e.target.value));
  pickCard.addEventListener("input", (e) => document.documentElement.style.setProperty("--bg-card", e.target.value));
  pickAccent.addEventListener("input", (e) => {
    document.documentElement.style.setProperty("--accent", e.target.value);
    document.documentElement.style.setProperty("--progress-fill", e.target.value);
  });
  pickText.addEventListener("input", (e) => document.documentElement.style.setProperty("--text-primary", e.target.value));

  // Export theme.json
  document.getElementById("export-theme-btn").addEventListener("click", () => {
    const themeObj = {
      name: "Custom Community Theme",
      author: "rex-music user",
      version: "1.0.0",
      colors: {
        "--bg-app": pickBg.value,
        "--bg-card": pickCard.value,
        "--accent": pickAccent.value,
        "--text-primary": pickText.value,
      }
    };
    const blob = new Blob([JSON.stringify(themeObj, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "rex-theme.json";
    a.click();
    URL.revokeObjectURL(url);
  });
}

function applyTheme(themeName) {
  document.documentElement.setAttribute("data-theme", themeName);
  localStorage.setItem("rex-music-theme", themeName);

  document.querySelectorAll(".theme-card").forEach(c => {
    if (c.getAttribute("data-theme-name") === themeName) {
      c.classList.add("active");
    } else {
      c.classList.remove("active");
    }
  });
}

// =====================================================================
// Library Management
// =====================================================================
async function initLibrary() {
  const scanInput = document.getElementById("scan-path-input");
  const scanBtn = document.getElementById("scan-folder-btn");
  const scanDemoBtn = document.getElementById("scan-demo-btn");
  const searchInput = document.getElementById("library-search");

  // Get default music dir
  try {
    const defaultDir = await invoke("get_default_music_dir");
    scanInput.value = defaultDir;
  } catch (_) {}

  scanBtn.addEventListener("click", async () => {
    const dir = scanInput.value.trim();
    if (!dir) return;
    renderScanningState();
    const tracks = await invoke("scan_music_directory", { dirPath: dir });
    currentTrackList = tracks || [];
    renderTrackTable(currentTrackList);
  });

  scanDemoBtn.addEventListener("click", async () => {
    renderScanningState();
    // Scan demo directory created in project
    const tracks = await invoke("scan_music_directory", { dirPath: "demo" });
    currentTrackList = tracks || [];
    renderTrackTable(currentTrackList);
  });

  searchInput.addEventListener("input", (e) => {
    const q = e.target.value.toLowerCase();
    const filtered = currentTrackList.filter(t =>
      t.title.toLowerCase().includes(q) ||
      t.artist.toLowerCase().includes(q) ||
      t.album.toLowerCase().includes(q)
    );
    renderTrackTable(filtered);
  });

  // Automatically scan demo directory initially if available
  try {
    const tracks = await invoke("scan_music_directory", { dirPath: "demo" });
    if (tracks && tracks.length > 0) {
      currentTrackList = tracks;
      renderTrackTable(currentTrackList);
    }
  } catch (_) {}
}

function renderScanningState() {
  const tbody = document.getElementById("library-tbody");
  tbody.innerHTML = `<tr><td colspan="7" style="text-align: center; color: var(--accent); padding: 30px;">Scanning files with bit-exact metadata parser...</td></tr>`;
}

function formatDuration(sec) {
  if (!sec || isNaN(sec)) return "0:00";
  const m = Math.floor(sec / 60);
  const s = Math.floor(sec % 60);
  return `${m}:${s < 10 ? '0' : ''}${s}`;
}

function renderTrackTable(tracks) {
  const tbody = document.getElementById("library-tbody");
  if (!tracks || tracks.length === 0) {
    tbody.innerHTML = `<tr><td colspan="7" style="text-align: center; color: var(--text-muted); padding: 30px;">No music tracks found in this directory.</td></tr>`;
    return;
  }

  tbody.innerHTML = tracks.map((t, idx) => `
    <tr class="track-row ${currentTrackIndex === idx ? 'playing' : ''}" data-index="${idx}">
      <td style="color: var(--text-muted); font-size: 0.75rem;">${idx + 1}</td>
      <td style="font-weight: 600;">${t.title}</td>
      <td style="color: var(--text-secondary);">${t.artist}</td>
      <td style="color: var(--text-muted);">${t.album}</td>
      <td><span class="badge highlight">${t.format}</span></td>
      <td style="font-family: monospace; font-size: 0.75rem;">${t.bit_depth}-bit / ${(t.sample_rate / 1000).toFixed(1)}k</td>
      <td style="text-align: right; font-family: monospace; color: var(--text-muted);">${formatDuration(t.duration_seconds)}</td>
    </tr>
  `).join("");

  document.querySelectorAll(".track-row").forEach(row => {
    row.addEventListener("click", () => {
      const idx = parseInt(row.getAttribute("data-index"), 10);
      playTrackByIndex(idx);
    });
  });
}

// =====================================================================
// Playback Engine & Polling
// =====================================================================
async function playTrackByIndex(idx) {
  if (idx < 0 || idx >= currentTrackList.length) return;
  currentTrackIndex = idx;
  if (orderFor !== currentTrackList || !order.length) buildOrder(idx);
  const track = currentTrackList[idx];

  renderTrackTable(currentTrackList);
  renderUpNext();
  loadCover(track.file_path);

  try {
    await invoke("play_track", {
      filePath: track.file_path,
      deviceId: currentDeviceId,
    });
    isPlaying = true;
    isPaused = false;
    updatePlayPauseButton();
  } catch (err) {
    console.error("Playback error:", err);
  }
}

const esc = (t) => String(t).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

// ---- Queue order: shuffle / repeat ----
let shuffleOn = false, repeatMode = "off"; // off | all | one
try {
  shuffleOn = localStorage.getItem("shuffle") === "1";
  repeatMode = localStorage.getItem("repeat") || "off";
} catch (_) {}
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
  const box = document.getElementById("up-next-list");
  const sum = document.getElementById("up-next-summary");
  const pos = orderPos();
  const upcoming = pos >= 0 ? order.slice(pos + 1) : [];
  const cur = currentTrackList[currentTrackIndex];
  if (!cur) return;

  const mins = Math.round(upcoming.reduce((s, i) => s + currentTrackList[i].duration_seconds, 0) / 60);
  sum.textContent = upcoming.length ? `${upcoming.length} more · ${mins} min` : "";

  const row = (t, idx, label, cls = "") => `
    <div class="up-next-row ${cls}" data-index="${idx}">
      <span class="un-idx"><span class="un-num">${label}</span><span class="un-play">▶</span></span>
      <span class="un-main"><span class="un-title">${esc(t.title)}</span><span class="un-artist">${esc(t.artist)}</span></span>
      <span class="un-dur">${formatDuration(t.duration_seconds)}</span>
    </div>`;

  box.innerHTML = row(cur, currentTrackIndex, "♪", "now") +
    upcoming.slice(0, 8).map((idx, i) => row(currentTrackList[idx], idx, i + 1)).join("") +
    (upcoming.length ? "" : '<p class="up-next-empty">End of queue.</p>');
  box.querySelectorAll(".up-next-row:not(.now)").forEach((r) =>
    r.addEventListener("click", () => playTrackByIndex(Number(r.dataset.index))));
}

// Cover art: shown as disc label, or as a square cover (toggle by clicking the artwork)
async function loadCover(filePath) {
  let url = null;
  try { url = await invoke("get_cover", { filePath }); } catch (_) {}
  const img = document.getElementById("cover-img");
  const center = document.querySelector(".vinyl-center");
  const wrap = document.getElementById("art-wrapper");
  img.hidden = !url;
  if (url) img.src = url;
  center.style.backgroundImage = url ? `url("${url}")` : "";
  center.classList.toggle("has-cover", !!url);
  wrap.classList.toggle("has-cover", !!url);
}

function initArtToggle() {
  const wrap = document.getElementById("art-wrapper");
  const apply = (square) => {
    wrap.classList.toggle("square", square);
  };
  let square = false;
  try { square = localStorage.getItem("artSquare") === "1"; } catch (_) {}
  apply(square);
  wrap.addEventListener("click", () => {
    if (!wrap.classList.contains("has-cover")) return; // nothing to show as a square
    square = !wrap.classList.contains("square");
    apply(square);
    try { localStorage.setItem("artSquare", square ? "1" : "0"); } catch (_) {}
  });
}

function initPlaybackControls() {
  const playBtn = document.getElementById("btn-play");
  const stopBtn = document.getElementById("btn-stop");
  const prevBtn = document.getElementById("btn-prev");
  const nextBtn = document.getElementById("btn-next");
  const progressBar = document.getElementById("progress-bar");

  playBtn.addEventListener("click", async () => {
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
  });

  stopBtn.addEventListener("click", async () => {
    await invoke("stop_track");
    isPlaying = false;
    isPaused = false;
    updatePlayPauseButton();
    resetNowPlayingUI();
  });

  prevBtn.addEventListener("click", () => {
    const pos = orderPos();
    if (currentPosition > 3 || pos <= 0) {
      invoke("seek_track", { positionSeconds: 0 }); // restart current track
    } else {
      playTrackByIndex(order[pos - 1]);
    }
  });

  nextBtn.addEventListener("click", () => {
    const n = nextIndex(false);
    if (n >= 0) playTrackByIndex(n);
  });

  // Shuffle / repeat toggles
  const shuffleBtn = document.getElementById("btn-shuffle");
  const repeatBtn = document.getElementById("btn-repeat");
  const paintModes = () => {
    shuffleBtn.classList.toggle("on", shuffleOn);
    repeatBtn.classList.toggle("on", repeatMode !== "off");
    repeatBtn.textContent = repeatMode === "one" ? "↻¹" : "↻";
    repeatBtn.title = `Repeat: ${repeatMode}`;
  };
  shuffleBtn.addEventListener("click", () => {
    shuffleOn = !shuffleOn;
    try { localStorage.setItem("shuffle", shuffleOn ? "1" : "0"); } catch (_) {}
    if (currentTrackIndex >= 0) { buildOrder(currentTrackIndex); renderUpNext(); }
    paintModes();
  });
  repeatBtn.addEventListener("click", () => {
    repeatMode = { off: "all", all: "one", one: "off" }[repeatMode];
    try { localStorage.setItem("repeat", repeatMode); } catch (_) {}
    paintModes();
  });
  paintModes();

  // Seeking on progress bar
  progressBar.addEventListener("click", async (e) => {
    if (!currentDuration) return;
    const rect = progressBar.getBoundingClientRect();
    const clickX = e.clientX - rect.left;
    const fraction = Math.max(0, Math.min(1, clickX / rect.width));
    const targetSeconds = fraction * currentDuration;

    lastSeekAt = performance.now();
    lastPollPos = targetSeconds; // bar jumps instantly
    lastPollAt = lastSeekAt;
    await invoke("seek_track", { positionSeconds: targetSeconds });
  });

  // Quick device select
  document.getElementById("quick-device-select").addEventListener("change", (e) => {
    currentDeviceId = e.target.value;
    loadAudioDevices();
  });
}

function updatePlayPauseButton() {
  const btn = document.getElementById("btn-play");
  const vinyl = document.getElementById("vinyl-disc");

  if (isPlaying && !isPaused) {
    btn.textContent = "⏸";
    vinyl.classList.add("spinning");
  } else {
    btn.textContent = "▶";
    vinyl.classList.remove("spinning");
  }
}

function resetNowPlayingUI() {
  document.getElementById("time-current").textContent = "0:00";
  document.getElementById("progress-fill").style.width = "0%";
  document.getElementById("signal-dot").classList.remove("active");
  document.getElementById("signal-hud-text").textContent = "Signal Path: Idle";
}

let endedHandled = false;
let lastSeekAt = 0;

// Volume: DAC hardware mixer through ALSA (disabled for non-hw devices, where it would not be bit-perfect)
async function refreshVolume() {
  const slider = document.getElementById("volume-slider");
  const label = document.getElementById("vol-label");
  const v = await invoke("get_volume", { deviceId: currentDeviceId });
  slider.disabled = v === null;
  slider.title = v === null ? "No hardware volume for this device" : "";
  if (v !== null) slider.value = v;
  label.textContent = v === null ? "--" : `${v}%`;
}
function initVolume() {
  const slider = document.getElementById("volume-slider");
  slider.addEventListener("input", () => {
    document.getElementById("vol-label").textContent = `${slider.value}%`;
    invoke("set_volume", { deviceId: currentDeviceId, percent: Number(slider.value) });
  });
}

// Smooth progress: poll gives position every 250ms, so extrapolate each frame
let lastPollPos = 0, lastPollAt = 0, lastTimeText = "";
function animateProgress() {
  if (isPlaying && currentDuration > 0) {
    const elapsed = isPaused ? 0 : (performance.now() - lastPollAt) / 1000;
    const pos = Math.min(currentDuration, lastPollPos + elapsed);
    document.getElementById("progress-fill").style.width = `${(pos / currentDuration) * 100}%`;
    const t = formatDuration(pos);
    if (t !== lastTimeText) {
      lastTimeText = t;
      document.getElementById("time-current").textContent = t;
    }
  }
  requestAnimationFrame(animateProgress);
}

// Poll engine state every 250ms
function startStatusPolling() {
  setInterval(async () => {
    try {
      const status = await invoke("get_playback_status");
      if (!status) return;

      // Autoplay: engine says the track ran to its natural end -> next one
      if (status.ended && !endedHandled) {
        endedHandled = true;
        const n = nextIndex(true);
        if (n >= 0) playTrackByIndex(n);
      } else if (!status.ended) endedHandled = false;

      isPlaying = status.is_playing;
      isPaused = status.is_paused;
      currentDuration = status.duration_seconds;
      currentPosition = status.position_seconds;
      // right after a seek, ignore polls until the engine has applied it
      if (performance.now() - lastSeekAt > 400) {
      }

      updatePlayPauseButton();

      if (isPlaying) {
        // Update seekbar
        lastPollPos = currentPosition;
        lastPollAt = performance.now();
        document.getElementById("time-duration").textContent = formatDuration(currentDuration);

        // Update Now Playing headers
        document.getElementById("np-title").textContent = status.title;
        document.getElementById("np-artist").textContent = status.artist;
        document.getElementById("np-album").textContent = status.album;

        // Mini player
        document.getElementById("mini-title").textContent = status.title;
        document.getElementById("mini-artist").textContent = status.artist;

        // Badges
        document.getElementById("np-badge-bits").textContent = `${status.bit_depth}-bit`;
        document.getElementById("np-badge-rate").textContent = `${(status.sample_rate / 1000).toFixed(1)} kHz`;
        document.getElementById("np-badge-channels").textContent = status.channels === 1 ? "Mono" : "Stereo";

        // Signal HUD & Bit-Perfect Badge
        const signalDot = document.getElementById("signal-dot");
        const signalHudText = document.getElementById("signal-hud-text");
        const signalChain = document.getElementById("np-signal-chain");

        signalDot.classList.add("active");

        const srcRateK = (status.sample_rate / 1000).toFixed(1);
        const hwRateK = (status.active_hardware_rate / 1000).toFixed(1);

        if (status.is_bit_perfect) {
          signalHudText.textContent = `⚡ Bit-Perfect Direct ALSA (${hwRateK} kHz · ${status.active_hardware_format} · 0 Resampling)`;
          signalChain.textContent = `Source [${status.bit_depth}b/${srcRateK}k] ➔ rex-music [Bit-Exact Pipeline] ➔ DAC [${status.active_device_id} @ ${hwRateK}kHz · ${status.active_hardware_format}] (Zero DSP)`;
          signalDot.style.backgroundColor = "var(--accent)";
          signalDot.style.boxShadow = "0 0 10px var(--accent)";
        } else {
          signalHudText.textContent = `⚠️ Fallback: ${status.active_device_id} (DAC locked at ${hwRateK} kHz)`;
          signalChain.textContent = `Source [${status.bit_depth}b/${srcRateK}k] ➔ System Mixer ➔ DAC [${status.active_device_id}] (WirePlumber holding device)`;
          signalDot.style.backgroundColor = "#ff5555";
          signalDot.style.boxShadow = "0 0 10px #ff5555";
        }
      } else {
        document.getElementById("signal-dot").classList.remove("active");
        document.getElementById("signal-hud-text").textContent = "Signal Path: Idle";
      }
    } catch (_) {}
  }, 250);
}

// Engine log panel: poll backend buffer, re-render only when it changes
let lastLogCount = -1;
function initLogPanel() {
  const out = document.getElementById("log-output");
  document.getElementById("clear-logs-btn").addEventListener("click", async () => {
    await invoke("clear_engine_logs");
    lastLogCount = -1;
  });
  setInterval(async () => {
    try {
      const logs = await invoke("get_engine_logs");
      if (logs.length === lastLogCount) return;
      lastLogCount = logs.length;
      const stick = out.scrollTop + out.clientHeight >= out.scrollHeight - 20;
      out.replaceChildren(...logs.map((l) => {
        const div = document.createElement("div");
        div.className = `log-line ${l.level.toLowerCase()}`;
        const t = document.createElement("span");
        t.className = "log-time";
        t.textContent = l.timestamp;
        div.append(t, document.createTextNode(`[${l.level}] ${l.message}`));
        return div;
      }));
      if (stick) out.scrollTop = out.scrollHeight;
    } catch (e) { /* backend not ready */ }
  }, 500);
}

// =====================================================================
// Bootstrap
// =====================================================================
window.addEventListener("DOMContentLoaded", () => {
  initNavigation();
  initThemeEngine();
  loadAudioDevices();
  initLibrary();
  initPlaybackControls();
  startStatusPolling();
  initVolume();
  initArtToggle();
  requestAnimationFrame(animateProgress);
  initLogPanel();
});
