use super::*;
const VALID:&str="[[listeners]]\ntag='socks'\nprotocol='socks5'\nlisten='127.0.0.1:1080'\n[[listeners.users]]\nname='user'\npassword='secret'\n";
fn check(text: &str) -> Result<Prepared> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("server.toml");
    std::fs::write(&path, text)?;
    load(&path)
}
#[test]
fn rejects_unauthenticated_ambiguous_and_public_plaintext_configs() {
    assert!(check(VALID).is_ok());
    assert!(check(&VALID.replace("127.0.0.1", "0.0.0.0")).is_err());
    assert!(check(&format!(
        "allow_private_targets=true\n{}",
        VALID.replace("listen=", "typo=")
    ))
    .is_err());
    assert!(check(VALID.split("[[listeners.users]]").next().unwrap()).is_err());
    assert!(check(&format!("{VALID}\n{VALID}")).is_err());
    assert!(check(&VALID.replace("password='secret'", "password=''")).is_err());
}

#[test]
fn candidate_validation_does_not_create_or_replace_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("server.toml");
    assert!(prepare_text(&source, VALID).is_ok());
    assert!(!source.exists());
    std::fs::write(&source, b"original").unwrap();
    assert!(prepare_text(&source, VALID).is_ok());
    assert_eq!(std::fs::read(source).unwrap(), b"original");
}
