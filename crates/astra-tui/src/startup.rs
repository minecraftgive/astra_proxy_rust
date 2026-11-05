//! Per-user Windows sign-in startup, using a shortcut rather than Run's
//! 260-character command-line limit. Only an explicit settings change writes it.
use crate::windows::Context;
use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

type Target = Option<(PathBuf, Option<String>)>;

/// Cheap enough for opening Settings: never creates a COM object or files.
pub(crate) fn enabled(context: &Context) -> Result<bool> {
    let path = startup_directory()?.join(shortcut_name(context, &crate::protocol::target()));
    shortcut_exists(&path)
}

pub(crate) fn set_enabled(context: &Context, enabled: bool, silent: bool) -> Result<()> {
    let target = crate::protocol::target();
    let path = startup_directory()?.join(shortcut_name(context, &target));
    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Remove Windows sign-in shortcut"),
        };
    }

    // Build outside the Startup folder, then atomically publish a complete link.
    // A failed COM operation leaves any previously working shortcut untouched.
    let temporary = TemporaryShortcut::new(&context.home)?;
    create_shortcut(&temporary.path(), context, &target, silent)?;
    let bytes = std::fs::read(temporary.path()).context("Read Windows sign-in shortcut")?;
    anyhow::ensure!(
        !bytes.is_empty(),
        "Windows created an empty sign-in shortcut"
    );
    std::fs::create_dir_all(path.parent().context("Missing Startup directory")?)?;
    astra_platform::storage::atomic_write_default(&path, &bytes)
        .context("Save Windows sign-in shortcut")
}

fn shortcut_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("Inspect Windows sign-in shortcut"),
    }
}

fn startup_directory() -> Result<PathBuf> {
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Startup, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY},
    };
    let mut pointer = std::ptr::null_mut();
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Startup,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut pointer,
        )
    };
    let result = if status < 0 || pointer.is_null() {
        Err(anyhow::anyhow!(
            "Cannot locate Windows Startup directory (HRESULT 0x{:08X})",
            status as u32
        ))
    } else {
        let mut length = 0;
        unsafe {
            while *pointer.add(length) != 0 {
                length += 1;
            }
            Ok(PathBuf::from(OsString::from_wide(
                std::slice::from_raw_parts(pointer, length),
            )))
        }
    };
    unsafe { CoTaskMemFree(pointer.cast()) };
    result
}

fn shortcut_name(context: &Context, target: &Target) -> String {
    let mut hash = Sha256::new();
    let mut add = |value: &OsStr| {
        for unit in value.encode_wide() {
            hash.update(unit.to_le_bytes());
        }
        hash.update(0u16.to_le_bytes());
    };
    add(&shell_path(&context.home));
    // New release directories replace the existing entry, rather than adding a
    // second login window. The client and server frontends remain independent.
    add(context.executable.file_name().unwrap_or_default());
    if let Some((endpoint, agent)) = target {
        add(&shell_path(endpoint));
        add(OsStr::new(agent.as_deref().unwrap_or_default()));
    }
    let digest = format!("{:x}", hash.finalize());
    format!("Astra TUI {}.lnk", &digest[..32])
}

/// Shell COM expects ordinary DOS/UNC paths, whereas canonicalize returns the
/// extended-length spelling. Keep UTF-16 intact, including non-Unicode names.
fn shell_path(path: &Path) -> OsString {
    let wide: Vec<_> = path.as_os_str().encode_wide().collect();
    let prefix: Vec<_> = r"\\?\".encode_utf16().collect();
    let unc: Vec<_> = r"UNC\".encode_utf16().collect();
    if let Some(tail) = wide.strip_prefix(&prefix[..]) {
        if let Some(tail) = tail.strip_prefix(&unc[..]) {
            return OsString::from_wide(&[&[b'\\' as u16, b'\\' as u16], tail].concat());
        }
        if tail.get(1) == Some(&(b':' as u16)) {
            return OsString::from_wide(tail);
        }
    }
    path.as_os_str().into()
}

fn arguments(context: &Context, target: &Target, silent: bool) -> Result<OsString> {
    let mut args = vec![
        OsString::from("--state-dir"),
        shell_path(&std::path::absolute(&context.home)?),
        OsString::from("--config-dir"),
        shell_path(&std::path::absolute(&context.config)?),
        OsString::from("--bin-dir"),
        shell_path(&std::path::absolute(&context.bin)?),
    ];
    if !silent {
        args.push(OsString::from("--native-console"));
    }
    if let Some((endpoint, agent)) = target {
        args.extend([
            OsString::from("--endpoint"),
            shell_path(&std::path::absolute(endpoint)?),
        ]);
        if let Some(agent) = agent {
            args.extend([OsString::from("--agent"), agent.into()]);
        }
    }
    // No --lang, --view or --window-id: use saved preferences and the primary
    // window's session, not the page/language of the window toggling this option.
    quote_arguments(&args)
}

