use super::*;
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

struct Fixture {
    path: PathBuf,
    parent: PathBuf,
    name: OsString,
}

impl Fixture {
    fn new() -> Self {
        Self::new_in(&std::env::temp_dir())
    }

    fn new_in(parent: &Path) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = OsString::from(format!(
            "astra-proxy-privilege-{}-{nonce}",
            std::process::id()
        ));
        let path = parent.join(&name);
        let parent = std::fs::canonicalize(parent).unwrap();
        std::fs::create_dir(&path).unwrap();
        Self { path, parent, name }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(target) = std::fs::canonicalize(&self.path) {
            assert_eq!(target, self.parent.join(&self.name));
            let _ = std::fs::remove_dir_all(target);
        }
    }
}

#[test]
fn standalone_arguments_include_the_supervising_process_identity() {
    let fixture = Fixture::new();
    let config = fixture.path.join("proxy config.toml");
    let socket = fixture.path.join("control socket");
    let log = fixture.path.join("proxy log");
    let runtime = fixture.path.join("runtime");
    let args = elevated_arguments(&config, &socket, &log, "debug", &runtime).unwrap();

    assert_eq!(args.len(), 12, "The service requires all 12 core arguments");
    assert_eq!(args[0], OsStr::new("--windows-elevated"));
    assert_eq!(args[1], config.as_os_str());
    assert_eq!(args[2], socket.as_os_str());
    assert_eq!(args[3], log.as_os_str());
    assert_eq!(args[4], OsStr::new("debug"));
    assert!(Path::new(&args[6]).is_absolute());
    assert!(args[7].to_str().unwrap().starts_with("S-1-"));
    assert_eq!(args[8], runtime.as_os_str());
    assert!(runtime.is_dir());
    assert!(args[9].is_empty(), "Standalone launches replace no process");
    assert!(matches!(args[10].to_str(), Some("0" | "1")));

    let owner: astra_platform::lifecycle::ProcessIdentity =
        serde_json::from_str(args[11].to_str().unwrap()).unwrap();
    assert_eq!(owner.pid, std::process::id());
    assert_eq!(
        owner,
        astra_platform::lifecycle::capture(std::process::id()).unwrap()
    );
    assert!(astra_platform::lifecycle::matches_live(&owner).unwrap());
    assert!(
        !config.exists(),
        "Argument preparation must not create a config"
    );
}

#[test]
fn relative_runtime_directory_is_sent_as_an_absolute_path() {
    let fixture = Fixture::new_in(Path::new("."));
    let root = std::path::absolute(&fixture.path).unwrap();
    let runtime = fixture.path.join("runtime");
    let args = elevated_arguments(
        &root.join("proxy.toml"),
        &root.join("control"),
        &root.join("logs"),
        "info",
        &runtime,
    )
    .unwrap();

    assert!(!runtime.is_absolute());
    assert_eq!(args[8], root.join("runtime").as_os_str());
    assert!(Path::new(&args[8]).is_dir());
}

#[tokio::test]
async fn non_tun_launch_never_requires_elevation_or_an_existing_config() {
    let fixture = Fixture::new();
    assert!(!delegate(&fixture.path.join("missing.toml"), false)
        .await
        .unwrap());
}
