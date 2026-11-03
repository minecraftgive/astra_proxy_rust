//! On-demand process observations. CPU is 100% per logical core; children are excluded.
use crate::lifecycle::{self, ProcessIdentity};
use std::{
    collections::HashMap,
    io,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct ProcessSample {
    pub identity: ProcessIdentity,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: u64,
    pub virtual_memory_bytes: Option<u64>,
}
struct Previous {
    at: Instant,
    cpu_seconds: f64,
    sample: ProcessSample,
}
/// Bounded history, with no background thread or polling when nobody observes it.
#[derive(Default)]
pub struct Sampler {
    previous: HashMap<u32, Previous>,
}
impl Sampler {
    pub fn sample(&mut self, pid: u32) -> io::Result<ProcessSample> {
        let identity = lifecycle::capture(pid)?;
        let now = Instant::now();
        self.previous
            .retain(|_, p| now.duration_since(p.at) < Duration::from_secs(30));
        if let Some(previous) = self.previous.get(&pid) {
            if previous.sample.identity == identity
                && now.duration_since(previous.at) < Duration::from_millis(250)
            {
                return Ok(previous.sample.clone());
            }
        }
        let raw = observe(pid)?;
        if lifecycle::capture(pid)? != identity {
            self.previous.remove(&pid);
            return Err(io::Error::other(
                "Process identity changed during monitor sample",
            ));
        }
        let sample = ProcessSample {
            cpu_percent: self.previous.get(&pid).and_then(|p| {
                cpu_delta(
                    &p.sample.identity,
                    &identity,
                    p.cpu_seconds,
                    raw.cpu_seconds,
                    now.duration_since(p.at),
                )
            }),
            identity,
            memory_bytes: raw.memory_bytes,
            virtual_memory_bytes: raw.virtual_memory_bytes,
        };
        if self.previous.len() >= 256 && !self.previous.contains_key(&pid) {
            if let Some(oldest) = self
                .previous
                .iter()
                .min_by_key(|(_, p)| p.at)
                .map(|(pid, _)| *pid)
            {
                self.previous.remove(&oldest);
            }
        }
        self.previous.insert(
            pid,
            Previous {
                at: now,
                cpu_seconds: raw.cpu_seconds,
                sample: sample.clone(),
            },
        );
        Ok(sample)
    }
}
fn cpu_delta(
    before: &ProcessIdentity,
    after: &ProcessIdentity,
    old: f64,
    new: f64,
    elapsed: Duration,
) -> Option<f64> {
    if before != after || elapsed.is_zero() || new < old {
        return None;
    }
    let value = (new - old) / elapsed.as_secs_f64() * 100.0;
    value.is_finite().then_some(value)
}
struct Raw {
    cpu_seconds: f64,
    memory_bytes: u64,
    virtual_memory_bytes: Option<u64>,
}

#[cfg(target_os = "linux")]
fn observe(pid: u32) -> io::Result<Raw> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ticks <= 0 || page <= 0 {
        return Err(io::Error::other("Invalid OS process units"));
    }
    parse_linux(&text, ticks as u64, page as u64)
}
#[cfg(any(target_os = "linux", test))]
fn parse_linux(text: &str, ticks: u64, page: u64) -> io::Result<Raw> {
    let fields: Vec<_> = text
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("Invalid process stat"))?
        .1
        .split_whitespace()
        .collect();
    let number = |index: usize| -> io::Result<u64> {
        fields
            .get(index)
            .ok_or_else(|| io::Error::other("Incomplete process stat"))?
            .parse()
            .map_err(io::Error::other)
    };
    if ticks == 0 || page == 0 {
        return Err(io::Error::other("Invalid process units"));
    }
    Ok(Raw {
        cpu_seconds: (number(11)? as f64 + number(12)? as f64) / ticks as f64,
        memory_bytes: number(21)?
            .checked_mul(page)
            .ok_or_else(|| io::Error::other("RSS overflow"))?,
        virtual_memory_bytes: Some(number(20)?),
    })
}

