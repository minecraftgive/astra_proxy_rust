//! SCM hosts only a narrow, per-user privileged-core launcher. Configuration
//! content is never read by LocalSystem and no caller selects an executable.
use super::{pipe_name, service_name, SpawnRequest, SpawnResponse, MAX_FRAME};
use crate::elevation::Handle;
use std::{
    ffi::{c_void, OsStr},
    io,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::*,
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        *,
    },
    System::{Pipes::*, Services::*, SystemInformation::GetWindowsDirectoryW, Threading::*},
};

struct State {
    owner: String,
    status: AtomicUsize,
    stopping: AtomicBool,
    wake: tokio::sync::Notify,
    error: Mutex<Option<String>>,
}
static STATE: OnceLock<State> = OnceLock::new();

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
fn denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}
fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(invalid("NUL in Windows argument"));
    }
    value.push(0);
    Ok(value)
}

pub(super) fn run(owner: &str) -> io::Result<()> {
    super::security::validate_sid(owner)?;
    let token = process_token(unsafe { GetCurrentProcess() })?;
    if token_sid(token.0)? != "S-1-5-18" {
        return Err(denied(
            "The privilege broker must be started by SCM as LocalSystem",
        ));
    }
    super::security::allow_service_verification(owner)?;
    STATE
        .set(State {
            owner: owner.into(),
            status: AtomicUsize::new(0),
            stopping: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            error: Mutex::new(None),
        })
        .map_err(|_| invalid("Service dispatcher already initialized"))?;
    let mut name = wide(OsStr::new(&service_name(owner)))?;
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(error) = STATE.get().unwrap().error.lock().unwrap().take() {
        return Err(io::Error::other(error));
    }
    Ok(())
}

unsafe extern "system" fn service_main(_: u32, _: *mut *mut u16) {
    let state = STATE.get().unwrap();
    let result = (|| -> io::Result<()> {
        let name = wide(OsStr::new(&service_name(&state.owner)))?;
        let status = unsafe {
            RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), std::ptr::null())
        };
        if status.is_null() {
            return Err(io::Error::last_os_error());
        }
        state.status.store(status as usize, Ordering::Release);
        report(SERVICE_START_PENDING, 0);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(serve(&state.owner))
    })();
    let code = if let Err(error) = result {
        *state.error.lock().unwrap() = Some(error.to_string());
        ERROR_SERVICE_SPECIFIC_ERROR
    } else {
        0
    };
    report(SERVICE_STOPPED, code);
}
unsafe extern "system" fn control(code: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    if matches!(code, SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN) {
        let state = STATE.get().unwrap();
        state.stopping.store(true, Ordering::Release);
        report(SERVICE_STOP_PENDING, 0);
        state.wake.notify_one();
    }
    NO_ERROR
}
fn report(current: u32, exit: u32) {
    let state = STATE.get().unwrap();
    let handle = state.status.load(Ordering::Acquire) as SERVICE_STATUS_HANDLE;
    if handle.is_null() {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: current,
        dwControlsAccepted: if current == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: exit,
        dwServiceSpecificExitCode: u32::from(exit != 0),
        dwCheckPoint: u32::from(matches!(
            current,
            SERVICE_START_PENDING | SERVICE_STOP_PENDING
        )),
        dwWaitHint: if matches!(current, SERVICE_START_PENDING | SERVICE_STOP_PENDING) {
            10_000
        } else {
            0
        },
    };
    unsafe {
        SetServiceStatus(handle, &status);
    }
}

