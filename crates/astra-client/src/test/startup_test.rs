use super::*;

fn context() -> Context {
    Context {
        executable: r"C:\Astra & tools\astra-tui.exe".into(),
        home: r"C:\Users\用户\state".into(),
        config: r"D:\配置 $() & %PATH%\config".into(),
        bin: r"C:\Astra & tools\release".into(),
        language: crate::i18n::Language::Simplified,
    }
}

fn parse_arguments(arguments: &OsStr) -> Vec<OsString> {
    use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};
    let mut command: Vec<_> = "astra-tui.exe ".encode_utf16().collect();
    command.extend(arguments.encode_wide());
    command.push(0);
    let mut count = 0;
    let pointer = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
    assert!(!pointer.is_null());
    let mut parsed = Vec::new();
    for index in 1..count as usize {
        unsafe {
            let arg = *pointer.add(index);
            let mut length = 0;
            while *arg.add(length) != 0 {
                length += 1;
            }
            parsed.push(OsString::from_wide(std::slice::from_raw_parts(arg, length)));
        }
    }
    unsafe { LocalFree(pointer.cast()) };
    parsed
}

#[test]
fn windows_arguments_round_trip_spaces_quotes_backslashes_and_unicode() {
    let arguments: Vec<OsString> = [
        "",
        "plain",
        "two words",
        "tabs\there",
        r"C:\",
        r"D:\目录 space\",
        "a\"b",
        "a\\\"b\\",
        "& | ; $() %PATH% ` '中文'",
    ]
    .map(OsString::from)
    .into();
    assert_eq!(
        parse_arguments(&quote_arguments(&arguments).unwrap()),
        arguments
    );
    assert!(quote_arguments(&[OsString::from_wide(&[b'a' as u16, 0, b'b' as u16])]).is_err());
}

#[test]
fn startup_preserves_storage_and_target_without_detached_window_overrides() {
    let context = context();
    let endpoint = PathBuf::from(r"C:\controller space\management.sock");
    let target = Some((endpoint.clone(), Some("agent \"one\" & two".into())));
    let arguments = parse_arguments(&arguments(&context, &target, false).unwrap());
    for (name, path) in [
        ("--state-dir", &context.home),
        ("--config-dir", &context.config),
        ("--bin-dir", &context.bin),
        ("--endpoint", &endpoint),
    ] {
        assert!(arguments
            .windows(2)
            .any(|pair| { pair == [OsString::from(name), path.as_os_str().to_owned()] }));
    }
    assert!(arguments.windows(2).any(|pair| {
        pair == [
            OsString::from("--agent"),
            OsString::from("agent \"one\" & two"),
        ]
    }));
    assert!(arguments.contains(&OsString::from("--native-console")));
    for forbidden in ["--view", "--window-id", "--instance", "--lang", "--manager"] {
        assert!(!arguments.contains(&OsString::from(forbidden)));
    }
    let local = parse_arguments(&super::arguments(&context, &None, false).unwrap());
    assert!(!local.contains(&OsString::from("--endpoint")));
    let silent = parse_arguments(&super::arguments(&context, &target, true).unwrap());
    assert_eq!(
        silent,
        arguments
            .into_iter()
            .map(|argument| if argument == "--native-console" {
                OsString::from("--silent")
            } else {
                argument
            })
            .collect::<Vec<_>>()
    );
}

#[test]
fn shortcut_identity_survives_updates_and_isolates_frontends_and_targets() {
    let original = context();
    let name = shortcut_name(&original, &None);
    assert_eq!(name.len(), "Astra TUI ".len() + 32 + ".lnk".len());
    let mut updated = original.clone();
    updated.executable = r"D:\Astra releases\v2\astra-tui.exe".into();
    updated.bin = r"D:\Astra releases\v2".into();
    assert_eq!(shortcut_name(&updated, &None), name);
    updated.home = r"D:\another-state".into();
    assert_ne!(shortcut_name(&updated, &None), name);
    updated = original.clone();
    updated.executable.set_file_name("astra-server-tui.exe");
    assert_ne!(shortcut_name(&updated, &None), name);
    let first = Some((r"C:\remote\management.sock".into(), Some("one".into())));
    let second = Some((r"C:\remote\management.sock".into(), Some("two".into())));
    assert_ne!(shortcut_name(&original, &first), name);
    assert_ne!(
        shortcut_name(&original, &first),
        shortcut_name(&original, &second)
    );
}

