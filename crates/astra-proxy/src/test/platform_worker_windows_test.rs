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
fn nul_path_is_rejected() {
    assert!(wide(OsStr::new("bad\0path")).is_err());
}
