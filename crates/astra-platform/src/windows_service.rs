//! Owner-scoped Windows privilege broker. Registration requires UAC; subsequent
//! launches use a SYSTEM-authenticated pipe and fixed, protected installed code.
use crate::elevation::{ElevatedChild, Handle};
#[cfg(test)]
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        LocalFree, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
        ERROR_SERVICE_NOT_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
        DACL_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{DELETE, READ_CONTROL, WRITE_DAC},
    System::{
        Com::CoTaskMemFree,
        Pipes::GetNamedPipeServerProcessId,
        Services::*,
        Threading::{GetExitCodeProcess, GetProcessId, WaitForSingleObject},
    },
    UI::Shell::{
        FOLDERID_ProgramFiles, SHGetKnownFolderPath, ShellExecuteExW, KF_FLAG_DONT_VERIFY,
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    },
};

mod client_io;
mod host;
mod security;

const ASSETS: [&str; 3] = ["astra-service.exe", "astra-proxy.exe", "wintun.dll"];
const MAX_FRAME: usize = 64 * 1024;
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);
const OWNER_SERVICE_ACCESS: u32 = READ_CONTROL
    | SERVICE_QUERY_CONFIG
    | SERVICE_QUERY_STATUS
    | SERVICE_START
    | SERVICE_INTERROGATE;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SpawnRequest {
    pub args: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SpawnResponse {
    pub handle: Option<u64>,
    pub error: Option<String>,
}

#[cfg(test)]
pub(super) fn read_frame<T: DeserializeOwned>(stream: &mut impl Read) -> io::Result<T> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid service frame length",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
#[cfg(test)]
pub(super) fn write_frame<T: Serialize>(
    stream: &mut impl std::io::Write,
    value: &T,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service request exceeds 64 KiB",
        ));
    }
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in service argument",
        ));
    }
    value.push(0);
    Ok(value)
}
fn quote(value: &OsStr) -> io::Result<Vec<u16>> {
    let value = wide(value)?;
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for unit in &value[..value.len() - 1] {
        if *unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            b'\\' as u16,
            if *unit == b'"' as u16 {
                2 * slashes + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        result.push(*unit);
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    Ok(result)
}
fn command_line(values: &[&OsStr]) -> io::Result<Vec<u16>> {
    let mut result = Vec::new();
    for value in values {
        if !result.is_empty() {
            result.push(b' ' as u16);
        }
        result.extend(quote(value)?);
    }
    if result.len() >= 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service command is too long",
        ));
    }
    result.push(0);
    Ok(result)
}
fn validate_sid_shape(owner: &str) -> io::Result<()> {
    if !(owner.starts_with("S-1-5-21-") || owner.starts_with("S-1-12-1-"))
        || owner.len() > 184
        || owner
            .split('-')
            .skip(1)
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service requires a concrete Windows account SID",
        ));
    }
    Ok(())
}
fn shell_path(path: &Path) -> PathBuf {
    let value: Vec<_> = path.as_os_str().encode_wide().collect();
    let prefix: Vec<_> = r"\\?\".encode_utf16().collect();
    let unc: Vec<_> = r"UNC\".encode_utf16().collect();
    if let Some(tail) = value.strip_prefix(&prefix[..]) {
        if let Some(tail) = tail.strip_prefix(&unc[..]) {
            return PathBuf::from(OsString::from_wide(&[&[92, 92], tail].concat()));
        }
        if tail.get(1) == Some(&(b':' as u16)) {
            return PathBuf::from(OsString::from_wide(tail));
        }
    }
    path.into()
}
pub(super) fn service_name(owner: &str) -> String {
    format!("AstraPrivilege-{owner}")
}
pub(super) fn pipe_name(owner: &str) -> String {
    format!(r"\\.\pipe\AstraPrivilege-{owner}")
}
pub(super) fn installation_dir(owner: &str) -> io::Result<PathBuf> {
    validate_sid_shape(owner)?;
    let mut pointer = null_mut();
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramFiles,
            KF_FLAG_DONT_VERIFY as u32,
            null_mut(),
            &mut pointer,
        )
    };
    let result = if status < 0 || pointer.is_null() {
        Err(io::Error::other(format!(
            "Cannot locate Program Files (HRESULT 0x{:08X})",
            status as u32
        )))
    } else {
        let mut length = 0;
        unsafe {
            while *pointer.add(length) != 0 {
                length += 1;
            }
            Ok(
                PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(
                    pointer, length,
                )))
                .join("Astra Privilege Service")
                .join(owner),
            )
        }
    };
    unsafe { CoTaskMemFree(pointer.cast()) };
    result
}

