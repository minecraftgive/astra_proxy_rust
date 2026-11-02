//! Windows privilege boundary. Only a TUN proxy is elevated; its caller retains
//! the process handle, so ownership never depends on a saved or reported PID.
use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::AsRawHandle,
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::ExitStatus,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, LocalFree, GENERIC_ALL, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, GetNamedSecurityInfoW, GetSecurityInfo, SE_FILE_OBJECT,
        },
        GetAce, GetTokenInformation, TokenElevation, TokenOwner, TokenUser, ACCESS_ALLOWED_ACE,
        ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_ELEVATION, TOKEN_OWNER,
        TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE,
        VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
    },
    System::{
        Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE},
        Threading::{
            GetCurrentProcess, GetExitCodeProcess, GetProcessId, OpenProcess, OpenProcessToken,
            TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        },
    },
    UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
};

pub(crate) struct Handle(pub(crate) HANDLE);
// Windows process/token handles can be queried across threads. The handle is
// closed only on Drop, which requires exclusive ownership.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn token() -> io::Result<Handle> {
    let mut handle = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(handle))
}
pub fn is_elevated() -> io::Result<bool> {
    let token = token()?;
    let mut elevation: TOKEN_ELEVATION = unsafe { std::mem::zeroed() };
    let mut size = 0;
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of_val(&elevation) as u32,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}
fn token_information(class: i32) -> io::Result<Vec<usize>> {
    let token = token()?;
    let mut size = 0;
    unsafe {
        GetTokenInformation(token.0, class, std::ptr::null_mut(), 0, &mut size);
    }
    let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe { GetTokenInformation(token.0, class, data.as_mut_ptr().cast(), size, &mut size) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(data)
}
fn user_sid() -> io::Result<String> {
    let data = token_information(TokenUser)?;
    sid_string(unsafe { &*data.as_ptr().cast::<TOKEN_USER>() }.User.Sid)
}
/// The default owner of objects this process creates: the user itself, or
/// usually Administrators for an elevated token.
fn token_owner_sid() -> io::Result<String> {
    let data = token_information(TokenOwner)?;
    sid_string(unsafe { &*data.as_ptr().cast::<TOKEN_OWNER>() }.Owner)
}
fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(value)
    }
}