fn listener(owner: &str, first: bool) -> io::Result<NamedPipeServer> {
    super::security::validate_sid(owner)?;
    // Only the installed owner and SYSTEM can connect. Medium integrity is
    // intentional: the owner's ordinary manager requests a narrow operation.
    let sddl = wide(OsStr::new(&format!(
        "D:P(A;;GA;;;SY)(A;;0x00100083;;;{owner})S:(ML;;NW;;;ME)"
    )))?;
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .max_instances(16)
            .create_with_security_attributes_raw(
                pipe_name(owner),
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    };
    unsafe {
        LocalFree(descriptor);
    }
    result
}
async fn serve(owner: &str) -> io::Result<()> {
    // Pin the service installation before reporting readiness.
    let executable = std::env::current_exe()?;
    let installation = executable
        .parent()
        .ok_or_else(|| invalid("Missing service directory"))?
        .to_owned();
    let _service = super::security::pin_protected(&executable)?;
    let _proxy = super::security::pin_protected(&installation.join("astra-proxy.exe"))?;
    let _driver = super::security::pin_protected(&installation.join("wintun.dll"))?;
    let mut current = listener(owner, true)?;
    report(SERVICE_RUNNING, 0);
    let mut tasks = tokio::task::JoinSet::new();
    let state = STATE.get().unwrap();
    while !state.stopping.load(Ordering::Acquire) {
        tokio::select! {
            _ = state.wake.notified() => break,
            result = current.connect() => {
                result?;
                let next = listener(owner, false)?;
                let pipe = std::mem::replace(&mut current, next);
                if tasks.len() < 8 {
                    let owner = owner.to_owned();
                    let installation = installation.clone();
                    tasks.spawn(async move { handle(pipe, &owner, &installation).await });
                }
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    // Transferred cores belong to their requesting manager. Stop accepting new
    // work and cancel uncommitted requests; their guards terminate candidates.
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
async fn handle(mut pipe: NamedPipeServer, owner: &str, installation: &Path) {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let size = pipe.read_u32_le().await? as usize;
        if size > MAX_FRAME {
            return Err(invalid("Service request exceeds size limit"));
        }
        let mut bytes = vec![0; size];
        pipe.read_exact(&mut bytes).await?;
        let request: SpawnRequest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        let peer = authenticate(pipe.as_raw_handle(), owner)?;
        spawn(peer, request, owner, installation)
    })
    .await;
    match result {
        Ok(Ok(mut child)) => {
            let response = SpawnResponse {
                handle: Some(child.remote as u64),
                error: None,
            };
            if tokio::time::timeout(Duration::from_secs(5), respond(&mut pipe, &response))
                .await
                .is_ok_and(|r| r.is_ok())
            {
                // A complete response lets the client adopt this handle. Never
                // revoke its slot afterward: it may already have been closed
                // and reused while its ACK was interrupted. Process ownership
                // stays here until ACK, resume and the final READY all succeed.
                child.handle_delivered = true;
                if tokio::time::timeout(
                    Duration::from_secs(5),
                    acknowledge_and_resume(&mut pipe, || child.resume()),
                )
                .await
                .is_ok_and(|r| r.is_ok())
                {
                    child.transferred = true;
                }
            }
        }
        Ok(Err(error)) => {
            let response = SpawnResponse {
                handle: None,
                error: Some(error.to_string()),
            };
            let _ =
                tokio::time::timeout(Duration::from_secs(5), respond(&mut pipe, &response)).await;
        }
        Err(_) => {}
    }
}
async fn respond(pipe: &mut NamedPipeServer, response: &SpawnResponse) -> io::Result<()> {
    let bytes = serde_json::to_vec(response).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(invalid("Service reply exceeds size limit"));
    }
    pipe.write_u32_le(bytes.len() as u32).await?;
    pipe.write_all(&bytes).await?;
    pipe.flush().await
}

async fn acknowledge_and_resume(
    pipe: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    resume: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if pipe.read_u8().await? != 1 {
        return Err(invalid("Invalid service ownership acknowledgment"));
    }
    // The client now guards the process and will kill it if READY is lost.
    resume()?;
    pipe.write_u8(1).await?;
    pipe.flush().await
}

fn process_token(process: HANDLE) -> io::Result<Handle> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(token))
}
fn token_data(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut size = 0;
    unsafe {
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut size);
    }
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe { GetTokenInformation(token, class, bytes.as_mut_ptr().cast(), size, &mut size) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}
fn token_sid(token: HANDLE) -> io::Result<String> {
    let bytes = token_data(token, TokenUser)?;
    let user = unsafe { &*bytes.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    unsafe {
        while *sid.add(len) != 0 {
            len += 1;
        }
    }
    let result = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, len) });
    unsafe {
        LocalFree(sid.cast());
    }
    Ok(result)
}
fn token_session(token: HANDLE) -> io::Result<u32> {
    Ok(unsafe { *token_data(token, TokenSessionId)?.as_ptr().cast::<u32>() })
}
fn elevated(token: HANDLE) -> io::Result<bool> {
    Ok(unsafe {
        (*token_data(token, TokenElevation)?
            .as_ptr()
            .cast::<TOKEN_ELEVATION>())
        .TokenIsElevated
            != 0
    })
}
struct Peer {
    process: Handle,
    token: Handle,
    identity: crate::lifecycle::ProcessIdentity,
}
fn authenticate(pipe: HANDLE, owner: &str) -> io::Result<Peer> {
    let (mut pid, mut session) = (0, 0);
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) } == 0
        || pid == 0
        || unsafe { GetNamedPipeClientSessionId(pipe, &mut session) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Authenticate the token attached to the actual pipe exchange as well as
    // the pinned process. No await occurs while the thread impersonates it.
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pipe_token = std::ptr::null_mut();
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut pipe_token) };
    let error = io::Error::last_os_error();
    let reverted = unsafe { RevertToSelf() };
    if reverted == 0 {
        // Continuing under an unknown impersonation identity is unsafe.
        std::process::abort();
    }
    if opened == 0 {
        return Err(error);
    }
    let pipe_token = Handle(pipe_token);
    verify_identity(
        owner,
        &token_sid(pipe_token.0)?,
        session,
        token_session(pipe_token.0)?,
    )?;
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = Handle(process);
    if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_TIMEOUT {
        return Err(denied("Requesting manager has exited"));
    }
    let token = process_token(process.0)?;
    verify_identity(
        owner,
        &token_sid(token.0)?,
        session,
        token_session(token.0)?,
    )?;
    let linked = if elevated(token.0)? {
        token
    } else {
        let bytes = token_data(token.0, TokenLinkedToken)?;
        Handle(unsafe { (*bytes.as_ptr().cast::<TOKEN_LINKED_TOKEN>()).LinkedToken })
    };
    verify_identity(
        owner,
        &token_sid(linked.0)?,
        session,
        token_session(linked.0)?,
    )?;
    if !elevated(linked.0)? {
        return Err(denied(
            "This account has no elevated token; administrator sign-in is required",
        ));
    }
    let mut primary = std::ptr::null_mut();
    if unsafe {
        DuplicateTokenEx(
            linked.0,
            TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let identity = pinned_identity(process.0, pid)?;
    Ok(Peer {
        process,
        token: Handle(primary),
        identity,
    })
}
fn verify_identity(owner: &str, actual: &str, session: u32, actual_session: u32) -> io::Result<()> {
    if actual != owner || actual == "S-1-5-18" || session == 0 || actual_session != session {
        return Err(denied(
            "Service request must belong to the registered user and logon session",
        ));
    }
    Ok(())
}
fn pinned_identity(process: HANDLE, pid: u32) -> io::Result<crate::lifecycle::ProcessIdentity> {
    let [mut created, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
    if unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(crate::lifecycle::ProcessIdentity {
        pid,
        started: ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
            .to_string(),
    })
}

fn validated_args(
    mut args: Vec<String>,
    owner: &str,
    installation: &Path,
    identity: &crate::lifecycle::ProcessIdentity,
) -> io::Result<Vec<String>> {
    if args.len() != 12 || args[0] != "--windows-elevated" || args.iter().any(|a| a.contains('\0'))
    {
        return Err(invalid("Invalid privileged core argument layout"));
    }
    for index in [1, 2, 3, 8] {
        if args[index].is_empty() || !Path::new(&args[index]).is_absolute() {
            return Err(invalid("Privileged core paths must be absolute"));
        }
    }
    if !matches!(
        args[4].as_str(),
        "error" | "warn" | "info" | "debug" | "trace" | "route"
    ) {
        return Err(invalid("Invalid core log level"));
    }
    if !args[9].is_empty()
        && args[9]
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid != 0)
            .is_none()
    {
        return Err(invalid("Invalid predecessor PID"));
    }
    args[5] = installation
        .join("wintun.dll")
        .to_str()
        .ok_or_else(|| invalid("Service path is not UTF-8"))?
        .into();
    args[6] = installation
        .to_str()
        .ok_or_else(|| invalid("Service path is not UTF-8"))?
        .into();
    args[7] = owner.into();
    args[10] = "1".into();
    args[11] = serde_json::to_string(identity).map_err(io::Error::other)?;
    Ok(args)
}
fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in value.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            '\\',
            if c == '"' { slashes * 2 + 1 } else { slashes },
        ));
        result.push(c);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}
