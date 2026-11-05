use super::*;

fn verify_descriptor(sddl: &str, leaf: bool) -> io::Result<()> {
    let mut descriptor = null_mut();
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(OsStr::new(sddl))?.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    })?;
    let _allocation = LocalAllocation(descriptor);
    let (mut owner, mut defaulted, mut present, mut acl) = (null_mut(), 0, 0, null_mut());
    check(unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) })?;
    check(unsafe {
        GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted)
    })?;
    check_acl(owner, acl, leaf)
}

#[test]
fn read_execute_does_not_delegate_control_of_the_installed_code() {
    let user = "S-1-5-21-1-2-3-1001";
    assert!(verify_descriptor(
        &format!("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;{user})"),
        true
    )
    .is_ok());
    // Even a read-only DACL is not sufficient when the user owns the file.
    assert!(
        verify_descriptor(&format!("O:{user}D:P(A;;FA;;;SY)(A;;GRGX;;;{user})"), true).is_err()
    );
    for right in [
        "GA", "GW", "WD", "WO", "SD", "0x2", "0x4", "0x10", "0x40", "0x100",
    ] {
        assert!(
            verify_descriptor(
                &format!("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;{right};;;{user})"),
                true
            )
            .is_err(),
            "{right}"
        );
    }
}

#[test]
fn parent_creation_rights_are_distinct_from_replacing_the_protected_child() {
    let user = "S-1-5-21-1-2-3-1001";
    // Default Windows drive roots can allow users to create unrelated folders.
    assert!(verify_descriptor(&format!("O:BAD:P(A;;FA;;;BA)(A;;0x4;;;{user})"), false).is_ok());
    for right in ["WD", "WO", "SD", "0x40", "0x100"] {
        assert!(
            verify_descriptor(&format!("O:BAD:P(A;;FA;;;BA)(A;;{right};;;{user})"), false).is_err(),
            "{right}"
        );
    }
}

#[test]
fn null_acl_and_untrusted_grants_fail_closed_even_with_overlapping_denials() {
    let user = "S-1-5-21-1-2-3-1001";
    assert!(verify_descriptor("O:BAD:NO_ACCESS_CONTROL", true).is_err());
    assert!(verify_descriptor(
        &format!("O:BAD:P(D;;GA;;;{user})(A;;FA;;;BA)(A;;GA;;;{user})"),
        true
    )
    .is_err());
    assert!(verify_descriptor("O:BAD:P(A;OICIIO;FA;;;CO)(A;;FA;;;BA)", true).is_ok());
}

#[test]
fn installation_paths_reject_remote_device_and_traversal_spellings() {
    for path in [
        r"C:\Program Files\Astra\service.exe",
        r"\\?\C:\Program Files\Astra\service.exe",
    ] {
        assert!(local_absolute(Path::new(path)).is_ok(), "{path}");
    }
    for path in [
        r"service.exe",
        r"C:service.exe",
        r"C:\Astra\..\service.exe",
        r"\\server\share\service.exe",
        r"\\?\UNC\server\share\service.exe",
        r"\\.\pipe\service",
    ] {
        assert!(local_absolute(Path::new(path)).is_err(), "{path}");
    }
}

#[test]
fn user_writable_installation_and_different_enrollment_identity_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("astra-service.exe");
    std::fs::write(&executable, b"not an installed program").unwrap();
    assert!(validate_protected(&executable).is_err());
    assert!(validate_owner_sid("S-1-5-18").is_err());
    assert!(validate_owner_sid("S-1-5-21-1-2-3-1001").is_err());
    assert!(validate_owner_sid("not a SID").is_err());
}

#[test]
fn service_verification_only_grants_the_enrolled_user_query_access() {
    let user = "S-1-5-21-1-2-3-1001";
    for rights in [
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        TOKEN_QUERY,
    ] {
        let descriptor = service_read_descriptor(user, rights).unwrap();
        let (mut defaulted, mut present, mut acl) = (0, 0, null_mut());
        check(unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
        })
        .unwrap();
        assert_ne!(present, 0);
        assert!(!acl.is_null());
        assert_eq!(unsafe { (*acl).AceCount }, 3);
        let mut ace = null_mut();
        check(unsafe { GetAce(acl, 2, &mut ace) }).unwrap();
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(allowed.Mask, rights);
        let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
        assert_eq!(sid_text(sid).unwrap(), user);
        assert_eq!(
            allowed.Mask & (GENERIC_ALL | GENERIC_WRITE | WRITE_DAC | WRITE_OWNER),
            0
        );
    }
}

#[test]
fn shared_installation_ancestors_remain_readable_by_other_enrolled_accounts() {
    let descriptor = install_descriptor("S-1-5-21-1-2-3-1001").unwrap();
    let (mut owner, mut defaulted, mut present, mut acl) = (null_mut(), 0, 0, null_mut());
    check(unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) }).unwrap();
    check(unsafe {
        GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
    })
    .unwrap();
    check_acl(owner, acl, true).unwrap();
    let mut ace = null_mut();
    check(unsafe { GetAce(acl, 2, &mut ace) }).unwrap();
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(
        sid_text(std::ptr::addr_of!(allowed.SidStart).cast_mut().cast()).unwrap(),
        "S-1-5-32-545"
    );
    assert_eq!(
        allowed.Mask,
        GENERIC_READ | windows_sys::Win32::Foundation::GENERIC_EXECUTE
    );
}
