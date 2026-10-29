//! Windows privilege boundary. Only a TUN proxy is elevated; its caller retains
//! the process handle, so ownership never depends on a saved or reported PID.
use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::ExitStatusExt},
    path::Path,
    process::ExitStatus,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenElevation, TokenUser,
        TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    },
    System::{
        Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE},
        Threading::{
            GetCurrentProcess, GetExitCodeProcess, GetProcessId, OpenProcessToken,
            TerminateProcess, WaitForSingleObject,
        },
    },
    UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
};

struct Handle(HANDLE);
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
fn user_sid() -> io::Result<String> {
    let token = token()?;
    let mut size = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut size);
    }
    let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    unsafe {
        while *sid.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());
        Ok(text)
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
pub fn core_arguments(
    config: &Path,
    socket: Option<&Path>,
    log: &Path,
    level: &str,
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
        std::env::var_os("ASTRA_RUNTIME_DIR")
            .map(std::path::absolute)
            .transpose()?
            .map(|p| p.into_os_string())
            .unwrap_or_default(),
    ])
}
pub fn spawn(exe: &Path, args: &[OsString]) -> io::Result<ElevatedChild> {
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

/// Called before logging/config loading in the elevated core. The fixed argument
/// list prevents accidentally interpreting a privileged launch as a normal one.
pub fn bootstrap() -> io::Result<Option<OsString>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|arg| arg != "--windows-elevated") {
        return Ok(None);
    }
    if args.len() != 9 || !is_elevated()? || args[7] != OsStr::new(&user_sid()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "TUN elevation requires the original Windows account and an elevated token",
        ));
    }
    std::env::set_current_dir(&args[6])?;
    if args[8].is_empty() {
        std::env::remove_var("ASTRA_RUNTIME_DIR");
    } else {
        std::env::set_var("ASTRA_RUNTIME_DIR", &args[8]);
    }
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
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args[3])?;
    unsafe {
        if SetStdHandle(STD_OUTPUT_HANDLE, file.as_raw_handle()) == 0
            || SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle()) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // Standard handles stay valid until process exit.
    std::mem::forget(file);
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
    fn token_queries_work_without_elevation() {
        is_elevated().unwrap();
        assert!(user_sid().unwrap().starts_with("S-1-"));
    }
}
