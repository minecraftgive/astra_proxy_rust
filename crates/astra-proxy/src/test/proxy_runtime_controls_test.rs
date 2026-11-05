use super::*;
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "astra-override-checkpoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn rejected_override_checkpoint_restores_exact_previous_file_or_absence() {
    let directory = Directory::new();
    let path = directory.0.join("overrides.json");
    let fault = Arc::new(std::sync::OnceLock::new());
    let next = Overrides {
        allow_lan: Some(true),
        ..Default::default()
    };
    std::fs::write(&path, b"{\n  \"allow_lan\": false\n}\n").unwrap();
    let before = std::fs::read(&path).unwrap();
    drop(OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::remove_file(&path).unwrap();
    drop(OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap());
    assert!(!path.exists());
    assert!(fault.get().is_none());
    let mut checkpoint = OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap();
    checkpoint.commit();
    drop(checkpoint);
    let saved: Overrides = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved.allow_lan, Some(true));
}
#[test]
fn failed_override_rollback_latches_uncertain_runtime() {
    let directory = Directory::new();
    let path = directory.0.join("overrides.json");
    std::fs::write(&path, b"{}").unwrap();
    let fault = Arc::new(std::sync::OnceLock::new());
    let checkpoint =
        OverrideCheckpoint::begin(path.clone(), &Overrides::default(), fault.clone()).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    drop(checkpoint);
    assert!(fault.get().unwrap().contains("outcome unknown"));
    assert!(path.is_dir());
}