/// Windows argv quoting; there is no cmd.exe, PowerShell or percent expansion
/// when the shortcut launches the executable. Double trailing backslashes too.
fn quote_arguments(args: &[OsString]) -> Result<OsString> {
    let mut output = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index != 0 {
            output.push(b' ' as u16);
        }
        output.push(b'"' as u16);
        let mut slashes = 0;
        for unit in arg.encode_wide() {
            anyhow::ensure!(unit != 0, "Startup arguments cannot contain NUL");
            if unit == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            if unit == b'"' as u16 {
                output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
            } else {
                output.extend(std::iter::repeat_n(b'\\' as u16, slashes));
            }
            slashes = 0;
            output.push(unit);
        }
        output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        output.push(b'"' as u16);
    }
    anyhow::ensure!(
        output.len() < 32_000,
        "Windows startup arguments are too long"
    );
    Ok(OsString::from_wide(&output))
}

fn shortcut_executable(context: &Context, silent: bool) -> Result<PathBuf> {
    let executable = std::path::absolute(&context.executable)?;
    if !silent {
        return Ok(executable);
    }
    let helper = executable.with_file_name("astra-tui-startup.exe");
    anyhow::ensure!(
        helper.is_file(),
        "Silent startup requires astra-tui-startup.exe beside the TUI; install a complete package ({})",
        helper.display()
    );
    Ok(helper)
}

fn create_shortcut(path: &Path, context: &Context, target: &Target, silent: bool) -> Result<()> {
    // A dedicated apartment avoids changing the UI executor thread's COM mode.
    // Use IShellLinkW directly: WScript.Shell corrupts non-ANSI arguments even
    // when its automation properties receive Unicode strings.
    std::thread::scope(|scope| {
        scope
            .spawn(|| create_shortcut_in_apartment(path, context, target, silent))
            .join()
            .map_err(|_| anyhow::anyhow!("Windows shortcut worker panicked"))?
    })
}

fn create_shortcut_in_apartment(
    path: &Path,
    context: &Context,
    target: &Target,
    silent: bool,
) -> Result<()> {
    use ::windows::{
        core::{w, Interface, PCWSTR},
        Win32::{
            System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER},
            UI::{
                Shell::{IShellLinkW, ShellLink},
                WindowsAndMessaging::SW_SHOWNORMAL,
            },
        },
    };
    let _apartment = ComApartment::new()?;
    let executable = shortcut_executable(context, silent)?;
    let directory = wide(&shell_path(
        executable
            .parent()
            .context("Missing executable directory")?,
    ))?;
    let executable = wide(&shell_path(&executable))?;
    let arguments = wide(&arguments(context, target, silent)?)?;
    let destination = wide(&shell_path(path))?;
    unsafe {
        let shortcut: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("Create Windows sign-in shortcut")?;
        shortcut.SetPath(PCWSTR(executable.as_ptr()))?;
        shortcut.SetArguments(PCWSTR(arguments.as_ptr()))?;
        shortcut.SetWorkingDirectory(PCWSTR(directory.as_ptr()))?;
        shortcut.SetDescription(w!("Astra TUI - start at Windows sign-in"))?;
        // SetShowCmd only supports normal/maximized/minimized, not SW_HIDE.
        // The silent helper uses the GUI subsystem and never creates a window.
        shortcut.SetShowCmd(SW_SHOWNORMAL)?;
        let file: IPersistFile = shortcut.cast()?;
        file.Save(PCWSTR(destination.as_ptr()), true)
            .context("Write Windows sign-in shortcut")?;
    }
    Ok(())
}

fn wide(value: &OsStr) -> Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    anyhow::ensure!(!value.contains(&0), "Startup values cannot contain NUL");
    value.push(0);
    Ok(value)
}

struct ComApartment;
impl ComApartment {
    fn new() -> Result<Self> {
        use ::windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .context("Initialize Windows shortcut support")?;
        Ok(Self)
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { ::windows::Win32::System::Com::CoUninitialize() };
    }
}

struct TemporaryShortcut(PathBuf);
impl TemporaryShortcut {
    fn new(parent: &Path) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            ".startup-{}-{}-{}",
            std::process::id(),
            astra_ipc::progress::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        );
        let directory = parent.join(name);
        std::fs::create_dir(&directory).context("Prepare Windows sign-in shortcut")?;
        Ok(Self(directory))
    }
    fn path(&self) -> PathBuf {
        self.0.join("startup.lnk")
    }
}
impl Drop for TemporaryShortcut {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path());
        let _ = std::fs::remove_dir(&self.0);
    }
}

#[cfg(test)]
#[path = "test/startup_test.rs"]
mod tests;
