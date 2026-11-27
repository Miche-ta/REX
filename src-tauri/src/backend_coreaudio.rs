//! macOS CoreAudio HAL Exclusive / Hog Mode audio backend.
//!
//! Uses macOS CoreAudio (`AudioObjectGetPropertyData` / `AudioDeviceCreateIOProcID`) to
//! switch the DAC's nominal sample rate to match the track and optionally claim Hog Mode
//! (`kAudioDevicePropertyHogMode`) for bit-perfect output.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::audio_backend::{AudioBackend, OpenInfo, OpenReq, ProbeResult, WriteErr};
use crate::audio_probe::AudioDevice;
use crate::logger::log_msg;
use crate::pcm_format::OutFmt;
use crate::ring_buffer::SpscRing;

#[allow(non_upper_case_globals, dead_code)]
mod sys {
    use std::ffi::c_void;

    pub type AudioObjectID = u32;
    pub type AudioObjectPropertySelector = u32;
    pub type AudioObjectPropertyScope = u32;
    pub type AudioObjectPropertyElement = u32;
    pub type OSStatus = i32;
    pub type AudioDeviceIOProcID = *mut c_void;

    pub const kAudioObjectSystemObject: AudioObjectID = 1;
    pub const kAudioObjectPropertyScopeGlobal: AudioObjectPropertyScope = u32::from_be_bytes(*b"glob");
    pub const kAudioObjectPropertyScopeOutput: AudioObjectPropertyScope = u32::from_be_bytes(*b"outp");
    pub const kAudioObjectPropertyElementMain: AudioObjectPropertyElement = 0;

    pub const kAudioHardwarePropertyDevices: AudioObjectPropertySelector = u32::from_be_bytes(*b"dev#");
    pub const kAudioHardwarePropertyDefaultOutputDevice: AudioObjectPropertySelector = u32::from_be_bytes(*b"dOut");
    pub const kAudioDevicePropertyNominalSampleRate: AudioObjectPropertySelector = u32::from_be_bytes(*b"nsrt");
    pub const kAudioDevicePropertyHogMode: AudioObjectPropertySelector = u32::from_be_bytes(*b"oink");
    pub const kAudioDevicePropertyDeviceNameCFString: AudioObjectPropertySelector = u32::from_be_bytes(*b"lnam");

    #[repr(C)]
    pub struct AudioObjectPropertyAddress {
        pub mSelector: AudioObjectPropertySelector,
        pub mScope: AudioObjectPropertyScope,
        pub mElement: AudioObjectPropertyElement,
    }

    #[repr(C)]
    pub struct AudioBuffer {
        pub mNumberChannels: u32,
        pub mDataByteSize: u32,
        pub mData: *mut c_void,
    }

    #[repr(C)]
    pub struct AudioBufferList {
        pub mNumberBuffers: u32,
        pub mBuffers: [AudioBuffer; 1],
    }

    pub type AudioDeviceIOProc = extern "C" fn(
        inDevice: AudioObjectID,
        inNow: *const c_void,
        inInputData: *const AudioBufferList,
        inInputTime: *const c_void,
        outOutputData: *mut AudioBufferList,
        inOutputTime: *const c_void,
        inClientData: *mut c_void,
    ) -> OSStatus;

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        pub fn AudioObjectGetPropertyDataSize(
            inObjectID: AudioObjectID,
            inAddress: *const AudioObjectPropertyAddress,
            inQualifierDataSize: u32,
            inQualifierData: *const c_void,
            outDataSize: *mut u32,
        ) -> OSStatus;

        pub fn AudioObjectGetPropertyData(
            inObjectID: AudioObjectID,
            inAddress: *const AudioObjectPropertyAddress,
            inQualifierDataSize: u32,
            inQualifierData: *const c_void,
            ioDataSize: *mut u32,
            outData: *mut c_void,
        ) -> OSStatus;

        pub fn AudioObjectSetPropertyData(
            inObjectID: AudioObjectID,
            inAddress: *const AudioObjectPropertyAddress,
            inQualifierDataSize: u32,
            inQualifierData: *const c_void,
            inDataSize: u32,
            inData: *const c_void,
        ) -> OSStatus;

        pub fn AudioDeviceCreateIOProcID(
            inDevice: AudioObjectID,
            inProc: AudioDeviceIOProc,
            inClientData: *mut c_void,
            outIOProcID: *mut AudioDeviceIOProcID,
        ) -> OSStatus;

        pub fn AudioDeviceDestroyIOProcID(
            inDevice: AudioObjectID,
            inIOProcID: AudioDeviceIOProcID,
        ) -> OSStatus;

