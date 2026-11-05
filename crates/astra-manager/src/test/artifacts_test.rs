use super::*;
#[test]
fn retries_are_idempotent_and_only_verified_bytes_can_be_read() {
    let root = tempfile::tempdir().unwrap();
    let data = b"verified content";
    let digest = format!("{:x}", sha2::Sha256::digest(data));
    handle(
        root.path(),
        1024,
        ArtifactCommand::Begin {
            id: "a".into(),
            name: "../irrelevant.zip".into(),
            size: data.len().to_string(),
            sha256: digest,
        },
    )
    .unwrap();
    let command = ArtifactCommand::Write {
        id: "a".into(),
        offset: "0".into(),
        base64: base64::engine::general_purpose::STANDARD.encode(data),
        sha256: format!("{:x}", sha2::Sha256::digest(data)),
    };
    handle(root.path(), 1024, command.clone()).unwrap();
    handle(root.path(), 1024, command).unwrap();
    assert!(complete_path(root.path(), "a").is_err());
    handle(
        root.path(),
        1024,
        ArtifactCommand::Finish { id: "a".into() },
    )
    .unwrap();
    assert_eq!(
        std::fs::read(complete_path(root.path(), "a").unwrap()).unwrap(),
        data
    );
    assert!(paths(root.path(), "../escape").is_err());
    let view = package_view(root.path(), "a").unwrap();
    assert_eq!(view.path.extension().unwrap(), "zip");
    assert_eq!(std::fs::read(&view.path).unwrap(), data);
    let path = view.path.clone();
    drop(view);
    assert!(!path.exists());
}
