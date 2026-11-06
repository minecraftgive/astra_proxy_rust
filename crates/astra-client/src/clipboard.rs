//! Write plain text to the desktop clipboard without evaluating it as shell code.
use anyhow::{ensure, Context, Result};
use std::{
    io::Write,
    process::{Command, Stdio},
};

pub fn copy(text: &str) -> Result<()> {
    #[cfg(windows)]
    {
        // A BOM makes clip.exe interpret stdin as Unicode, independent of the
        // console's active code page. No shell or PowerShell interpolation occurs.
        let bytes: Vec<u8> = std::iter::once(0xfeff_u16)
            .chain(text.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        write_to("clip.exe", &[], &bytes)
    }
    #[cfg(target_os = "macos")]
    {
        write_to("pbcopy", &[], text.as_bytes())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let mut errors = Vec::new();
        for (program, args) in [
            ("wl-copy", &["--type", "text/plain;charset=utf-8"][..]),
            ("xclip", &["-selection", "clipboard", "-in"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            match write_to(program, args, text.as_bytes()) {
                Ok(()) => return Ok(()),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        anyhow::bail!(
            "Desktop clipboard unavailable; install wl-copy, xclip or xsel and connect to a desktop session: {}",
            errors.join("; ")
        )
    }
}

fn write_to(program: &str, args: &[&str], bytes: &[u8]) -> Result<()> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // Linux clipboard owners may fork and retain their output handles.
        // Do not await pipe EOF from the long-lived clipboard owner.
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot start {program}"))?;
    let written = child
        .stdin
        .take()
        .context("Clipboard helper has no input pipe")?
        .write_all(bytes);
    // Closing stdin lets the helper finish; always reap it, including write errors.
    let status = child
        .wait()
        .with_context(|| format!("Cannot wait for {program}"))?;
    ensure!(status.success(), "{program} failed ({status})");
    written.with_context(|| format!("Cannot send text to {program}"))
}
