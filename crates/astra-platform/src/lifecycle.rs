//! Stable OS process identities. Observation errors are never evidence of death.
pub use astra_model::control::{Identity, ProcessIdentity};
use std::io;

pub fn boot_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}
pub fn identity(applied: Option<astra_model::control::Applied>) -> io::Result<Identity> {
    Ok(Identity {
        version: 1,
        boot_id: boot_id().into(),
        process: capture(std::process::id())?,
        applied,
    })
}
pub fn matches_live(identity: &ProcessIdentity) -> io::Result<bool> {
    match capture(identity.pid) {
        Ok(actual) => Ok(actual == *identity),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Startup lease: only a durable readiness commit releases the candidate.
pub struct Candidate {
    owner: Option<ProcessIdentity>,
    committed: std::sync::atomic::AtomicBool,
}
impl Candidate {
    pub fn from_env() -> io::Result<Self> {
        let owner = std::env::var("ASTRA_SERVICE_OWNER")
            .ok()
            .map(|text| serde_json::from_str::<ProcessIdentity>(&text).map_err(io::Error::other))
            .transpose()?;
        if let Some(owner) = &owner {
            if !matches_live(owner)? {
                return Err(io::Error::other("startup owner exited before readiness"));
            }
        }
        Ok(Self {
            owner,
            committed: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub fn commit(&self) -> io::Result<()> {
        if self.committed.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        if let Some(owner) = &self.owner {
            if !matches_live(owner)? {
                return Err(io::Error::other("startup owner exited before commit"));
            }
        }
        self.committed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub async fn wait_owner_loss(&self) -> io::Error {
        loop {
            if self.committed.load(std::sync::atomic::Ordering::Acquire) || self.owner.is_none() {
                std::future::pending::<()>().await;
            }
            match matches_live(self.owner.as_ref().unwrap()) {
                Ok(true) => {}
                Ok(false) => {
                    return io::Error::other("startup owner exited before service commitment")
                }
                Err(error) => {
                    return io::Error::other(format!("startup owner observation failed: {error}"))
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}
#[cfg(target_os = "linux")]
pub fn capture(pid: u32) -> io::Result<ProcessIdentity> {
    // A process exiting between open and read makes the read fail with ESRCH;
    // that is the same "gone" answer as a missing /proc entry.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|error| {
        if error.raw_os_error() == Some(libc::ESRCH) {
            io::ErrorKind::NotFound.into()
        } else {
            error
        }
    })?;
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("invalid process stat"))?
        .1
        .split_whitespace()
        .collect();
    if fields.first() == Some(&"Z") {
        return Err(io::ErrorKind::NotFound.into());
    }
    let ticks = fields
        .get(19)
        .ok_or_else(|| io::Error::other("missing process start time"))?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(ProcessIdentity {
        pid,
        started: format!("{}:{ticks}", boot.trim()),
    })
}
#[cfg(target_os = "macos")]
pub fn capture(pid: u32) -> io::Result<ProcessIdentity> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info);
    let count = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as i32,
        )
    };
    if count != size as i32 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ESRCH) {
            io::ErrorKind::NotFound.into()
        } else {
            error
        });
    }
    if info.pbi_status == libc::SZOMB {
        return Err(io::ErrorKind::NotFound.into());
    }
    Ok(ProcessIdentity {
        pid,
        started: format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
    })
}
#[cfg(windows)]
pub fn capture(pid: u32) -> io::Result<ProcessIdentity> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, WAIT_OBJECT_0},
        System::Threading::*,
    };
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(87) {
            io::ErrorKind::NotFound.into()
        } else {
            error
        });
    }
    let [mut created, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
    let alive = unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0;
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) };
    let error = io::Error::last_os_error();
    unsafe {
        CloseHandle(handle);
    }
    if !alive {
        return Err(io::ErrorKind::NotFound.into());
    }
    if ok == 0 {
        return Err(error);
    }
    Ok(ProcessIdentity {
        pid,
        started: format!(
            "{}",
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_process_identity_is_stable_and_rejects_reused_pid() {
        let mut current = capture(std::process::id()).unwrap();
        assert!(matches_live(&current).unwrap());
        assert_eq!(
            identity(None).unwrap().boot_id,
            identity(None).unwrap().boot_id
        );
        current.started.push('x');
        assert!(!matches_live(&current).unwrap());
    }
    #[tokio::test]
    async fn candidate_owner_loss_is_fatal_until_commit() {
        let mut dead = capture(std::process::id()).unwrap();
        dead.started.push('x');
        let candidate = Candidate {
            owner: Some(dead),
            committed: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(candidate.commit().is_err());
        assert!(candidate
            .wait_owner_loss()
            .await
            .to_string()
            .contains("before service commitment"));
        let candidate = Candidate {
            owner: Some(capture(std::process::id()).unwrap()),
            committed: std::sync::atomic::AtomicBool::new(false),
        };
        candidate.commit().unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            candidate.wait_owner_loss()
        )
        .await
        .is_err());
    }
}