/// Every path an elevated core receives comes from the unelevated user, who
/// can plant junctions, symbolic links and hard links in their own folders to
/// redirect it at protected files. Objects are therefore opened without
/// following a final reparse point and checked by handle.
pub(crate) fn open_object(path: &Path, access: u32) -> io::Result<Handle> {
    let name = wide(path.as_os_str())?;
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(handle))
}
fn file_information(handle: HANDLE) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}
/// Requires `READ_CONTROL` on the handle.
fn owner_sid(handle: HANDLE) -> io::Result<String> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let error = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let result = sid_string(owner);
    unsafe {
        LocalFree(descriptor);
    }
    result
}
fn final_path(handle: HANDLE) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 1024];
    loop {
        let len = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                VOLUME_NAME_DOS,
            )
        } as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if len < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..len])));
        }
        buffer.resize(len + 1, 0);
    }
}
/// Accept an opened object only if it is not a reparse point, is not a hard
/// link to some other file, and belongs to the user: owned by the user, or
/// created by this (elevated) process inside a folder the user owns. A
/// redirected path lands on a system-owned object and is refused before
/// anything is written to it or its permissions change.
pub(crate) fn ensure_user_object(handle: &Handle, path: &Path) -> io::Result<()> {
    let refuse = |reason: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} {reason}; refusing to use it from an elevated process",
                path.display()
            ),
        ))
    };
    let info = file_information(handle.0)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return refuse("is a link or junction");
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.nNumberOfLinks != 1 {
        return refuse("has several hard links");
    }
    let user = user_sid()?;
    let owner = owner_sid(handle.0)?;
    if owner == user {
        return Ok(());
    }
    if owner == token_owner_sid()? {
        if let Some(parent) = final_path(handle.0)?.parent() {
            let directory = open_object(parent, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
            let attributes = file_information(directory.0)?.dwFileAttributes;
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 && owner_sid(directory.0)? == user {
                return Ok(());
            }
        }
    }
    refuse("is not owned by the current user")
}
/// Open an existing user object (see `ensure_user_object`) for `access`.
pub(crate) fn open_user_object(path: &Path, access: u32) -> io::Result<Handle> {
    let handle = open_object(path, access | READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    ensure_user_object(&handle, path)?;
    Ok(handle)
}

/// SIDs that may control a program UAC runs elevated: SYSTEM, Administrators,
/// TrustedInstaller, and CREATOR OWNER / OWNER RIGHTS (which only stand for
/// the owner, itself required to be one of the others).
const ADMIN_SIDS: &[&str] = &[
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
    "S-1-3-0",
    "S-1-3-4",
];
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;

/// A program UAC runs elevated must not be replaceable by the unelevated user
/// (or anything running as them) while the prompt is open: the file, its
/// folder (where a planted DLL would load first) and every folder above must
/// be owned by an administrator principal and grant no one else rights that
/// replace, rename or re-permission them. Returns the resolved path.
pub fn ensure_admin_controlled(path: &Path) -> io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    if !std::fs::metadata(&path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the elevated program is not a regular file",
        ));
    }
    let control = WRITE_DAC | WRITE_OWNER | GENERIC_ALL;
    for (depth, item) in path.ancestors().enumerate() {
        let dangerous = match depth {
            0 => control | DELETE | FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE,
            1 => control | DELETE | FILE_DELETE_CHILD | FILE_WRITE_DATA | GENERIC_WRITE,
            _ => control | DELETE | FILE_DELETE_CHILD,
        };
        if let Some(principal) = foreign_writer(item, dangerous)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} can be changed by {principal}; install the core in a folder only administrators can modify (for example under Program Files) before enabling TUN",
                    item.display()
                ),
            ));
        }
    }
    Ok(strip_verbatim(path))
}
/// The first non-administrator principal that owns `path` or holds any of
/// `dangerous` on it.
fn foreign_writer(path: &Path, dangerous: u32) -> io::Result<Option<String>> {
    let name = wide(path.as_os_str())?;
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let error = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let result = (|| {
        let owner = sid_string(owner)?;
        if !ADMIN_SIDS.contains(&owner.as_str()) {
            return Ok(Some(format!("its owner {owner}")));
        }
        if dacl.is_null() {
            return Ok(Some("everyone (it has no access list)".into()));
        }
        for index in 0..u32::from(unsafe { (*dacl).AceCount }) {
            let mut ace = std::ptr::null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
                || !matches!(
                    header.AceType,
                    ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                )
            {
                continue;
            }
            // The callback variant shares this layout up to the SID.
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            if unsafe { (*allowed).Mask } & dangerous == 0 {
                continue;
            }
            let sid = sid_string(unsafe { std::ptr::addr_of_mut!((*allowed).SidStart) }.cast())?;
            if !ADMIN_SIDS.contains(&sid.as_str()) {
                return Ok(Some(sid));
            }
        }
        Ok(None)
    })();
    unsafe {
        LocalFree(descriptor);
    }
    result
}
/// ShellExecute does not reliably accept `\\?\` paths; a local drive path is
/// the same file without the prefix.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}
fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in Windows argument",
        ));
    }
    value.push(0);
    Ok(value)
}
// Windows CRT argument quoting, including empty arguments and trailing slashes.
fn quote(value: &OsStr) -> Vec<u16> {
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for ch in value.encode_wide() {
        if ch == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            b'\\' as u16,
            if ch == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        result.push(ch);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    result
}

pub struct ElevatedChild {
    handle: Handle,
    exited: bool,
}
impl ElevatedChild {
    pub fn id(&self) -> Option<u32> {
        Some(unsafe { GetProcessId(self.handle.0) })
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match unsafe { WaitForSingleObject(self.handle.0, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.handle.0, &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                self.exited = true;
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn start_kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_none() && unsafe { TerminateProcess(self.handle.0, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    pub async fn kill(&mut self) -> io::Result<()> {
        self.start_kill()?;
        self.wait().await?;
        Ok(())
    }
}
impl Drop for ElevatedChild {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.start_kill();
        }
    }
}

/// UAC does not inherit the caller's environment or standard handles. Pass only
/// the settings needed by the core, and keep the original user identity.
///
/// `replaces` names a running core this one takes over from: the elevated
/// process is created (and UAC approved) while that core still serves, then
/// waits for it to exit before loading its configuration. A cancelled prompt
/// therefore leaves the running core untouched.
pub fn core_arguments(
    config: &Path,
    socket: Option<&Path>,
    log: &Path,
    level: &str,
    replaces: Option<u32>,
) -> io::Result<Vec<OsString>> {
    Ok(vec![
        "--windows-elevated".into(),
        config.as_os_str().into(),
        socket.map(|p| p.as_os_str().to_owned()).unwrap_or_default(),
        log.as_os_str().into(),
        level.into(),
        std::env::var_os("ASTRA_WINTUN_DLL")
            .map(std::path::absolute)
            .transpose()?
            .map(|p| p.into_os_string())
            .unwrap_or_default(),
        std::env::current_dir()?.into_os_string(),
        user_sid()?.into(),
        runtime_dir()?.into_os_string(),
        replaces
            .map(|pid| pid.to_string())
            .unwrap_or_default()
            .into(),
    ])
}
/// Created here, unelevated, so the elevated core never creates folders on a
/// path the user controls; it only verifies this one.
fn runtime_dir() -> io::Result<PathBuf> {
    let path = std::path::absolute(crate::storage::runtime_root()?)?;
    std::fs::create_dir_all(&path)?;
    crate::platform::set_private(&path, 0o700)?;
    Ok(path)
}
/// How long a replacement core waits for its predecessor to drain and exit.
const REPLACE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
fn wait_for_exit(pid: u32) -> io::Result<()> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // ERROR_INVALID_PARAMETER: the predecessor has already exited.
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let handle = Handle(handle);
    match unsafe { WaitForSingleObject(handle.0, REPLACE_WAIT.as_millis() as u32) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("previous core PID {pid} did not exit; elevated core not started"),
        )),
        _ => Err(io::Error::last_os_error()),
    }
}
pub fn spawn(exe: &Path, args: &[OsString]) -> io::Result<ElevatedChild> {
    let exe = ensure_admin_controlled(exe)?;
    let exe = wide(exe.as_os_str())?;
    let verb = wide(OsStr::new("runas"))?;
    let mut parameters = Vec::new();
    for arg in args {
        wide(arg)?;
        if !parameters.is_empty() {
            parameters.push(b' ' as u16);
        }
        parameters.extend(quote(arg));
    }
    parameters.push(0);
    let directory = wide(std::env::current_dir()?.as_os_str())?;
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of_val(&info) as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = exe.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.lpDirectory = directory.as_ptr();
    info.nShow = 0;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(1223) {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows TUN elevation was cancelled; instance was not started",
            )
        } else {
            error
        });
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other("UAC returned no owned process handle"));
    }
    Ok(ElevatedChild {
        handle: Handle(info.hProcess),
        exited: false,
    })
}

