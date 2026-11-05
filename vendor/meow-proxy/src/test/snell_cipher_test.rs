use super::*;

#[test]
fn kdf_is_deterministic_for_same_inputs() {
    let psk = b"shared-secret";
    let salt = [7u8; 16];
    let a = snell_kdf(psk, &salt, 16);
    let b = snell_kdf(psk, &salt, 16);
    assert_eq!(a, b);
    assert_eq!(a.len(), 16);
}

#[test]
fn kdf_differs_on_different_salt() {
    let psk = b"shared-secret";
    let a = snell_kdf(psk, &[0u8; 16], 16);
    let b = snell_kdf(psk, &[1u8; 16], 16);
    assert_ne!(a, b);
}
