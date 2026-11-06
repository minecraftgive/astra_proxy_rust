use super::*;
use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};

#[test]
fn registered_service_is_preferred_for_interactive_and_silent_requests() {
    for allow_uac in [false, true] {
        let method = LaunchMethod::preferred_with(allow_uac, || Ok(true)).unwrap();
        assert_eq!(method, LaunchMethod::Service);
        assert_eq!(
            method
                .spawn_with(|| Ok("service"), || panic!("UAC must not be called"))
                .unwrap(),
            "service"
        );
    }
}

#[test]
fn only_unregistered_interactive_requests_may_use_uac() {
    assert_eq!(
        LaunchMethod::preferred_with(false, || Ok(false))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let method = LaunchMethod::preferred_with(true, || Ok(false)).unwrap();
    assert_eq!(method, LaunchMethod::Uac);
    assert_eq!(
        method
            .spawn_with(|| panic!("No service was registered"), || Ok("uac"))
            .unwrap(),
        "uac"
    );
}

#[test]
fn service_query_failure_never_grants_permission_to_launch_or_prompt() {
    for allow_uac in [false, true] {
        let result: io::Result<()> =
            LaunchMethod::preferred_with(allow_uac, || Err(io::Error::from_raw_os_error(5)))
                .and_then(|method| {
                    method.spawn_with(
                        || panic!("A failed service query must not launch a core"),
                        || panic!("A failed service query must not trigger UAC"),
                    )
                });
        assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
    }
}

#[test]
fn registered_service_failures_do_not_retry_through_uac() {
    for allow_uac in [false, true] {
        for (kind, reason) in [
            (io::ErrorKind::NotFound, "Service removed after selection"),
            (
                io::ErrorKind::PermissionDenied,
                "Broker authentication failed",
            ),
            (io::ErrorKind::TimedOut, "Stopped service did not start"),
            (io::ErrorKind::Other, "Installed proxy snapshot differs"),
            (io::ErrorKind::InvalidInput, "Unsupported executable"),
        ] {
            let method = LaunchMethod::preferred_with(allow_uac, || Ok(true)).unwrap();
            let result: io::Result<()> = method.spawn_with(
                || Err(io::Error::new(kind, reason)),
                || panic!("Service failure must never trigger interactive approval"),
            );
            let error = result.unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), reason);
        }
    }
}

#[test]
fn resident_mode_is_explicit_and_legacy_elevation_defaults_off() {
    assert!(!hosted_argument(None).unwrap());
    assert!(!hosted_argument(Some(&OsString::from("0"))).unwrap());
    assert!(hosted_argument(Some(&OsString::from("1"))).unwrap());
    assert!(hosted_argument(Some(&OsString::from("unexpected"))).is_err());
}

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
            detached: false,
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
fn user_writable_program_is_allowed_with_warning() {
    let exe = std::env::temp_dir().join(format!("astra-elevation-{}.exe", std::process::id()));
    std::fs::write(&exe, b"").unwrap();
    assert_eq!(
        ensure_admin_controlled(&exe).unwrap(),
        strip_verbatim(std::fs::canonicalize(&exe).unwrap())
    );
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