/// Append to the core log only inside a folder the user owns, never through a
/// final link, and only if the file that was opened is the user's own.
fn open_log(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::{fs::OpenOptionsExt, io::FromRawHandle, io::IntoRawHandle};
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "log path has no folder")
    })?;
    drop(open_user_object(parent, 0)?);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .access_mode(FILE_APPEND_DATA | SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let handle = Handle(file.into_raw_handle());
    ensure_user_object(&handle, path)?;
    let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    std::mem::forget(handle);
    Ok(file)
}

/// Called before logging/config loading in the elevated core. The fixed argument
/// list prevents accidentally interpreting a privileged launch as a normal one.
pub fn bootstrap() -> io::Result<Option<OsString>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|arg| arg != "--windows-elevated") {
        return Ok(None);
    }
    if args.len() != 10 || !is_elevated()? || args[7] != OsStr::new(&user_sid()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "TUN elevation requires the original Windows account and an elevated token",
        ));
    }
    // This core is driven by its non-elevated launcher (manager/TUI).
    crate::platform::allow_unelevated_clients();
    std::env::set_current_dir(&args[6])?;
    // Every path below was chosen by the unelevated user; each must resolve
    // to the user's own object, never one redirected into a protected folder.
    let runtime = Path::new(&args[8]);
    let directory = open_user_object(runtime, 0)?;
    if file_information(directory.0)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the runtime directory is not a folder",
        ));
    }
    drop(directory);
    std::env::set_var("ASTRA_RUNTIME_DIR", runtime);
    // Reading the configuration as administrator must not disclose files the
    // user could not read themselves.
    open_user_object(Path::new(&args[1]), 0)?;
    if !args[2].is_empty() {
        std::env::set_var("ASTRA_CONTROL_SOCKET", &args[2]);
    } else {
        std::env::remove_var("ASTRA_CONTROL_SOCKET");
    }
    std::env::set_var("ASTRA_LOG_LEVEL", &args[4]);
    if !args[5].is_empty() {
        std::env::set_var("ASTRA_WINTUN_DLL", &args[5]);
    } else {
        std::env::remove_var("ASTRA_WINTUN_DLL");
    }
    let file = open_log(Path::new(&args[3]))?;
    unsafe {
        if SetStdHandle(STD_OUTPUT_HANDLE, file.as_raw_handle()) == 0
            || SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle()) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // Standard handles stay valid until process exit.
    std::mem::forget(file);
    if !args[9].is_empty() {
        let pid = args[9]
            .to_str()
            .and_then(|pid| pid.parse().ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid predecessor PID")
            })?;
        // Ports, the TUN adapter and the control endpoint free up only once
        // the predecessor is gone; an error here lands in the redirected log.
        wait_for_exit(pid)?;
    }
    Ok(Some(args[1].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};

    fn owned_process(command: &mut std::process::Command) -> (std::process::Child, ElevatedChild) {
        let child = command.spawn().unwrap();
        let mut handle = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    child.as_raw_handle(),
                    GetCurrentProcess(),
                    &mut handle,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            },
            0
        );
        (
            child,
            ElevatedChild {
                handle: Handle(handle),
                exited: false,
            },
        )
    }

    #[tokio::test]
    async fn owned_handle_preserves_pid_and_exit_status() {
        let mut command = std::process::Command::new(std::env::var_os("COMSPEC").unwrap());
        command.args(["/d", "/c", "exit 7"]);
        let (mut process, mut owned) = owned_process(&mut command);
        assert_eq!(owned.id(), Some(process.id()));
        assert_eq!(owned.wait().await.unwrap().code(), Some(7));
        assert_eq!(owned.try_wait().unwrap().unwrap().code(), Some(7));
        process.wait().unwrap();
    }

    #[test]
    fn dropping_owned_handle_terminates_live_process() {
        let mut command = std::process::Command::new(std::env::var_os("COMSPEC").unwrap());
        // No subprocess: an interactive cmd blocks on its open input pipe.
        command
            .args(["/d", "/q"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null());
        let (mut process, mut owned) = owned_process(&mut command);
        assert!(owned.try_wait().unwrap().is_none());
        drop(owned);
        assert_eq!(process.wait().unwrap().code(), Some(1));
    }

    #[test]
    fn argument_list_round_trips_through_windows_parser() {
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
        let values = [
            "",
            "中文 路径",
            "C:\\space dir\\",
            "a\"b",
            "\\\\\"quoted",
            "& calc.exe",
        ];
        let mut line: Vec<u16> = "astra.exe".encode_utf16().collect();
        for value in values {
            line.push(b' ' as u16);
            line.extend(quote(OsStr::new(value)));
        }
        line.push(0);
        let mut count = 0;
        let args = unsafe { CommandLineToArgvW(line.as_ptr(), &mut count) };
        assert!(!args.is_null());
        assert_eq!(count as usize, values.len() + 1);
        let mut actual = Vec::new();
        unsafe {
            for index in 1..count as usize {
                let ptr = *args.add(index);
                let mut len = 0;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                actual.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                    ptr, len,
                )));
            }
            LocalFree(args.cast());
        }
        assert_eq!(actual, values);
        assert!(wide(OsStr::new("bad\0argument")).is_err());
    }
    #[test]
    fn windows_argument_boundaries() {
        for (input, expected) in [
            ("", "\"\""),
            ("中文 路径", "\"中文 路径\""),
            ("C:\\with space\\", "\"C:\\with space\\\\\""),
            ("a\"b", "\"a\\\"b\""),
        ] {
            assert_eq!(
                String::from_utf16(&quote(OsStr::new(input))).unwrap(),
                expected
            );
        }
    }
    #[test]
    fn wait_for_exit_returns_once_the_process_is_gone() {
        let mut child = std::process::Command::new(std::env::var_os("COMSPEC").unwrap())
            .args(["/d", "/c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        drop(child);
        wait_for_exit(pid).unwrap();
        wait_for_exit(u32::MAX - 3).unwrap();
    }
    #[test]
    fn user_writable_program_is_not_elevated() {
        let exe = std::env::temp_dir().join(format!("astra-elevation-{}.exe", std::process::id()));
        std::fs::write(&exe, b"").unwrap();
        assert!(ensure_admin_controlled(&exe).is_err());
        std::fs::remove_file(exe).unwrap();
    }
    #[test]
    fn system_program_is_admin_controlled() {
        let shell = std::env::var_os("COMSPEC").unwrap();
        let resolved = ensure_admin_controlled(Path::new(&shell)).unwrap();
        assert!(!resolved.to_string_lossy().starts_with(r"\\?\"));
    }
    #[test]
    fn user_objects_pass_and_hard_links_are_refused() {
        let root = std::env::temp_dir().join(format!("astra-objects-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("log");
        std::fs::write(&file, b"").unwrap();
        open_user_object(&root, 0).unwrap();
        open_user_object(&file, 0).unwrap();
        std::fs::hard_link(&file, root.join("alias")).unwrap();
        assert!(open_user_object(&file, 0).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn token_queries_work_without_elevation() {
        is_elevated().unwrap();
        assert!(user_sid().unwrap().starts_with("S-1-"));
    }
}