        pub fn AudioDeviceStart(inDevice: AudioObjectID, inProcID: AudioDeviceIOProcID) -> OSStatus;
        pub fn AudioDeviceStop(inDevice: AudioObjectID, inProcID: AudioDeviceIOProcID) -> OSStatus;
    }
}

struct IoCtx {
    ring: SpscRing<i32>,
    paused: AtomicBool,
    channels: usize,
}

extern "C" fn io_proc(
    _dev: sys::AudioObjectID,
    _now: *const c_void,
    _in_data: *const sys::AudioBufferList,
    _in_time: *const c_void,
    out_data: *mut sys::AudioBufferList,
    _out_time: *const c_void,
    client_data: *mut c_void,
) -> sys::OSStatus {
    let ctx = unsafe { &*(client_data as *const IoCtx) };
    let buf = unsafe { &mut (*out_data).mBuffers[0] };
    let dst = unsafe {
        std::slice::from_raw_parts_mut(buf.mData as *mut f32, buf.mDataByteSize as usize / 4)
    };
    if ctx.paused.load(Relaxed) {
        dst.fill(0.0);
        return 0;
    }
    let mut tmp = [0i32; 2048];
    let mut pos = 0;
    while pos < dst.len() {
        let want = (dst.len() - pos).min(tmp.len());
        let got = ctx.ring.pop_slice(&mut tmp[..want]);
        if got == 0 {
            dst[pos..].fill(0.0);
            break;
        }
        for i in 0..got {
            dst[pos + i] = tmp[i] as f32 / 2_147_483_648.0;
        }
        pos += got;
    }
    let _ = ctx.channels;
    0
}

pub struct CoreAudioBackend {
    device_id: sys::AudioObjectID,
    proc_id: sys::AudioDeviceIOProcID,
    ctx: Arc<IoCtx>,
    info: OpenInfo,
}

unsafe impl Send for CoreAudioBackend {}

impl CoreAudioBackend {
    pub fn open(req: &OpenReq) -> Result<Self, ()> {
        let dev_id = resolve_device_id(&req.device_id).ok_or_else(|| {
            log_msg("ERROR", format!("CoreAudio device '{}' not found", req.device_id));
        })?;

        // Claim Hog Mode when using an explicit hardware device.
        if req.device_id != "default" {
            let pid = std::process::id() as i32;
            let addr = sys::AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyHogMode,
                mScope: sys::kAudioObjectPropertyScopeGlobal,
                mElement: sys::kAudioObjectPropertyElementMain,
            };
            unsafe {
                let _ = sys::AudioObjectSetPropertyData(
                    dev_id,
                    &addr,
                    0,
                    std::ptr::null(),
                    std::mem::size_of::<i32>() as u32,
                    &pid as *const _ as *const c_void,
                );
            }
        }

        // Switch DAC nominal sample rate to match the track.
        let rate_f64 = req.sample_rate as f64;
        let rate_addr = sys::AudioObjectPropertyAddress {
            mSelector: sys::kAudioDevicePropertyNominalSampleRate,
            mScope: sys::kAudioObjectPropertyScopeGlobal,
            mElement: sys::kAudioObjectPropertyElementMain,
        };
        unsafe {
            let _ = sys::AudioObjectSetPropertyData(
                dev_id,
                &rate_addr,
                0,
                std::ptr::null(),
                std::mem::size_of::<f64>() as u32,
                &rate_f64 as *const _ as *const c_void,
            );
        }

        let ctx = Arc::new(IoCtx {
            ring: SpscRing::new(131_072),
            paused: AtomicBool::new(false),
            channels: req.channels as usize,
        });
        let mut proc_id: sys::AudioDeviceIOProcID = std::ptr::null_mut();
        let status = unsafe {
            sys::AudioDeviceCreateIOProcID(
                dev_id,
                io_proc,
                Arc::as_ptr(&ctx) as *mut c_void,
                &mut proc_id,
            )
        };
        if status != 0 || proc_id.is_null() {
            log_msg("ERROR", format!("CoreAudio AudioDeviceCreateIOProcID failed ({status})"));
            return Err(());
        }
        unsafe {
            sys::AudioDeviceStart(dev_id, proc_id);
        }

        let info = OpenInfo {
            device_id: req.device_id.clone(),
            hw_rate: req.sample_rate,
            channels: req.channels,
            wide: req.bits >= 24,
            out_fmt: OutFmt::S32,
            lossless: true,
            fmt: OutFmt::S32.label(req.bits, true),
            buffer: 4096,
            period: 512,
        };

