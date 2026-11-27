# rex-music

Heavily vibe-coded music player.

A bit-perfect, gapless music player built with Tauri v2 (Rust backend, plain HTML/CSS/JS frontend). It sends decoded PCM and DoP (DSD over PCM) straight to your DAC with no mixer and no resampling—via direct ALSA `hw:` on Linux, with WASAPI Exclusive (Windows) and CoreAudio Hog Mode (macOS) backends.

## Features
- **Zero-jitter audio pipeline**: Two-thread decoder/writer architecture connected by a lock-free SPSC ring buffer, RTKit / MMCSS real-time thread priority, gapless hand-off, and USB DAC hot-plug reconnection.
- **Bit-perfect output & formats**: Direct hardware output (`S32_LE`, `S24_LE`, `S24_3LE`, `S16_LE`), DSD (`.dsf` / `.dff` via DoP v1.1), CUE sheet virtual tracks, 0 dBFS hardware volume lock, hardware DAC mixer volume with ReplayGain, and a live Signal Path readout.
- **Visualizers & Now Playing**: Latency-compensated 32-band FFT spectrum analyzer, goniometer phase scope with stereo correlation & Peak/RMS meters, VU meter, smooth seekbar, cover art/disc, synced lyrics (`.lrc`, embedded tags, and opt-in LRCLIB online lookup), and queue with shuffle/repeat.
- **Library & UI**: Disk-cached incremental library scan with search, sort, Hi-res/CD filter, Tracks/Albums/Folders views, `.m3u` playlists, `Ctrl+K` command palette, sleep timer, and six themes with a JSON-exportable customizer.
- **OS integration & diagnostics**: MPRIS media-key support on Linux (with Windows SMTC and macOS NowPlaying hooks), standard cross-platform config/cache/data directories, and a Terminal tab with engine logs, kernel `hw_params`, health counters, and copyable diagnostics reports.

## Run
Needs Rust and the Tauri v2 prerequisites for your OS (on Linux, also `alsa-lib` development headers).

```sh
cd src-tauri
cargo tauri dev      # or: cargo tauri build
```

Run unit tests:

```sh
cd src-tauri
cargo test
```

The frontend lives in `src/` and has no build step: edit `index.html`, `styles.css`, or `main.js` and reload.

## How output works
- **Linux (ALSA + PipeWire):** PipeWire normally holds the sound card open. Before playing to a `hw:` device, rex-music suspends that card's PipeWire sinks via `pactl suspend-sink` (or requests release over D-Bus `ReserveDevice1` when enabled) and returns them when playback stops or the app exits. To stop WirePlumber from re-grabbing the card, set `session.suspend-timeout-seconds = 0` for it.
- **Windows / macOS:** Uses WASAPI Exclusive Mode (`AUDCLNT_SHAREMODE_EXCLUSIVE`) on Windows and CoreAudio HAL Hog Mode on macOS *(implemented, pending native build & hardware verification)*.

## Keyboard shortcuts
`Space` play/pause · `←/→` seek 5 s · `↑/↓` volume · `n`/`p` next/previous · `s` shuffle · `r` repeat ·
`/` search · `Ctrl+K` command palette · `1`–`5` views · `?` help

## Known issues
- Some Intel xHCI USB controllers log `not part of TD` errors with isochronous audio and can freeze the desktop
  during direct ALSA playback. Try another port, the other controller, or `options snd-usb-audio lowlatency=0`.
- Switching tracks mid-playback can briefly report `EBUSY` while the device is released.
- The queue follows file order, not the library's column sort.

## License
See `LICENSE`.
