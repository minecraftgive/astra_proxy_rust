use super::*;

const OWNER: &str = "S-1-5-21-123456-234567-345678-1001";

#[test]
fn service_identity_cannot_escape_install_directory_or_inject_sddl() {
    validate_sid_shape(OWNER).unwrap();
    validate_sid_shape("S-1-12-1-1234-2345-3456-4567").unwrap();
    for invalid in [
        "",
        "S-1-5-18",
        "S-1-5-21-1/../../x",
        "S-1-5-21-1)(A;;GA;;;WD)",
        "S-1-5-21--1",
        "S-1-5-21-中文",
    ] {
        assert!(validate_sid_shape(invalid).is_err(), "{invalid}");
    }
    assert_eq!(service_name(OWNER), format!("AstraPrivilege-{OWNER}"));
    assert_eq!(
        pipe_name(OWNER),
        format!(r"\\.\pipe\AstraPrivilege-{OWNER}")
    );
    assert_ne!(
        service_name(OWNER),
        service_name("S-1-5-21-123456-234567-345678-1002")
    );
}

#[test]
fn owner_service_rights_exclude_reconfiguration_and_shutdown() {
    let sddl = String::from_utf16(&service_dacl(OWNER).unwrap()).unwrap();
    assert!(sddl.contains(&format!("(A;;0x00020095;;;{OWNER})")));
    let rights = OWNER_SERVICE_ACCESS;
    assert_ne!(rights & SERVICE_START, 0);
    assert_ne!(rights & SERVICE_QUERY_STATUS, 0);
    assert_eq!(
        rights & (SERVICE_CHANGE_CONFIG | SERVICE_STOP | DELETE | WRITE_DAC | 0x00080000),
        0
    );
    // Verify the actual Windows SDDL parser accepts our narrow descriptor.
    let descriptor = service_dacl(OWNER).unwrap();
    let mut pointer = null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor.as_ptr(),
                1,
                &mut pointer,
                null_mut(),
            )
        },
        0
    );
    unsafe { LocalFree(pointer) };
}

#[test]
fn service_arguments_round_trip_unicode_metacharacters_and_trailing_slashes() {
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
    let values = [
        OsStr::new(r"C:\Program Files\Astra Privilege Service\astra-service.exe"),
        OsStr::new("--service"),
        OsStr::new("--owner"),
        OsStr::new(OWNER),
        OsStr::new("中文 & $() \"quoted\"\\"),
    ];
    let command = command_line(&values).unwrap();
    let mut count = 0;
    let pointer = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
    assert!(!pointer.is_null());
    assert_eq!(count as usize, values.len());
    for (index, expected) in values.into_iter().enumerate() {
        unsafe {
            let value = *pointer.add(index);
            let mut length = 0;
            while *value.add(length) != 0 {
                length += 1;
            }
            assert_eq!(
                OsString::from_wide(std::slice::from_raw_parts(value, length)),
                expected
            );
        }
    }
    unsafe { LocalFree(pointer.cast()) };
    assert!(command_line(&[OsStr::new("bad\0value")]).is_err());
    assert_eq!(
        shell_path(Path::new(r"\\?\C:\中文\service.exe")),
        PathBuf::from(r"C:\中文\service.exe")
    );
    assert_eq!(
        shell_path(Path::new(r"\\?\UNC\server\share\service.exe")),
        PathBuf::from(r"\\server\share\service.exe")
    );
}

#[test]
fn framed_protocol_is_bounded_and_rejects_truncation() {
    let request = SpawnRequest {
        args: vec!["--windows-elevated".into(), "中文 路径".into()],
    };
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &request).unwrap();
    let decoded: SpawnRequest = read_frame(&mut &bytes[..]).unwrap();
    assert_eq!(decoded.args, request.args);
    assert!(read_frame::<SpawnRequest>(&mut &bytes[..bytes.len() - 1]).is_err());
    assert!(read_frame::<SpawnRequest>(&mut &0u32.to_le_bytes()[..]).is_err());
    assert!(read_frame::<SpawnRequest>(&mut &((MAX_FRAME + 1) as u32).to_le_bytes()[..]).is_err());
    let oversized = SpawnRequest {
        args: vec!["x".repeat(MAX_FRAME)],
    };
    assert!(write_frame(&mut Vec::new(), &oversized).is_err());
    let response = SpawnResponse {
        handle: None,
        error: Some("permission denied".into()),
    };
    let mut encoded = Vec::new();
    write_frame(&mut encoded, &response).unwrap();
    let response: SpawnResponse = read_frame(&mut &encoded[..]).unwrap();
    assert!(response.handle.is_none());
    assert_eq!(response.error.as_deref(), Some("permission denied"));
}

#[test]
fn proxy_snapshot_comparison_detects_same_size_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.exe");
    let installed = directory.path().join("installed.exe");
    let bytes = vec![42; 150_000];
    std::fs::write(&source, &bytes).unwrap();
    std::fs::write(&installed, &bytes).unwrap();
    assert!(same_contents(&source, &installed).unwrap());
    let mut changed = bytes;
    changed[70_000] = 13;
    std::fs::write(&source, &changed).unwrap();
    assert!(!same_contents(&source, &installed).unwrap());
    std::fs::write(&source, b"short").unwrap();
    assert!(!same_contents(&source, &installed).unwrap());
}

#[test]
fn service_spawn_rejects_arbitrary_program_before_contacting_scm() {
    let result = spawn(Path::new(r"C:\Windows\System32\cmd.exe"), &[]);
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::InvalidInput));
}
