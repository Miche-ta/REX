//! Windows WASAPI Exclusive Mode audio backend.
//!
//! Opens the selected endpoint with `AUDCLNT_SHAREMODE_EXCLUSIVE` so the Windows Audio Engine
//! mixer and software resampler are bypassed, locking the DAC to the track's native sample rate
//! and bit depth.

use std::ptr;
use std::slice;
use std::thread;
use std::time::Duration;

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

use crate::audio_backend::{AudioBackend, OpenInfo, OpenReq, ProbeResult, WriteErr};
use crate::audio_probe::AudioDevice;
use crate::logger::log_msg;
use crate::pcm_format::{pick_format, OutFmt};

const WAVE_FORMAT_PCM_TAG: u16 = 1;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;

struct ComGuard;
impl ComGuard {
    fn init() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        Self
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize(); }
    }
}

pub struct WasapiBackend {
    _com: ComGuard,
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
    started: bool,
    info: OpenInfo,
}

unsafe impl Send for WasapiBackend {}

impl WasapiBackend {
    pub fn open(req: &OpenReq) -> Result<Self, ()> {
        let com = ComGuard::init();
        let device = find_endpoint(&req.device_id).map_err(|e| {
            log_msg("ERROR", format!("WASAPI endpoint '{}' not found: {e}", req.device_id));
        })?;

        let client: IAudioClient = unsafe {
            device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| log_msg("ERROR", format!("WASAPI Activate failed: {e}")))?
        };

        let exclusive = req.device_id != "default";
        let share_mode = if exclusive {
            AUDCLNT_SHAREMODE_EXCLUSIVE
        } else {
            AUDCLNT_SHAREMODE_SHARED
        };

        let (out_fmt, lossless) = pick_format(req.bits, |fmt| {
            let wfx = build_wave_format(req.sample_rate, req.channels, fmt);
            unsafe {
                client
                    .IsFormatSupported(share_mode, &wfx.Format, None)
                    .is_ok()
            }
        });

        let wfx = build_wave_format(req.sample_rate, req.channels, out_fmt);
        let cfg = crate::settings::get();
        let mut period_hns = (cfg.period_ms as i64).max(10) * 10_000;