#[test]
fn extended_paths_are_compatible_with_windows_shell() {
    for (input, output) in [
        (
            r"\\?\C:\Astra space\astra-tui.exe",
            r"C:\Astra space\astra-tui.exe",
        ),
        (r"\\?\UNC\server\share\state", r"\\server\share\state"),
        (r"C:\Users\用户\state", r"C:\Users\用户\state"),
    ] {
        assert_eq!(shell_path(Path::new(input)), OsString::from(output));
    }
}

#[test]
fn native_shortcut_values_preserve_unicode_and_reject_embedded_nul() {
    let value = OsStr::new(r"C:\测试 ' ; $()\startup.lnk");
    let converted = wide(value).unwrap();
    assert_eq!(converted.last(), Some(&0));
    assert_eq!(
        OsString::from_wide(&converted[..converted.len() - 1]),
        value
    );
    assert!(wide(&OsString::from_wide(&[42, 0, 12])).is_err());
}

#[test]
fn shortcut_com_round_trip_in_temporary_directory_only() {
    use ::windows::{
        core::{Interface, PCWSTR},
        Win32::{
            System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ},
            UI::{
                Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH},
                WindowsAndMessaging::SW_SHOWNORMAL,
            },
        },
    };
    // This does not call startup_directory/set_enabled or touch login settings.
    let temporary = TemporaryShortcut::new(&std::env::temp_dir()).unwrap();
    let context = Context {
        executable: temporary.0.join("astra-tui.exe"),
        ..context()
    };
    // Only target-path metadata is needed: these files are never executed.
    std::fs::write(&context.executable, b"shortcut target placeholder").unwrap();
    let target = Some((
        r"C:\remote space\management.sock".into(),
        Some("node & one".into()),
    ));
    assert!(!shortcut_exists(&temporary.path()).unwrap());
    let _apartment = ComApartment::new().unwrap();
    let path = wide(&shell_path(&temporary.path())).unwrap();
    for silent in [false, true] {
        create_shortcut(&temporary.path(), &context, &target, silent).unwrap();
        assert!(shortcut_exists(&temporary.path()).unwrap());
        let mut arg_buffer = vec![0; 32_768];
        let mut exe_buffer = vec![0; 32_768];
        let mut dir_buffer = vec![0; 32_768];
        unsafe {
            let shortcut: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            let file: IPersistFile = shortcut.cast().unwrap();
            file.Load(PCWSTR(path.as_ptr()), STGM_READ).unwrap();
            shortcut.GetArguments(&mut arg_buffer).unwrap();
            shortcut
                .GetPath(&mut exe_buffer, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
                .unwrap();
            shortcut.GetWorkingDirectory(&mut dir_buffer).unwrap();
            assert_eq!(shortcut.GetShowCmd().unwrap(), SW_SHOWNORMAL);
        }
        let decoded = |buffer: &[u16]| {
            OsString::from_wide(&buffer[..buffer.iter().position(|unit| *unit == 0).unwrap()])
        };
        assert_eq!(
            decoded(&arg_buffer),
            arguments(&context, &target, silent).unwrap()
        );
        assert_eq!(decoded(&exe_buffer), shell_path(&context.executable));
        assert_eq!(
            decoded(&dir_buffer),
            shell_path(context.executable.parent().unwrap())
        );
    }
    std::fs::remove_file(context.executable).unwrap();
}

#[test]
fn graphical_startup_does_not_request_a_terminal_console() {
    let mut context = context();
    context.executable = context.executable.with_file_name("astra-gui.exe");
    let args = parse_arguments(&arguments(&context, &None, false).unwrap());
    assert!(!args.contains(&OsString::from("--native-console")));
    assert!(!args.contains(&OsString::from("--silent")));
    let silent = parse_arguments(&arguments(&context, &None, true).unwrap());
    assert!(silent.contains(&OsString::from("--silent")));
    assert_ne!(
        shortcut_name(&context, &None),
        shortcut_name(
            &context
                .for_frontend(crate::preferences::StartupFrontend::Tui)
                .unwrap(),
            &None
        )
    );
}
