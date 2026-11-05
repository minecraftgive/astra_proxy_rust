//! Bounded synchronous commands for OS setting changes, including Drop cleanup.
use std::{
    io,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

pub const SYSTEM_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Spool output to files so a noisy or stuck child cannot fill a pipe and hang cleanup.
/// The caller supplies argv directly; no shell is used.
pub fn output(command: &mut Command) -> io::Result<Output> {
    output_with_timeout(command, SYSTEM_COMMAND_TIMEOUT)
}
fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    use io::{Read, Seek};
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => error,
                    _ => io::Error::new(
                        io::ErrorKind::TimedOut,
                        "system command exceeded its deadline",
                    ),
                });
            }
        }
    };
    stdout.rewind()?;
    stderr.rewind()?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.take(1024 * 1024).read_to_end(&mut out)?;
    stderr.take(1024 * 1024).read_to_end(&mut err)?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

#[cfg(test)]
#[path = "test/command_test.rs"]
mod tests;
