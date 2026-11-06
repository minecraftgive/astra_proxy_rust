//! One-shot installation approval. This does not select or contact the TUN service.
use anyhow::{Context, Result};
use std::{
    ffi::OsString,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, Prefix},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::Threading::{GetExitCodeProcess, WaitForSingleObject},
    UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
};

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

// ShellExecute requires ordinary drive/UNC paths. Preserve the exact UTF-16
// filename when removing the canonicalization-only verbatim prefix.
fn shell_program(program: &Path) -> Result<Vec<u16>> {
    let mut value: Vec<u16> = program.as_os_str().encode_wide().collect();
    anyhow::ensure!(!value.contains(&0), "Invalid installation helper path");
    match program.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(_) => {
                value.drain(..4);
            }
            Prefix::VerbatimUNC(_, _) => {
                value.splice(..8, [b'\\' as u16, b'\\' as u16]);
            }
            _ => {}
        },
        _ => {}
    }
    value.push(0);
    Ok(value)
}

pub(crate) fn elevate(program: &Path, arguments: &[String]) -> Result<()> {
    let program = std::fs::canonicalize(program)?;
    anyhow::ensure!(
        program.is_file(),
        "Installation helper is not a regular file"
    );
    let program = shell_program(&program)?;
    let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
    let arguments: Vec<u16> = crate::startup::quote_arguments(&arguments)?
        .encode_wide()
        .chain(Some(0))
        .collect();
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of_val(&info) as u32;
    info.fMask = SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = program.as_ptr();
    info.lpParameters = arguments.as_ptr();
    info.nShow = 0;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context(crate::i18n::t(
            "Installation was cancelled or administrator permission was not granted.",
        ));
    }
    anyhow::ensure!(
        !info.hProcess.is_null(),
        "Installation helper returned no process handle"
    );
    let process = Process(info.hProcess);
    anyhow::ensure!(
        unsafe { WaitForSingleObject(process.0, u32::MAX) } == WAIT_OBJECT_0,
        "Cannot wait for installation helper"
    );
    let mut code = 0;
    anyhow::ensure!(
        unsafe { GetExitCodeProcess(process.0, &mut code) } != 0,
        "Cannot read installation helper result"
    );
    anyhow::ensure!(code == 0, "Installation helper exited with code {code}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_shell_paths_preserve_unicode_and_support_unc() {
        for (input, expected) in [
            (r"\\?\C:\安装\astra-tui.exe", r"C:\安装\astra-tui.exe"),
            (
                r"\\?\UNC\目标\共享\astra-tui.exe",
                r"\\目标\共享\astra-tui.exe",
            ),
            (r"C:\安装\astra-tui.exe", r"C:\安装\astra-tui.exe"),
        ] {
            let actual = shell_program(Path::new(input)).unwrap();
            assert_eq!(
                actual,
                expected.encode_utf16().chain(Some(0)).collect::<Vec<_>>()
            );
        }
    }
}
