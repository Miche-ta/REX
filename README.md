# rex-music

A bit-perfect music player for Linux. It plays FLAC/WAV straight to an ALSA `hw:` device, with no mixer and no
resampling, so a USB DAC gets the file's exact samples. Built with Tauri v2 (Rust backend, plain HTML/CSS/JS frontend).

## Features
- Direct ALSA output (`S32_LE` for 24-bit), resampling disabled, hardware DAC volume through the ALSA mixer
- Library scan with search, sort, Hi-res/CD filter, Tracks/Albums grouping
- Now Playing with cover art or disc, VU meter, synced lyrics (`.lrc` or embedded tags), queue with shuffle/repeat
- Signal readout (source, output, clock, format, buffer/period)
- MPRIS media-key support
- Six themes plus a customizer, exportable as JSON
- Terminal tab with engine logs (device opens, ALSA errors, PipeWire hand-off)

## Run
Needs Rust, the Tauri v2 prerequisites for Linux, and ALSA dev headers (`alsa-lib`).

```sh
cd src-tauri
cargo tauri dev      # or: cargo tauri build
```

The frontend is in `src/` and has no build step: edit `index.html`, `styles.css`, `main.js` and reload.

## How output works
PipeWire normally owns the sound card. Before playing to a `hw:` device, rex-music suspends that card's PipeWire
sinks with `pactl suspend-sink` and gives them back when playback stops or the app exits.
To stop WirePlumber from re-grabbing the card, set `session.suspend-timeout-seconds = 0` for it.

## Keyboard shortcuts
`Space` play/pause · `←/→` seek 5 s · `↑/↓` volume · `n`/`p` next/previous · `s` shuffle · `r` repeat ·
`/` search · `1`–`5` views · `?` help

## Known issues
- Some Intel xHCI USB controllers log `not part of TD` errors with isochronous audio and can freeze the desktop
  during direct ALSA playback. Try another port, the other controller, or `options snd-usb-audio lowlatency=0`.
- Switching tracks mid-playback can briefly report `EBUSY` while the device is released.
- The queue follows file order, not the library's column sort.

## License
See `LICENSE`.