        unsafe {
            let mut default_period = 0i64;
            let mut min_period = 0i64;
            if client
                .GetDevicePeriod(Some(&mut default_period), Some(&mut min_period))
                .is_ok()
            {
                period_hns = period_hns.max(min_period).max(default_period);
            }

            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            let init_res = client.Initialize(
                share_mode,
                flags,
                period_hns,
                if exclusive { period_hns } else { 0 },
                &wfx.Format,
                None,
            );
            if let Err(e) = init_res {
                if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                    let aligned_frames = client.GetBufferSize().unwrap_or(1024);
                    let aligned_hns =
                        (aligned_frames as i64 * 10_000_000) / req.sample_rate.max(1) as i64;
                    client
                        .Initialize(
                            share_mode,
                            flags,
                            aligned_hns,
                            if exclusive { aligned_hns } else { 0 },
                            &wfx.Format,
                            None,
                        )
                        .map_err(|err| {
                            log_msg("ERROR", format!("WASAPI aligned Initialize failed: {err}"));
                        })?;
                } else {
                    log_msg("ERROR", format!("WASAPI Initialize failed: {e}"));
                    return Err(());
                }
            }
        }

        let buffer_frames = unsafe { client.GetBufferSize().unwrap_or(0) };
        let event = unsafe {
            CreateEventW(None, false, false, None)
                .map_err(|e| log_msg("ERROR", format!("WASAPI CreateEventW failed: {e}")))?
        };
        unsafe {
            client
                .SetEventHandle(event)
                .map_err(|e| log_msg("ERROR", format!("WASAPI SetEventHandle failed: {e}")))?;
        }
        let render: IAudioRenderClient = unsafe {
            client
                .GetService()
                .map_err(|e| log_msg("ERROR", format!("WASAPI GetService failed: {e}")))?
        };

        let fmt_label = out_fmt.label(req.bits, lossless);
        log_msg(
            "WASAPI",
            format!(
                "Opened '{}' ({}) · {} Hz · {} · {} frames",
                req.device_id,
                if exclusive { "Exclusive" } else { "Shared" },
                req.sample_rate,
                fmt_label,
                buffer_frames
            ),
        );

        let info = OpenInfo {
            device_id: req.device_id.clone(),
            hw_rate: req.sample_rate,
            channels: req.channels,
            wide: req.bits >= 24,
            out_fmt,
            lossless,
            fmt: fmt_label,
            buffer: buffer_frames as u64,
            period: (buffer_frames / 2) as u64,
        };

        Ok(Self {
            _com: com,
            client,
            render,
            event,
            buffer_frames,
            started: false,
            info,
        })
    }

    fn ensure_started(&mut self) -> Result<(), WriteErr> {
        if !self.started {
            unsafe {
                self.client.Start().map_err(|e| {
                    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                        WriteErr::Disconnected
                    } else {
                        WriteErr::Fatal
                    }
                })?;
            }
            self.started = true;
        }
        Ok(())
    }

    fn write_raw(&mut self, src: &[u8], frames: usize, bytes_per_frame: usize) -> Result<(), WriteErr> {
        let mut offset = 0usize;
        while offset < frames {
            let padding = unsafe {
                self.client.GetCurrentPadding().map_err(|e| {
                    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                        WriteErr::Disconnected
                    } else {
                        WriteErr::Fatal
                    }
                })?
            };
            let avail = self.buffer_frames.saturating_sub(padding) as usize;
            if avail == 0 {
                self.ensure_started()?;
                let wait = unsafe { WaitForSingleObject(self.event, 200) };
                if wait != WAIT_OBJECT_0 {
                    thread::sleep(Duration::from_millis(2));
                }
                continue;
            }
            let chunk = (frames - offset).min(avail);
            unsafe {
                let ptr = self.render.GetBuffer(chunk as u32).map_err(|e| {
                    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                        WriteErr::Disconnected
                    } else {
                        WriteErr::Fatal
                    }
                })?;
                let byte_len = chunk * bytes_per_frame;
                let dst = slice::from_raw_parts_mut(ptr, byte_len);
                let src_off = offset * bytes_per_frame;
                dst.copy_from_slice(&src[src_off..src_off + byte_len]);
                self.render.ReleaseBuffer(chunk as u32, 0).map_err(|_| WriteErr::Fatal)?;
            }
            offset += chunk;
        }
        self.ensure_started()?;
        Ok(())
    }
}

impl AudioBackend for WasapiBackend {
    fn info(&self) -> &OpenInfo {
        &self.info
    }

    fn write_i16(&mut self, data: &[i16], frames: usize) -> Result<(), WriteErr> {
        let bytes_per_frame = self.info.channels as usize * 2;
        let bytes = unsafe { slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
        self.write_raw(bytes, frames, bytes_per_frame)
    }

    fn write_i32(&mut self, data: &[i32], frames: usize) -> Result<(), WriteErr> {
        let bytes_per_frame = self.info.channels as usize * 4;
        let bytes = unsafe { slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4) };
        self.write_raw(bytes, frames, bytes_per_frame)
    }

    fn write_bytes(&mut self, data: &[u8], frames: usize, bytes_per_frame: usize) -> Result<(), WriteErr> {
        self.write_raw(data, frames, bytes_per_frame)
    }

    fn delay_frames(&self) -> u64 {
        unsafe { self.client.GetCurrentPadding().unwrap_or(0) as u64 }
    }

    fn pause(&mut self, paused: bool) {
        unsafe {
            if paused {
                let _ = self.client.Stop();
                self.started = false;
            } else {
                let _ = self.client.Start();
                self.started = true;
            }
        }
    }

    fn drain(&mut self) {
        for _ in 0..200 {
            if self.delay_frames() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        unsafe {
            let _ = self.client.Stop();
        }
        self.started = false;
    }

    fn flush_and_prepare(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
            let _ = ResetEvent(self.event);
        }
        self.started = false;
    }

    fn close(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
        }
        self.started = false;
    }
}

impl Drop for WasapiBackend {
    fn drop(&mut self) {
        self.close();
    }
}