fn environment(installation: &Path) -> io::Result<Vec<u16>> {
    let mut root = vec![0u16; 32768];
    let len = unsafe { GetWindowsDirectoryW(root.as_mut_ptr(), root.len() as u32) } as usize;
    if len == 0 || len >= root.len() {
        return Err(io::Error::last_os_error());
    }
    let root = String::from_utf16(&root[..len]).map_err(io::Error::other)?;
    // No inherited user PATH, DLL search overrides, plugin commands or ASTRA
    // switches. Bootstrap derives the permitted switches from validated args.
    let entries = [
        format!("PATH={};{}\\System32", installation.display(), root),
        format!("SystemRoot={root}"),
        format!("WINDIR={root}"),
    ];
    let mut result = Vec::new();
    for entry in entries {
        result.extend(wide(OsStr::new(&entry))?);
    }
    result.push(0);
    Ok(result)
}
struct Spawned {
    process: Handle,
    thread: Handle,
    target: Handle,
    remote: usize,
    handle_delivered: bool,
    transferred: bool,
}
impl Spawned {
    fn resume(&self) -> io::Result<()> {
        if unsafe { ResumeThread(self.thread.0) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Spawned {
    fn drop(&mut self) {
        if !self.transferred {
            unsafe {
                TerminateProcess(self.process.0, 1);
                if self.remote != 0 && !self.handle_delivered {
                    let mut local = std::ptr::null_mut();
                    if DuplicateHandle(
                        self.target.0,
                        self.remote as HANDLE,
                        GetCurrentProcess(),
                        &mut local,
                        0,
                        0,
                        DUPLICATE_CLOSE_SOURCE,
                    ) != 0
                        && !local.is_null()
                    {
                        CloseHandle(local);
                    }
                }
            }
        }
    }
}
fn spawn(
    peer: Peer,
    request: SpawnRequest,
    owner: &str,
    installation: &Path,
) -> io::Result<Spawned> {
    let proxy = super::security::pin_protected(&installation.join("astra-proxy.exe"))?;
    let _driver = super::security::pin_protected(&installation.join("wintun.dll"))?;
    let args = validated_args(request.args, owner, installation, &peer.identity)?;
    let executable = wide(proxy.path().as_os_str())?;
    let mut command = quote(
        proxy
            .path()
            .to_str()
            .ok_or_else(|| invalid("Service executable path is not UTF-8"))?,
    );
    for arg in args {
        command.push(' ');
        command.push_str(&quote(&arg));
    }
    let mut command = wide(OsStr::new(&command))?;
    if command.len() > 32767 {
        return Err(invalid(
            "Privileged core command line exceeds Windows limit",
        ));
    }
    let environment = environment(installation)?;
    let cwd = wide(installation.as_os_str())?;
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();
    if unsafe {
        CreateProcessAsUserW(
            peer.token.0,
            executable.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup,
            &mut information,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut child = Spawned {
        process: Handle(information.hProcess),
        thread: Handle(information.hThread),
        target: peer.process,
        remote: 0,
        handle_delivered: false,
        transferred: false,
    };
    let mut remote = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            child.process.0,
            child.target.0,
            &mut remote,
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    child.remote = remote as usize;
    Ok(child)
}

#[cfg(test)]
#[path = "../test/windows_service_host_test.rs"]
mod tests;