        Ok(Self {
            device_id: dev_id,
            proc_id,
            ctx,
            info,
        })
    }

    fn push_i32(&self, samples: &[i32]) -> Result<(), WriteErr> {
        let mut off = 0;
        while off < samples.len() {
            let n = self.ctx.ring.push_slice(&samples[off..]);
            if n == 0 {
                thread::sleep(Duration::from_millis(2));
            } else {
                off += n;
            }
        }
        Ok(())
    }
}

impl AudioBackend for CoreAudioBackend {
    fn info(&self) -> &OpenInfo {
        &self.info
    }

    fn write_i16(&mut self, data: &[i16], _frames: usize) -> Result<(), WriteErr> {
        let scaled: Vec<i32> = data.iter().map(|&s| (s as i32) << 16).collect();
        self.push_i32(&scaled)
    }

    fn write_i32(&mut self, data: &[i32], _frames: usize) -> Result<(), WriteErr> {
        self.push_i32(data)
    }

    fn write_bytes(&mut self, _data: &[u8], _frames: usize, _bytes_per_frame: usize) -> Result<(), WriteErr> {
        Err(WriteErr::Fatal)
    }

    fn delay_frames(&self) -> u64 {
        (self.ctx.ring.len() / self.info.channels.max(1) as usize) as u64
    }

    fn pause(&mut self, paused: bool) {
        self.ctx.paused.store(paused, Relaxed);
    }

    fn drain(&mut self) {
        for _ in 0..200 {
            if self.ctx.ring.len() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn flush_and_prepare(&mut self) {
        self.ctx.ring.quiesce();
        self.ctx.ring.consumer_discard_all();
        self.ctx.ring.reset();
    }

    fn close(&mut self) {
        if !self.proc_id.is_null() {
            unsafe {
                sys::AudioDeviceStop(self.device_id, self.proc_id);
                sys::AudioDeviceDestroyIOProcID(self.device_id, self.proc_id);
            }
            self.proc_id = std::ptr::null_mut();
        }
    }
}

impl Drop for CoreAudioBackend {
    fn drop(&mut self) {
        self.close();
    }
}

fn resolve_device_id(id: &str) -> Option<sys::AudioObjectID> {
    if id == "default" {
        let mut dev: sys::AudioObjectID = 0;
        let mut size = std::mem::size_of::<sys::AudioObjectID>() as u32;
        let addr = sys::AudioObjectPropertyAddress {
            mSelector: sys::kAudioHardwarePropertyDefaultOutputDevice,
            mScope: sys::kAudioObjectPropertyScopeGlobal,
            mElement: sys::kAudioObjectPropertyElementMain,
        };
        let status = unsafe {
            sys::AudioObjectGetPropertyData(
                sys::kAudioObjectSystemObject,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                &mut dev as *mut _ as *mut c_void,
            )
        };
        return (status == 0 && dev != 0).then_some(dev);
    }
    id.strip_prefix("coreaudio:")?.parse::<u32>().ok()
}

pub fn probe_device(device_id: &str) -> ProbeResult {
    if resolve_device_id(device_id).is_some() {
        ProbeResult::Ready
    } else {
        ProbeResult::Missing
    }
}

pub fn list_devices() -> Vec<AudioDevice> {
    let mut devices = Vec::new();
    let addr = sys::AudioObjectPropertyAddress {
        mSelector: sys::kAudioHardwarePropertyDevices,
        mScope: sys::kAudioObjectPropertyScopeGlobal,
        mElement: sys::kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = 0;
    if unsafe {
        sys::AudioObjectGetPropertyDataSize(
            sys::kAudioObjectSystemObject,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
        )
    } == 0
        && size > 0
    {
        let count = size as usize / std::mem::size_of::<sys::AudioObjectID>();
        let mut ids = vec![0u32; count];
        if unsafe {
            sys::AudioObjectGetPropertyData(
                sys::kAudioObjectSystemObject,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                ids.as_mut_ptr() as *mut c_void,
            )
        } == 0
        {
            for id in ids {
                devices.push(AudioDevice {
                    id: format!("coreaudio:{id}"),
                    name: format!("CoreAudio Device #{id} (Hog Mode)"),
                    description: "macOS CoreAudio HAL direct output with sample rate switching".into(),
                    is_hardware: true,
                    is_usb_dac: true,
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
    devices.push(AudioDevice {
        id: "default".into(),
        name: "System Default (CoreAudio)".into(),
        description: "macOS default output device".into(),
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
