//! Real-time thread priority promotion for the audio writer thread.
//!
//! - Linux: sets `RLIMIT_RTTIME` as required by RTKit, then requests `SCHED_FIFO` / `SCHED_RR`
//!   via `org.freedesktop.RealtimeKit1` over system D-Bus (with nice-level fallback).
//! - Windows: registers the thread with MMCSS (`AvSetMmThreadCharacteristicsW("Pro Audio", ...)`).
//!
//! Never fails playback if real-time promotion is unavailable (e.g. inside containers or VMs).

#[cfg(target_os = "linux")]
pub fn promote_audio_thread() {
    use crate::logger::log_msg;
    let Some(tid) = linux_tid() else {
        log_msg("WARN", "RTKit: could not determine kernel thread ID");
        return;
    };
    let Ok(conn) = zbus::blocking::Connection::system() else {
        log_msg("WARN", "RTKit: system D-Bus unavailable; writer running at normal priority");
        return;
    };

    let rt_proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.RealtimeKit1",
        "/org/freedesktop/RealtimeKit1",
        "org.freedesktop.RealtimeKit1",
    );

    // RTKit strictly requires RLIMIT_RTTIME to be set (not RLIM_INFINITY) before MakeThreadRealtime.
    let rttime_max: u64 = rt_proxy
        .as_ref()
        .ok()
        .and_then(|p| p.get_property::<i64>("RTTimeUSecMax").ok())
        .filter(|&v| v > 0)
        .unwrap_or(200_000) as u64;

    let rlim = libc::rlimit {
        rlim_cur: rttime_max,
        rlim_max: rttime_max,
    };
    let rlim_res = unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &rlim) };
    if rlim_res != 0 {
        log_msg("WARN", "RTKit: failed to set RLIMIT_RTTIME; real-time promotion may be denied");
    }

    let max_prio: u32 = rt_proxy
        .as_ref()
        .ok()
        .and_then(|p| p.get_property::<i32>("MaxRealtimePriority").ok())
        .filter(|&v| v > 0)
        .unwrap_or(20) as u32;
    let prio = max_prio.min(20);

    let res = conn.call_method(
        Some("org.freedesktop.RealtimeKit1"),
        "/org/freedesktop/RealtimeKit1",
        Some("org.freedesktop.RealtimeKit1"),
        "MakeThreadRealtime",
        &(tid, prio),
    );
    match res {
        Ok(_) => {
            log_msg("INFO", format!("RTKit: promoted audio writer thread (tid {tid}, prio {prio}) to real-time"));
            return;
        }
        Err(e) => {
            // Fallback to high-priority nice level via RTKit if SCHED_FIFO is denied by policy.
            let min_nice: i32 = rt_proxy
                .as_ref()
                .ok()
                .and_then(|p| p.get_property::<i32>("MinNiceLevel").ok())
                .unwrap_or(-10)
                .max(-15);
            if conn
                .call_method(
                    Some("org.freedesktop.RealtimeKit1"),
                    "/org/freedesktop/RealtimeKit1",
                    Some("org.freedesktop.RealtimeKit1"),
                    "MakeThreadHighPriority",
                    &(tid, min_nice),
                )
                .is_ok()
            {
                log_msg("INFO", format!("RTKit: promoted audio writer thread (tid {tid}, nice {min_nice})"));
                return;
            }
            log_msg("WARN", format!("RTKit: real-time promotion skipped ({e}); writer running at normal priority"));
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_tid() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
    parse_stat_tid(&stat)
}

#[cfg(any(target_os = "linux", test))]
fn parse_stat_tid(stat: &str) -> Option<u64> {
    stat.split_whitespace().next()?.parse::<u64>().ok()
}

#[cfg(target_os = "windows")]
pub fn promote_audio_thread() {
    use crate::logger::log_msg;
    #[link(name = "avrt")]
    extern "system" {
        fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
    }
    let name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut task_index: u32 = 0;
    let handle = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut task_index) };
    if handle != 0 {
        log_msg("INFO", "MMCSS: promoted audio writer thread to 'Pro Audio' priority");
    } else {
        log_msg("WARN", "MMCSS: could not promote audio writer thread");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn promote_audio_thread() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kernel_tid_from_proc_stat() {
        assert_eq!(parse_stat_tid("12345 (rex-writer) R 1 12345 ..."), Some(12345));
        assert_eq!(parse_stat_tid(""), None);
    }
}