fn build_wave_format(rate: u32, channels: u32, fmt: OutFmt) -> WAVEFORMATEXTENSIBLE {
    let (bits_per_sample, valid_bits): (u16, u16) = match fmt {
        OutFmt::S16 => (16, 16),
        OutFmt::S243 => (24, 24),
        OutFmt::S24Low => (32, 24),
        OutFmt::S32 => (32, 32),
    };
    let block_align = channels as u16 * (bits_per_sample / 8);
    let avg_bytes_per_sec = rate * block_align as u32;
    let format = WAVEFORMATEX {
        wFormatTag: if bits_per_sample == 16 {
            WAVE_FORMAT_PCM_TAG
        } else {
            WAVE_FORMAT_EXTENSIBLE_TAG
        },
        nChannels: channels as u16,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: avg_bytes_per_sec,
        nBlockAlign: block_align,
        wBitsPerSample: bits_per_sample,
        cbSize: if bits_per_sample == 16 { 0 } else { 22 },
    };
    WAVEFORMATEXTENSIBLE {
        Format: format,
        Samples: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: if channels == 1 {
            SPEAKER_FRONT_LEFT
        } else {
            SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        },
        SubFormat: GUID::from_u128(KSDATAFORMAT_SUBTYPE_PCM.to_u128()),
    }
}

fn find_endpoint(device_id: &str) -> Result<IMMDevice, String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| e.to_string())?;
        if device_id == "default" {
            return enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| e.to_string());
        }
        let raw_id = device_id.strip_prefix("wasapi:").unwrap_or(device_id);
        let wide: Vec<u16> = raw_id.encode_utf16().chain(std::iter::once(0)).collect();
        enumerator
            .GetDevice(PCWSTR(wide.as_ptr()))
            .map_err(|e| e.to_string())
    }
}

pub fn probe_device(device_id: &str) -> ProbeResult {
    let _com = ComGuard::init();
    match find_endpoint(device_id) {
        Ok(_) => ProbeResult::Ready,
        Err(_) => ProbeResult::Missing,
    }
}

pub fn list_devices() -> Vec<AudioDevice> {
    let _com = ComGuard::init();
    let mut devices = Vec::new();
    unsafe {
        if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            if let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
                let count = collection.GetCount().unwrap_or(0);
                for i in 0..count {
                    if let Ok(dev) = collection.Item(i) {
                        let id_pwstr: PWSTR = dev.GetId().unwrap_or(PWSTR(ptr::null_mut()));
                        let id_str = if !id_pwstr.is_null() {
                            let s = id_pwstr.to_string().unwrap_or_default();
                            CoTaskMemFree(Some(id_pwstr.0 as *const _));
                            s
                        } else {
                            continue;
                        };
                        let friendly = dev
                            .OpenPropertyStore(STGM_READ)
                            .ok()
                            .and_then(|st| st.GetValue(&PKEY_Device_FriendlyName).ok())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| format!("WASAPI Device {i}"));
                        let is_usb = friendly.to_lowercase().contains("usb")
                            || friendly.to_lowercase().contains("dac");
                        devices.push(AudioDevice {
                            id: format!("wasapi:{id_str}"),
                            name: format!("{friendly} (WASAPI Exclusive)"),
                            description: "Bit-perfect WASAPI Exclusive Mode endpoint".into(),
                            is_hardware: true,
                            is_usb_dac: is_usb,
                            min_rate: 44100,
                            max_rate: 384000,
                            supported_rates: vec![44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000],
                            supports_16bit: true,
                            supports_24bit: true,
                            supports_32bit: true,
                        });
                    }
                }
            }
        }
    }
    devices.push(AudioDevice {
        id: "default".into(),
        name: "System Default (WASAPI Shared)".into(),
        description: "Windows default playback endpoint (shared mixer)".into(),
        is_hardware: false,
        is_usb_dac: false,
        min_rate: 44100,
        max_rate: 192000,
        supported_rates: vec![44100, 48000, 96000, 192000],
        supports_16bit: true,
        supports_24bit: true,
        supports_32bit: true,
    });
    devices
}
