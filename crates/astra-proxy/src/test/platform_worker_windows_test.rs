use super::*;
#[test]
fn environment_removes_host_control_and_is_double_terminated() {
    let values = [
        ("Path", "normal-bin"),
        ("aStRa_CoNtRoL_sOcKeT", "secret"),
        ("ASTRA_HOSTED", "1"),
        ("ASTRA_HOSTED_EXPERIMENTAL", "1"),
        ("SUDO_UID", "0"),
        ("ASTRA_ELEVATED_OWNER", "secret"),
    ];
    let block = environment_from(values.map(|(key, value)| (key.into(), value.into()))).unwrap();
    assert_eq!(&block[block.len() - 2..], &[0, 0]);
    assert_eq!(String::from_utf16_lossy(&block), "Path=normal-bin\0\0");
    for item in block
        .split(|value| *value == 0)
        .filter(|item| !item.is_empty())
    {
        let text = String::from_utf16_lossy(item);
        for name in super::super::REMOVED_ENV {
            assert!(!text.to_uppercase().starts_with(&format!("{name}=")));
        }
    }
}
#[test]
fn token_validation_agrees_with_current_elevation() {
    let current = token(unsafe { GetCurrentProcess() }, TOKEN_QUERY).unwrap();
    if astra_platform::elevation::is_elevated().unwrap() {
        assert!(ordinary(raw(&current)).is_err());
    } else {
        ordinary(raw(&current)).unwrap();
    }
    same_identity(raw(&current), raw(&current)).unwrap();
}

#[test]
fn identification_token_falls_back_to_verified_ordinary_supervisor() {
    use windows_sys::Win32::Security::{SecurityIdentification, TokenImpersonation};
    if astra_platform::elevation::is_elevated().unwrap() {
        return; // The fixture must itself be an ordinary supervisor.
    }
    let current = token(
        unsafe { GetCurrentProcess() },
        TOKEN_QUERY | TOKEN_DUPLICATE,
    )
    .unwrap();
    let mut identification = null_mut();
    check(unsafe {
        DuplicateTokenEx(
            raw(&current),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            null(),
            SecurityIdentification,
            TokenImpersonation,
            &mut identification,
        )
    })
    .unwrap();
    let identification = unsafe { own(identification) }.unwrap();
    // Reproduce Windows' real rejection of the old linked-token conversion.
    assert_eq!(
        duplicate_primary(raw(&identification))
            .unwrap_err()
            .raw_os_error(),
        Some(ERROR_BAD_IMPERSONATION_LEVEL as i32)
    );
    let owner = astra_platform::lifecycle::capture(std::process::id()).unwrap();
    let primary =
        duplicate_worker_token(raw(&current), raw(&identification), || Ok(owner)).unwrap();
    ordinary(raw(&primary)).unwrap();
    same_identity(raw(&current), raw(&primary)).unwrap();
    let kind = information(raw(&primary), TokenType).unwrap();
    assert_eq!(unsafe { *kind.as_ptr().cast::<i32>() }, TokenPrimary);

    let missing = duplicate_worker_token(raw(&current), raw(&identification), || {
        anyhow::bail!("missing supervisor fixture")
    })
    .unwrap_err();
    assert!(format!("{missing:#}").contains("missing supervisor fixture"));

    // A usable primary source keeps the existing path and needs no supervisor.
    duplicate_worker_token(raw(&current), raw(&current), || {
        panic!("ordinary primary token must not use the fallback")
    })
    .unwrap();
}

#[test]
fn supervisor_token_rejects_reused_process_identity_and_elevated_owner() {
    let current = token(unsafe { GetCurrentProcess() }, TOKEN_QUERY).unwrap();
    let mut owner = astra_platform::lifecycle::capture(std::process::id()).unwrap();
    owner.started.push_str("-stale");
    let error = supervisor_token(raw(&current), &owner).unwrap_err();
    assert!(format!("{error:#}").contains("process identity changed"));

    let owner = astra_platform::lifecycle::capture(std::process::id()).unwrap();
    let result = supervisor_token(raw(&current), &owner);
    if astra_platform::elevation::is_elevated().unwrap() {
        assert!(format!("{:#}", result.unwrap_err()).contains("refusing an elevated worker token"));
    } else {
        let primary = result.unwrap();
        ordinary(raw(&primary)).unwrap();
        same_identity(raw(&current), raw(&primary)).unwrap();
    }
}

#[test]
fn nul_path_is_rejected() {
    assert!(wide(OsStr::new("bad\0path")).is_err());
}
