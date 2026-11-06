//! Console setup for the single GUI-subsystem frontend executable.
use anyhow::{Context as _, Result};
use std::{
    fs::OpenOptions,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
};
use windows_sys::Win32::{
    Foundation::{
        DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE,
        ERROR_INVALID_PARAMETER, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        GetFileType, FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_PIPE, FILE_TYPE_UNKNOWN,
    },
    System::{
        Console::{
            AllocConsole, AttachConsole, FreeConsole, GetConsoleMode, GetConsoleProcessList,
            GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        },
        Threading::GetCurrentProcess,
    },
    UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleMode {
    /// Help/status/errors may use the parent's console, but never create one.
    CommandLine,
    /// A GUI executable's launching shell may not wait; use an independent
    /// console so its prompt cannot consume the TUI's keyboard input.
    Interactive,
    /// Explicit --native-console and detached-page launches use the same
    /// independent-console policy, even when standard handles were inherited.
    NewWindow,
    /// Started by a selected terminal's waiting console process. Share that
    /// host instead of allocating a second window or reading the shell prompt.
    Hosted,
}

pub fn prepare_console(mode: ConsoleMode) -> Result<()> {
    match mode {
        ConsoleMode::CommandLine => attach_parent(),
        ConsoleMode::Hosted => {
            attach_parent()?;
            let mut processes = [0u32; 1];
            anyhow::ensure!(
                unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 1) } > 0,
                "The selected terminal did not provide a console"
            );
            attach_console_io()
        }
        ConsoleMode::Interactive | ConsoleMode::NewWindow => {
            let mut processes = [0u32; 2];
            let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 2) };
            let exclusive = count == 1 && processes[0] == std::process::id();
            if !exclusive {
                if count != 0 && unsafe { FreeConsole() } == 0 {
                    return Err(io::Error::last_os_error()).context("Detach shared console");
                }
                if unsafe { AllocConsole() } == 0 {
                    return Err(io::Error::last_os_error()).context("Create interactive console");
                }
            }
            attach_console_io()
        }
    }
}

const STREAMS: [(u32, &str); 3] = [
    (STD_INPUT_HANDLE, "CONIN$"),
    (STD_OUTPUT_HANDLE, "CONOUT$"),
    (STD_ERROR_HANDLE, "CONOUT$"),
];

enum Redirect {
    FileOrPipe(OwnedHandle),
    Character(OwnedHandle),
}

fn save_redirect(handle: HANDLE) -> Result<Option<Redirect>> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    let mut mode = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } != 0 {
        return Ok(None);
    }
    Ok(match unsafe { GetFileType(handle) } {
        FILE_TYPE_DISK | FILE_TYPE_PIPE => Some(Redirect::FileOrPipe(duplicate(handle)?)),
        // Before AttachConsole, an inherited console handle can look like an
        // inaccessible character device. Do not fail startup while trying to
        // copy it. A successful copy is classified again after attachment so
        // genuine NUL redirection is preserved without retaining old consoles.
        FILE_TYPE_CHAR => duplicate(handle).ok().map(Redirect::Character),
        _ => None,
    })
}

fn duplicate(handle: HANDLE) -> Result<OwnedHandle> {
    let mut copy = std::ptr::null_mut();
    let process = unsafe { GetCurrentProcess() };
    if unsafe {
        DuplicateHandle(
            process,
            handle,
            process,
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("Preserve redirected standard handle");
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(copy) })
}

fn install(which: u32, handle: OwnedHandle) -> Result<()> {
    if unsafe { SetStdHandle(which, handle.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error()).context("Set console standard handle");
    }
    // Rust's standard streams query the OS handle table. Retain installed
    // handles until process exit, including code that cached an older handle.
    let _ = handle.into_raw_handle();
    Ok(())
}

fn open_console(path: &str) -> Result<OwnedHandle> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("Open console input/output")?
        .into())
}

fn attach_parent() -> Result<()> {
    // AttachConsole can replace standard handles. Save genuine redirections
    // (including pipes, files and NUL) before attaching, then restore them.
    let mut saved = Vec::with_capacity(STREAMS.len());
    for (which, _) in STREAMS {
        let handle = unsafe { GetStdHandle(which) };
        saved.push(save_redirect(handle)?);
    }
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_ACCESS_DENIED) => {} // Already attached.
            Some(ERROR_INVALID_HANDLE | ERROR_INVALID_PARAMETER) => return Ok(()),
            _ => return Err(error).context("Attach parent console"),
        }
    }
    for ((which, path), redirected) in STREAMS.into_iter().zip(saved) {
        install(
            which,
            match redirected {
                Some(Redirect::FileOrPipe(handle)) => handle,
                Some(Redirect::Character(handle)) => {
                    let mut mode = 0;
                    if unsafe { GetConsoleMode(handle.as_raw_handle(), &mut mode) } == 0
                        && unsafe { GetFileType(handle.as_raw_handle()) } == FILE_TYPE_CHAR
                    {
                        handle
                    } else {
                        open_console(path)?
                    }
                }
                None => open_console(path)?,
            },
        )?;
    }
    Ok(())
}

/// A double-clicked GUI frontend can fail before interactive console setup.
/// Keep those failures visible without overriding explicit stderr redirection.
/// The caller excludes silent/background launches before invoking this helper.
pub fn report_startup_error(message: &str) -> bool {
    let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if !stderr.is_null()
        && stderr != INVALID_HANDLE_VALUE
        && unsafe { GetFileType(stderr) } != FILE_TYPE_UNKNOWN
    {
        return false;
    }
    let title: Vec<u16> = "Astra\0".encode_utf16().collect();
    let message: Vec<u16> = message
        .replace('\0', "�")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        ) != 0
    }
}

/// A dedicated TUI window always needs its own input and output buffers,
/// even when its launcher supplied handles for another terminal or a pipe.
pub fn attach_console_io() -> Result<()> {
    for (which, path) in STREAMS {
        install(which, open_console(path)?)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../test/windows_console_test.rs"]
mod tests;