#[cfg(target_os = "macos")]
fn observe(pid: u32) -> io::Result<Raw> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info);
    let count = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size as i32,
        )
    };
    if count != size as i32 {
        return Err(io::Error::last_os_error());
    }
    // XNU fill_taskprocinfo returns Mach ticks. hw.tbfrequency is the kernel's
    // timebase frequency, including when a translated process observes native
    // workers; the user-space mach_timebase_info may be translated by Rosetta.
    // https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/bsd_kern.c
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_mib.c
    let mut frequency: u64 = 0;
    let mut size = std::mem::size_of_val(&frequency);
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.tbfrequency".as_ptr(),
            (&mut frequency as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != std::mem::size_of::<u64>() || frequency == 0 {
        return Err(io::Error::other("Cannot read CPU timebase frequency"));
    }
    Ok(Raw {
        cpu_seconds: (info.pti_total_user as f64 + info.pti_total_system as f64) / frequency as f64,
        memory_bytes: info.pti_resident_size,
        virtual_memory_bytes: Some(info.pti_virtual_size),
    })
}

#[cfg(windows)]
fn observe(pid: u32) -> io::Result<Raw> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::*,
        },
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let [mut created, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
        if unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        memory.cb = std::mem::size_of_val(&memory) as u32;
        if unsafe {
            GetProcessMemoryInfo(
                handle,
                &mut memory,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let ticks =
            |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
        Ok(Raw {
            cpu_seconds: (ticks(kernel) as f64 + ticks(user) as f64) / 1e7,
            memory_bytes: memory.WorkingSetSize as u64,
            // PagefileUsage is commit charge, not virtual address-space size.
            virtual_memory_bytes: None,
        })
    })();
    unsafe {
        CloseHandle(handle);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn monitor_cpu_identity_and_monotonic_deltas() {
        let first = ProcessIdentity {
            pid: 1,
            started: "first".into(),
        };
        let next = ProcessIdentity {
            pid: 1,
            started: "next".into(),
        };
        assert_eq!(
            cpu_delta(&first, &first, 2.0, 5.0, Duration::from_secs(2)),
            Some(150.0)
        );
        assert_eq!(
            cpu_delta(&first, &next, 2.0, 5.0, Duration::from_secs(2)),
            None
        );
        assert_eq!(
            cpu_delta(&first, &first, 5.0, 2.0, Duration::from_secs(2)),
            None
        );
        assert_eq!(cpu_delta(&first, &first, 2.0, 5.0, Duration::ZERO), None);
    }
    #[test]
    fn monitor_linux_stat_handles_parentheses_and_units() {
        let mut fields = vec!["0"; 22];
        fields[0] = "R";
        fields[11] = "250";
        fields[12] = "50";
        fields[20] = "999999";
        fields[21] = "12";
        let text = format!("42 (a process ) with parentheses) {}", fields.join(" "));
        let raw = parse_linux(&text, 100, 4096).unwrap();
        assert_eq!(raw.cpu_seconds, 3.0);
        assert_eq!(raw.memory_bytes, 49152);
        assert_eq!(raw.virtual_memory_bytes, Some(999999));
        assert!(parse_linux("bad stat", 100, 4096).is_err());
    }
    #[test]
    fn monitor_current_process_is_real_and_first_cpu_is_unknown() {
        let mut sampler = Sampler::default();
        let first = sampler.sample(std::process::id()).unwrap();
        assert_eq!(first.identity.pid, std::process::id());
        assert_eq!(first.cpu_percent, None);
        assert!(first.memory_bytes > 0);
        std::thread::sleep(Duration::from_millis(270));
        let second = sampler.sample(std::process::id()).unwrap();
        assert!(second
            .cpu_percent
            .is_some_and(|n| n >= 0.0 && n.is_finite()));
    }
}
