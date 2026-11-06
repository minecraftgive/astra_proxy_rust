use super::*;
use std::{
    io::{IsTerminal, Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0},
    System::{
        Console::{
            FlushConsoleInputBuffer, WriteConsoleInputW, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT,
            KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
        },
        Threading::{
            CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
            DETACHED_PROCESS, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTF_USESTDHANDLES,
            STARTUPINFOW,
        },
    },
};

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astra-console-{}-{}-{}",
            std::process::id(),
            astra_ipc::progress::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if let (Ok(parent), Ok(target)) = (
            std::fs::canonicalize(std::env::temp_dir()),
            std::fs::canonicalize(&self.0),
        ) {
            assert_eq!(target, parent.join(self.0.file_name().unwrap()));
            let _ = std::fs::remove_dir_all(target);
        }
    }
}
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn probe(mode: &str) -> (serde_json::Value, String, String) {
    probe_with_console(mode, false)
}

fn probe_with_console(mode: &str, inherit_console: bool) -> (serde_json::Value, String, String) {
    let temporary = Temporary::new();
    std::fs::write(temporary.0.join("request"), mode).unwrap();
    std::fs::write(temporary.0.join("stdin"), "input-from-parent").unwrap();
    let input = std::fs::File::open(temporary.0.join("stdin")).unwrap();
    let output = std::fs::File::create(temporary.0.join("stdout")).unwrap();
    let error = std::fs::File::create(temporary.0.join("stderr")).unwrap();
    let handles = if inherit_console {
        STREAMS.map(|(which, _)| unsafe { GetStdHandle(which) })
    } else {
        [
            input.as_raw_handle(),
            output.as_raw_handle(),
            error.as_raw_handle(),
        ]
    };
    for handle in handles {
        assert_ne!(
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) },
            0
        );
    }
    let executable = std::env::current_exe().unwrap();
    let application = wide(&executable);
    let mut command = wide(format!(
        "\"{}\" --exact windows::console::tests::console_child --ignored --nocapture",
        executable.display()
    ));
    let directory = wide(&temporary.0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW | STARTF_USESTDHANDLES,
        // AllocConsole honours the child's initial STARTUPINFO too. This
        // verifies real console I/O without ever showing a test window.
        wShowWindow: 0,
        hStdInput: handles[0],
        hStdOutput: handles[1],
        hStdError: handles[2],
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    assert_ne!(
        unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                DETACHED_PROCESS,
                std::ptr::null(),
                directory.as_ptr(),
                &startup,
                &mut process,
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    let child = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
    drop(unsafe { OwnedHandle::from_raw_handle(process.hThread) });
    let wait = unsafe { WaitForSingleObject(child.as_raw_handle(), 20_000) };
    if wait != WAIT_OBJECT_0 {
        unsafe { TerminateProcess(child.as_raw_handle(), 99) };
        unsafe { WaitForSingleObject(child.as_raw_handle(), 5_000) };
        panic!("Console probe timed out: {mode}");
    }
    let mut code = 0;
    assert_ne!(
        unsafe { GetExitCodeProcess(child.as_raw_handle(), &mut code) },
        0
    );
    let stdout = std::fs::read_to_string(temporary.0.join("stdout")).unwrap();
    let stderr = std::fs::read_to_string(temporary.0.join("stderr")).unwrap();
    assert_eq!(code, 0, "{mode}: {stdout}\n{stderr}");
    let result =
        serde_json::from_slice(&std::fs::read(temporary.0.join("result.json")).unwrap()).unwrap();
    (result, stdout, stderr)
}

#[test]
fn command_line_console_preserves_redirected_standard_streams() {
    let (result, stdout, stderr) = probe("command-line");
    assert_eq!(result["input"], "input-from-parent");
    assert_eq!(result["stdin"], false);
    assert_eq!(result["stdout"], false);
    assert_eq!(result["stderr"], false);
    assert!(stdout.contains("stdout-from-child"));
    assert!(stderr.contains("stderr-from-child"));
}

#[test]
fn interactive_console_is_independent_and_accepts_crossterm_keyboard_input() {
    for mode in ["interactive", "new-window"] {
        let (result, _, _) = probe(mode);
        assert_eq!(result["stdin"], true);
        assert_eq!(result["stdout"], true);
        assert_eq!(result["stderr"], true);
        assert_eq!(result["console_processes"], 1);
        assert_eq!(result["own_console"], true);
        assert_eq!(result["key"], "Char('a')");
        assert!(result["width"].as_u64().unwrap() > 0);
        assert!(result["height"].as_u64().unwrap() > 0);
    }
}

#[test]
fn inherited_character_handles_are_classified_after_attaching_to_parent_console() {
    let (attached, _, _) = probe("shared-command-line");
    assert_eq!(attached["before_console"], 0);
    assert_eq!(attached["stdin"], true);
    assert_eq!(attached["stdout"], true);
    assert_eq!(attached["stderr"], true);
    assert_eq!(attached["console_processes"], 2);
    let (independent, _, _) = probe("shared-interactive");
    assert_eq!(independent["before_console"], 0);
    assert_eq!(independent["console_processes"], 1);
    assert_eq!(independent["own_console"], true);
    assert_eq!(independent["key"], "Char('a')");
    let (nul, _, _) = probe("shared-nul");
    assert_eq!(nul["stdin"], true);
    assert_eq!(nul["stdout"], false);
    assert_eq!(nul["stderr"], true);
    assert_eq!(nul["console_processes"], 2);
}

#[test]
fn hosted_console_reuses_the_waiting_terminal_and_accepts_keyboard_input() {
    let (hosted, _, _) = probe("shared-hosted");
    assert_eq!(hosted["before_console"], 0);
    assert_eq!(hosted["console_processes"], 2);
    assert_eq!(hosted["stdin"], true);
    assert_eq!(hosted["stdout"], true);
    assert_eq!(hosted["stderr"], true);
    assert_eq!(hosted["key"], "Char('a')");
}

#[test]
#[ignore = "child probe invoked by isolated native-process tests"]
fn console_child() {
    let mode = std::fs::read_to_string("request").unwrap();
    let mut result = serde_json::json!({});
    let mut processes = [0u32; 4];
    result["before_console"] = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 4) }.into();
    if mode.starts_with("shared-") {
        // A hidden parent console supplies genuine CONIN$/CONOUT$ handles to
        // a detached child, reproducing GUI startup before AttachConsole.
        prepare_console(ConsoleMode::Interactive).unwrap();
        if mode == "shared-nul" {
            let nul = OpenOptions::new().write(true).open("NUL").unwrap();
            install(STD_OUTPUT_HANDLE, nul.into()).unwrap();
        }
        let child_mode = mode.replacen("shared-", "inherited-", 1);
        let (child, _, _) = probe_with_console(&child_mode, true);
        std::fs::write("result.json", serde_json::to_vec(&child).unwrap()).unwrap();
        return;
    } else if mode == "command-line" {
        prepare_console(ConsoleMode::CommandLine).unwrap();
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap();
        result["input"] = input.into();
        println!("stdout-from-child");
        eprintln!("stderr-from-child");
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
    } else if mode == "inherited-command-line" || mode == "inherited-nul" {
        prepare_console(ConsoleMode::CommandLine).unwrap();
        result["console_processes"] =
            unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 4) }.into();
        assert!(!report_startup_error(
            "This must stay on the usable stderr stream"
        ));
        eprintln!("inherited-console-stderr");
    } else {
        // Simulate preliminary CLI preparation before full argument parsing.
        prepare_console(ConsoleMode::CommandLine).unwrap();
        prepare_console(if mode == "inherited-hosted" {
            ConsoleMode::Hosted
        } else if mode == "new-window" {
            ConsoleMode::NewWindow
        } else {
            ConsoleMode::Interactive
        })
        .unwrap();
        let mut processes = [0u32; 4];
        let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 4) };
        result["console_processes"] = count.into();
        result["own_console"] = (processes[0] == std::process::id()).into();
        let (width, height) = crossterm::terminal::size().unwrap();
        result["width"] = width.into();
        result["height"] = height.into();
        crossterm::terminal::enable_raw_mode().unwrap();
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        assert_ne!(unsafe { FlushConsoleInputBuffer(input) }, 0);
        let event = INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: 1,
                    wRepeatCount: 1,
                    wVirtualKeyCode: b'A' as u16,
                    uChar: KEY_EVENT_RECORD_0 {
                        UnicodeChar: b'a' as u16,
                    },
                    ..Default::default()
                },
            },
        };
        let mut written = 0;
        assert_ne!(
            unsafe { WriteConsoleInputW(input, &event, 1, &mut written) },
            0
        );
        assert_eq!(written, 1);
        assert!(crossterm::event::poll(std::time::Duration::from_secs(2)).unwrap());
        let crossterm::event::Event::Key(key) = crossterm::event::read().unwrap() else {
            panic!("Expected injected key event");
        };
        result["key"] = format!("{:?}", key.code).into();
        crossterm::terminal::disable_raw_mode().unwrap();
    }
    result["stdin"] = std::io::stdin().is_terminal().into();
    result["stdout"] = std::io::stdout().is_terminal().into();
    result["stderr"] = std::io::stderr().is_terminal().into();
    std::fs::write(
        Path::new("result.json"),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
}
