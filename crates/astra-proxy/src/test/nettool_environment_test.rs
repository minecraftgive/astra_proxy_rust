//! Exercise the actual legacy entrypoints: host probes cannot load a profile
//! just because a caller supplied one, while configured outbounds still do.
use astra_model::nettool::{Egress, Event, Probe, Request};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpListener, process::Command};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("astra-nettool-env-{}", uuid::Uuid::new_v4()));
        astra_platform::storage::private_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn legacy_system_entrypoints_ignore_unrelated_broken_profiles() {
    let root = Directory::new();
    let profile = root.0.join("broken.toml");
    let source = "THIS_IS_NOT_A_PROXY_CONFIGURATION";
    std::fs::write(&profile, source).unwrap();
    let runtime = root.0.join("runtime");
    astra_platform::storage::private_dir(&runtime).unwrap();
    let sentinel = runtime.join("retained.cache");
    std::fs::write(&sentinel, "original state").unwrap();
    for stream in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_astra-proxy"));
        command
            .env("ASTRA_RUNTIME_DIR", &runtime)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stream {
            command.arg("--nettool-stream").arg(&profile);
        } else {
            command
                .arg("--nettool")
                .arg("--config")
                .arg(&profile)
                .args(["--via", "system", "-c", "1", "-i", "0", "-W", "1000", "tcp"])
                .arg(address.to_string());
        }
        let mut child = command.spawn().unwrap();
        if stream {
            let request = Request {
                config: source.into(),
                egress: Egress::System,
                probe: Probe::Tcp {
                    host: "127.0.0.1".into(),
                    port: address.port(),
                },
                count: 1,
                interval_ms: 0,
                timeout_ms: 1000,
            };
            child
                .stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&request).unwrap())
                .await
                .unwrap();
        } else {
            drop(child.stdin.take());
        }
        let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        if stream {
            let events: Vec<Event> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(events
                .iter()
                .any(|event| matches!(event, Event::Summary(summary) if summary.received == 1)));
            assert!(matches!(events.last(), Some(Event::Done)));
        } else {
            assert!(String::from_utf8(output.stdout)
                .unwrap()
                .contains("1 sent, 1 received"));
        }
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), source);
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "original state"
        );
        assert_eq!(std::fs::read_dir(&runtime).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn legacy_outbound_still_requires_its_configuration() {
    let root = Directory::new();
    let profile = root.0.join("missing.toml");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(env!("CARGO_BIN_EXE_astra-proxy"))
            .arg("--nettool")
            .arg("--config")
            .arg(profile)
            .args(["--via", "outbound:DIRECT", "-c", "1", "tcp", "127.0.0.1:9"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(" sent,"));
}
