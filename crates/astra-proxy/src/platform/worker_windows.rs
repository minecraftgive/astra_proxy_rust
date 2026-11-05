//! Launch package code under the user's filtered UAC token. Missing tokens or
//! launch privileges fail closed; there is no elevated fallback or UAC prompt.
//! See https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw
use anyhow::{ensure, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::Path,
    process::ExitStatus,
    ptr::{null, null_mut},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        DuplicateHandle, GetLastError, SetHandleInformation, DUPLICATE_SAME_ACCESS, HANDLE,
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        DuplicateTokenEx, EqualSid, GetSidSubAuthority, GetSidSubAuthorityCount,
        GetTokenInformation, IsValidSid, SecurityImpersonation, TokenElevation,
        TokenIntegrityLevel, TokenLinkedToken, TokenPrimary, TokenSessionId, TokenUser,
        SECURITY_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION,
        TOKEN_INFORMATION_CLASS, TOKEN_LINKED_TOKEN, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        TOKEN_USER,
    },
    System::{
        Pipes::CreatePipe,
        Threading::{
            CreateProcessAsUserW, DeleteProcThreadAttributeList, GetCurrentProcess,
            GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken, ResumeThread,
            TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW,
            CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            STARTF_USESTDHANDLES, STARTUPINFOEXW,
        },
    },
};

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}
unsafe fn own(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn wide(value: &OsStr) -> Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    ensure!(!value.contains(&0), "NUL in worker launch path");
    value.push(0);
    Ok(value)
}
fn token(process: HANDLE, access: u32) -> io::Result<OwnedHandle> {
    let mut handle = null_mut();
    check(unsafe { OpenProcessToken(process, access, &mut handle) })?;
    unsafe { own(handle) }
}
fn information(handle: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut size = 0;
    unsafe {
        GetTokenInformation(handle, class, null_mut(), 0, &mut size);
    }
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    check(unsafe {
        GetTokenInformation(handle, class, data.as_mut_ptr().cast(), size, &mut size)
    })?;
    Ok(data)
}
fn ordinary(handle: HANDLE) -> Result<()> {
    let elevation = information(handle, TokenElevation)?;
    let elevation = unsafe { &*elevation.as_ptr().cast::<TOKEN_ELEVATION>() };
    ensure!(
        elevation.TokenIsElevated == 0,
        "refusing an elevated worker token"
    );
    let label = information(handle, TokenIntegrityLevel)?;
    let label = unsafe { &*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    ensure!(
        unsafe { IsValidSid(label.Label.Sid) } != 0,
        "invalid token integrity SID"
    );
    let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
    ensure!(count > 0, "missing token integrity level");
    let integrity = unsafe { *GetSidSubAuthority(label.Label.Sid, (count - 1).into()) };
    ensure!(
        integrity <= 0x2000,
        "worker requires medium-or-lower integrity"
    );
    Ok(())
}
fn same_identity(left: HANDLE, right: HANDLE) -> Result<()> {
    let left_user = information(left, TokenUser)?;
    let right_user = information(right, TokenUser)?;
    let left_user = unsafe { &*left_user.as_ptr().cast::<TOKEN_USER>() };
    let right_user = unsafe { &*right_user.as_ptr().cast::<TOKEN_USER>() };
    ensure!(
        unsafe { EqualSid(left_user.User.Sid, right_user.User.Sid) } != 0,
        "worker token belongs to another user"
    );
    let left_session = information(left, TokenSessionId)?;
    let right_session = information(right, TokenSessionId)?;
    ensure!(
        unsafe { *left_session.as_ptr().cast::<u32>() == *right_session.as_ptr().cast::<u32>() },
        "worker token belongs to another session"
    );
    Ok(())
}
fn filtered_token() -> Result<OwnedHandle> {
    let current = token(unsafe { GetCurrentProcess() }, TOKEN_QUERY)?;
    let linked = information(raw(&current), TokenLinkedToken).context(
        "no filtered UAC token; start the resident host from its ordinary-user supervisor",
    )?;
    let linked = unsafe { own((*linked.as_ptr().cast::<TOKEN_LINKED_TOKEN>()).LinkedToken) }?;
    ordinary(raw(&linked))?;
    same_identity(raw(&current), raw(&linked))?;
    let mut primary = null_mut();
    check(unsafe {
        DuplicateTokenEx(
            raw(&linked),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    })?;
    unsafe { own(primary) }.map_err(Into::into)
}

struct Attributes {
    data: Vec<usize>,
}
impl Attributes {
    fn new(handles: &mut [HANDLE]) -> io::Result<Self> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut this = Self {
            data: vec![0; size.div_ceil(size_of::<usize>())],
        };
        if unsafe { InitializeProcThreadAttributeList(this.pointer(), 1, 0, &mut size) } == 0 {
            let error = io::Error::last_os_error();
            this.data.clear(); // Drop must not delete an uninitialized list.
            return Err(error);
        }
        check(unsafe {
            UpdateProcThreadAttribute(
                this.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_mut_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        })?;
        Ok(this)
    }
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.data.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        if !self.data.is_empty() {
            unsafe {
                DeleteProcThreadAttributeList(self.pointer());
            }
        }
    }
}

fn environment() -> Result<Vec<u16>> {
    environment_from(std::env::vars_os())
}
fn environment_from(values: impl IntoIterator<Item = (OsString, OsString)>) -> Result<Vec<u16>> {
    let mut values: Vec<_> = values
        .into_iter()
        .filter(|(key, _)| {
            !super::REMOVED_ENV
                .iter()
                .any(|name| key.to_string_lossy().eq_ignore_ascii_case(name))
        })
        .collect();
    values.sort_by_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (key, value) in values {
        let mut item = key;
        item.push("=");
        item.push(value);
        block.extend(wide(&item)?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

pub struct Child {
    process: OwnedHandle,
    pid: u32,
    exit: Option<ExitStatus>,
}
impl Child {
    pub fn id(&self) -> Option<u32> {
        self.exit.is_none().then_some(self.pid)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(exit) = self.exit {
            return Ok(Some(exit));
        }
        match unsafe { WaitForSingleObject(raw(&self.process), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                check(unsafe { GetExitCodeProcess(raw(&self.process), &mut code) })?;
                let exit = ExitStatus::from_raw(code);
                self.exit = Some(exit);
                Ok(Some(exit))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub async fn kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        check(unsafe { TerminateProcess(raw(&self.process), 1) })?;
        while self.try_wait()?.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !matches!(self.try_wait(), Ok(Some(_))) {
            unsafe {
                TerminateProcess(raw(&self.process), 1);
            }
        }
    }
}

pub fn spawn(binary: &Path, log: &File) -> Result<(Child, File)> {
    let primary = filtered_token()?;
    let binary = std::fs::canonicalize(binary)?;
    let application = wide(binary.as_os_str())?;
    let mut command = OsString::from("\"");
    command.push(&binary);
    command.push("\" --host-worker");
    let mut command = wide(&command)?;
    let environment = environment()?;
    let directory = wide(
        binary
            .parent()
            .context("worker has no parent directory")?
            .as_os_str(),
    )?;
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write) = (null_mut(), null_mut());
    check(unsafe { CreatePipe(&mut read, &mut write, &security, 0) })?;
    let stdin = unsafe { own(read) }?;
    let writer = unsafe { own(write) }?;
    check(unsafe { SetHandleInformation(raw(&writer), HANDLE_FLAG_INHERIT, 0) })?;
    let mut stdout = null_mut();
    let mut stderr = null_mut();
    let process = unsafe { GetCurrentProcess() };
    check(unsafe {
        DuplicateHandle(
            process,
            log.as_raw_handle(),
            process,
            &mut stdout,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    })?;
    let stdout = unsafe { own(stdout) }?;
    check(unsafe {
        DuplicateHandle(
            process,
            log.as_raw_handle(),
            process,
            &mut stderr,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    })?;
    let stderr = unsafe { own(stderr) }?;
    let mut handles = [raw(&stdin), raw(&stdout), raw(&stderr)];
    let mut attributes = Attributes::new(&mut handles)?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = raw(&stdin);
    startup.StartupInfo.hStdOutput = raw(&stdout);
    startup.StartupInfo.hStdError = raw(&stderr);
    startup.lpAttributeList = attributes.pointer();
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    check(unsafe {
        CreateProcessAsUserW(
            raw(&primary),
            application.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_NO_WINDOW
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    })
    .context("start ordinary worker using the filtered token; elevated fallback is prohibited")?;
    let child = Child {
        process: unsafe { own(info.hProcess) }?,
        pid: info.dwProcessId,
        exit: None,
    };
    let thread = unsafe { own(info.hThread) }?;
    // Recheck the actual child token before any package code executes.
    let child_token = token(raw(&child.process), TOKEN_QUERY)?;
    ordinary(raw(&child_token))?;
    same_identity(raw(&primary), raw(&child_token))?;
    if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
        return Err(io::Error::from_raw_os_error(unsafe { GetLastError() } as i32).into());
    }
    Ok((child, File::from(writer)))
}

#[cfg(test)]
#[path = "../test/platform_worker_windows_test.rs"]
mod tests;