struct ServiceHandle(SC_HANDLE);
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
fn manager(access: u32) -> io::Result<ServiceHandle> {
    let handle = unsafe { OpenSCManagerW(null(), null(), access) };
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(ServiceHandle(handle))
    }
}
fn service(owner: &str, access: u32) -> io::Result<Option<ServiceHandle>> {
    validate_sid_shape(owner)?;
    let manager = manager(SC_MANAGER_CONNECT)?;
    let name = wide(OsStr::new(&service_name(owner)))?;
    let handle = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if !handle.is_null() {
        return Ok(Some(ServiceHandle(handle)));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
        Ok(None)
    } else {
        Err(error)
    }
}
fn status(service: &ServiceHandle) -> io::Result<SERVICE_STATUS_PROCESS> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of_val(&status) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(status)
}
fn wait_state(service: &ServiceHandle, desired: u32) -> io::Result<SERVICE_STATUS_PROCESS> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let state = status(service)?;
        if state.dwCurrentState == desired {
            return Ok(state);
        }
        if desired == SERVICE_RUNNING && state.dwCurrentState == SERVICE_STOPPED {
            return Err(io::Error::other(format!(
                "Windows service stopped during startup (code {})",
                state.dwWin32ExitCode
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Windows service did not change state in time",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn start(service: &ServiceHandle) -> io::Result<SERVICE_STATUS_PROCESS> {
    if status(service)?.dwCurrentState == SERVICE_STOPPED
        && unsafe { StartServiceW(service.0, 0, null()) } == 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
            return Err(error);
        }
    }
    wait_state(service, SERVICE_RUNNING)
}

/// Read-only: never installs or starts the broker.
pub fn installed() -> io::Result<bool> {
    Ok(service(&security::current_user_sid()?, SERVICE_QUERY_STATUS)?.is_some())
}

/// Explicit registration/removal opens a single UAC prompt. It never falls
/// back to another account: the elevated installer rechecks the invoking SID.
pub fn set_installed(source_bin: &Path, enable: bool) -> io::Result<()> {
    let owner = security::current_user_sid()?;
    validate_sid_shape(&owner)?;
    if installed()? == enable {
        return Ok(());
    }
    let helper = if enable {
        for asset in ASSETS {
            if !source_bin.join(asset).is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Windows service requires a complete client package: missing {asset}"),
                ));
            }
        }
        std::fs::canonicalize(source_bin.join("astra-service.exe"))?
    } else {
        security::validate_protected(&installation_dir(&owner)?.join("astra-service.exe"))?
    };
    let helper = shell_path(&helper);
    let operation = if enable { "--install" } else { "--uninstall" };
    let arguments = command_line(&[
        OsStr::new(operation),
        OsStr::new("--owner"),
        OsStr::new(&owner),
    ])?;
    let executable = wide(helper.as_os_str())?;
    let directory = wide(
        helper
            .parent()
            .ok_or_else(|| io::Error::other("service helper has no directory"))?
            .as_os_str(),
    )?;
    let verb = wide(OsStr::new("runas"))?;
    let mut info = SHELLEXECUTEINFOW::default();
    info.cbSize = std::mem::size_of_val(&info) as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = executable.as_ptr();
    info.lpParameters = arguments.as_ptr();
    info.lpDirectory = directory.as_ptr();
    info.nShow = 0;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other(
            "Windows returned no service installer process",
        ));
    }
    let process = Handle(info.hProcess);
    match unsafe { WaitForSingleObject(process.0, 120_000) } {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Windows service installation is still running; check its status before retrying",
            ))
        }
        _ => return Err(io::Error::last_os_error()),
    }
    let mut exit = 0;
    if unsafe { GetExitCodeProcess(process.0, &mut exit) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if exit != 0 {
        return Err(io::Error::other(format!("Windows service operation failed (exit code {exit}); registration requires the same administrator account and a complete client package")));
    }
    if installed()? != enable {
        return Err(io::Error::other(
            "Windows service registration did not reach the requested state",
        ));
    }
    Ok(())
}

