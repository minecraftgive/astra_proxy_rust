//! Local executable identities for process routing. Run discovery on the target Agent.
pub mod owner;

use astra_model::ProcessInfo;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Snapshot only executable paths. Command lines and environments may contain credentials.
pub fn running_processes() -> Vec<ProcessInfo> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        ProcessRefreshKind::new().with_exe(UpdateKind::Always),
    );
    let mut programs = Vec::new();
    for (pid, process) in system.processes() {
        let Some(exe) = process.exe().filter(|path| path.is_absolute()) else {
            continue;
        };
        let (Some(path), Some(name)) = (exe.to_str(), exe.file_name().and_then(|s| s.to_str()))
        else {
            continue;
        };
        programs.push(ProcessInfo {
            pid: pid.as_u32(),
            path: path.to_owned(),
            name: name.to_owned(),
        });
    }
    programs.sort_unstable_by(|a, b| a.path.cmp(&b.path).then(a.pid.cmp(&b.pid)));
    programs
}

#[cfg(target_os = "linux")]
pub mod linux;

/// Bounded response for an interactive picker. The path and PID disambiguate
/// programs with the same display name. Filtering precedes the response limit.
pub fn search_processes(filter: &str) -> (Vec<ProcessInfo>, bool) {
    let query = filter.trim().to_lowercase();
    let mut programs = running_processes();
    programs.retain(|program| {
        program.path.len() <= 4096 && program.path.to_lowercase().contains(&query)
    });
    let truncated = programs.len() > 128;
    programs.truncate(128);
    (programs, truncated)
}

#[cfg(test)]
#[path = "test/discovery_test.rs"]
mod discovery_tests;

#[cfg(windows)]
pub mod windows;