fn service_dacl(owner: &str) -> io::Result<Vec<u16>> {
    validate_sid_shape(owner)?;
    // Owner: query config/status + start + interrogate + read security. No stop,
    // delete, change-config or ACL-write rights to the SYSTEM broker itself.
    wide(OsStr::new(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x{OWNER_SERVICE_ACCESS:08x};;;{owner})"
    )))
}
fn install(owner: &str) -> io::Result<()> {
    security::validate_owner_sid(owner)?;
    if service(owner, SERVICE_QUERY_STATUS)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Windows service already installed; unregister before replacing its snapshot",
        ));
    }
    let source = std::env::current_exe()?;
    let source = source
        .parent()
        .ok_or_else(|| io::Error::other("service helper has no directory"))?;
    let directory = installation_dir(owner)?;
    security::prepare_install_directory(&directory)?;
    for asset in ASSETS {
        security::copy_protected(&source.join(asset), &directory.join(asset))?;
    }
    let executable = security::validate_protected(&directory.join("astra-service.exe"))?;
    let name = wide(OsStr::new(&service_name(owner)))?;
    let display = wide(OsStr::new(&format!("Astra Privilege Service ({owner})")))?;
    let command = command_line(&[
        executable.as_os_str(),
        OsStr::new("--service"),
        OsStr::new("--owner"),
        OsStr::new(owner),
    ])?;
    let manager = manager(SC_MANAGER_CREATE_SERVICE)?;
    let handle = unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP | DELETE | WRITE_DAC,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            command.as_ptr(),
            null(),
            null_mut(),
            null(),
            null(),
            null(),
        )
    };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let service = ServiceHandle(handle);
    let result = (|| {
        let dacl = service_dacl(owner)?;
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                dacl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let ok =
            unsafe { SetServiceObjectSecurity(service.0, DACL_SECURITY_INFORMATION, descriptor) };
        let error = io::Error::last_os_error();
        unsafe { LocalFree(descriptor) };
        if ok == 0 {
            return Err(error);
        }
        start(&service)?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            let mut state = SERVICE_STATUS::default();
            ControlService(service.0, SERVICE_CONTROL_STOP, &mut state);
            DeleteService(service.0);
        }
    }
    result
}
fn uninstall(owner: &str) -> io::Result<()> {
    security::validate_administrator_sid(owner)?;
    let Some(service) = service(owner, SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE)? else {
        return Ok(());
    };
    if status(&service)?.dwCurrentState != SERVICE_STOPPED {
        let mut stopped = SERVICE_STATUS::default();
        if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut stopped) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_SERVICE_NOT_ACTIVE as i32) {
                return Err(error);
            }
        }
        wait_state(&service, SERVICE_STOPPED)?;
    }
    if unsafe { DeleteService(service.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Keep protected snapshot files: this helper may itself be running there.
    // A subsequent authorized installation atomically replaces the snapshot.
    Ok(())
}

/// Entrypoint for astra-service.exe; service mode only runs under SCM/SYSTEM.
pub fn run_cli() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 || args[1] != "--owner" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected --install|--uninstall|--service --owner SID",
        ));
    }
    validate_sid_shape(&args[2])?;
    match args[0].as_str() {
        "--install" => install(&args[2]),
        "--uninstall" => uninstall(&args[2]),
        "--service" => host::run(&args[2]),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown Windows service operation",
        )),
    }
}

fn same_contents(left: &Path, right: &Path) -> io::Result<bool> {
    let mut left = File::open(left)?;
    let mut right = File::open(right)?;
    if left.metadata()?.len() != right.metadata()?.len() {
        return Ok(false);
    }
    let mut a = [0; 64 * 1024];
    let mut b = [0; 64 * 1024];
    loop {
        let length = left.read(&mut a)?;
        if length == 0 {
            return Ok(right.read(&mut b)? == 0);
        }
        right.read_exact(&mut b[..length])?;
        if a[..length] != b[..length] {
            return Ok(false);
        }
    }
}

/// Launch via an installed broker without showing UAC. The server never sees a
/// caller-supplied executable: it can launch only its protected proxy snapshot.
pub fn spawn(exe: &Path, args: &[OsString]) -> io::Result<ElevatedChild> {
    if !exe.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .eq_ignore_ascii_case("astra-proxy.exe")
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows service only launches astra-proxy.exe",
        ));
    }
    let owner = security::current_user_sid()?;
    let directory = installation_dir(&owner)?;
    let proxy = security::pin_protected(&directory.join("astra-proxy.exe"))?;
    if !same_contents(exe, proxy.path())? {
        return Err(io::Error::other("Installed Windows service uses a different proxy version; unregister and register it again after updating"));
    }
    let service = service(&owner, SERVICE_QUERY_STATUS | SERVICE_START)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Windows privilege service is not registered",
        )
    })?;
    let state = start(&service)?;
    let _process =
        security::verify_service_process(state.dwProcessId, &directory.join("astra-service.exe"))?;
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    let pipe = loop {
        match client_io::Pipe::open(OsStr::new(&pipe_name(&owner))) {
            Ok(pipe) => break pipe,
            Err(error)
                if matches!(error.raw_os_error(), Some(2 | 231)) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    };
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if pid != state.dwProcessId || status(&service)?.dwProcessId != pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows service pipe is not owned by the registered broker",
        ));
    }
    let request = SpawnRequest {
        args: args
            .iter()
            .map(|value| {
                value.to_str().map(str::to_owned).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "service arguments must be Unicode",
                    )
                })
            })
            .collect::<io::Result<_>>()?,
    };
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    pipe.send(&request, deadline)?;
    let response: SpawnResponse = pipe.receive(deadline)?;
    match (response.handle, response.error) {
        (Some(handle), None) if handle != 0 && handle <= usize::MAX as u64 => {
            // The authenticated broker duplicated this limited process handle
            // directly into this process; it is already ours, not a server PID.
            let handle = Handle(handle as usize as _);
            if unsafe { GetProcessId(handle.0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let child = unsafe { ElevatedChild::from_owned_handle(handle.0) };
            std::mem::forget(handle);
            // Adopt the suspended candidate before acknowledging it, so any
            // timeout/disconnect after this point kills and closes our child.
            pipe.write_all(&[1], deadline)?;
            let mut ready = [0];
            pipe.read_exact(&mut ready, deadline)?;
            if ready != [1] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows service did not confirm child startup",
                ));
            }
            Ok(child)
        }
        (None, Some(message)) => Err(io::Error::other(message)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Windows service response",
        )),
    }
}

#[cfg(test)]
#[path = "test/windows_service_test.rs"]
mod tests;
