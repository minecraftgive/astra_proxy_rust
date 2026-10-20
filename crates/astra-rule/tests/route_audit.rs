//! Exercise real IPC, log formatting, provenance, rejection and generation changes.
use astra_ipc::{
    control, platform::LocalStream, Inbound, Network, ProcessInfo, RouteAction, RouteRequest,
    RouteResponse, Target, API_VERSION,
};
use std::{collections::HashSet, path::PathBuf, time::Duration};

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn request(id: u64, host: &str) -> RouteRequest {
    RouteRequest {
        api_version: API_VERSION,
        id,
        target: Target {
            host: host.into(),
            port: 443,
        },
        network: Network::Udp,
        inbound: Inbound::Tun,
        process: Some(ProcessInfo {
            pid: 88,
            name: "browser.exe".into(),
            path: "C:\\Apps\\browser.exe".into(),
        }),
    }
}
async fn query(stream: &mut LocalStream, req: &RouteRequest) -> RouteResponse {
    astra_ipc::send(stream, req).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), astra_ipc::receive(stream))
        .await
        .unwrap()
        .unwrap()
}
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(key))
        .unwrap()
}

#[tokio::test]
async fn every_received_request_has_correlated_decision_or_rejection_and_response() {
    for level in ["route", "info"] {
        let temp =
            Temp(std::env::temp_dir().join(format!("astra-audit-{}-{level}", std::process::id())));
        std::fs::create_dir_all(&temp.0).unwrap();
        let route = temp.0.join("rule.r");
        let ctl = temp.0.join("rule.ctl");
        let config = temp.0.join("rule.toml");
        let logfile = temp.0.join("rule.log");
        let text = format!("socket={:?}\ndefault_action='block'\ngeosite_database='geosite.dat'\n[[rules]]\nkind='rule_set'\nvalue='empty'\naction='block'\n[[rules]]\nkind='geosite'\nvalue='TEST'\naction='proxy:edge'\n[[rules]]\nkind='domain_exact'\nvalue='allowed.test'\naction='direct'\n[[rules]]\nkind='domain_exact'\nvalue='blocked.test'\naction='block'\n[rule_sets.empty]\nbehavior='domain'\npayload=[]\n",route.to_string_lossy());
        std::fs::write(&config, &text).unwrap();
        #[cfg(windows)]
        {
            std::fs::write(&route, b"").unwrap();
            std::fs::write(&ctl, b"").unwrap();
        }
        std::fs::write(
            temp.0.join("geosite.dat"),
            include_bytes!("data/geosite.dat"),
        )
        .unwrap();
        let log = std::fs::File::create(&logfile).unwrap();
        #[cfg(windows)]
        {
            let checked = astra_ipc::platform::background_core(env!("CARGO_BIN_EXE_astra-rule"))
                .args(["--check".as_ref(), config.as_os_str()])
                .env("ASTRA_CONTROL_SOCKET", &ctl)
                .output()
                .await
                .unwrap();
            assert!(checked.status.success());
            assert!(
                route.exists() && ctl.exists(),
                "--check must leave endpoints untouched"
            );
        }
        let mut child = astra_ipc::platform::background_core(env!("CARGO_BIN_EXE_astra-rule"))
            .arg(&config)
            .env("ASTRA_CONTROL_SOCKET", &ctl)
            .env("ASTRA_LOG_LEVEL", level)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut first = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(stream) = LocalStream::connect(&route).await {
                    break stream;
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(&logfile).unwrap()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let initial = query(&mut first, &request(1, "suffix.test")).await;
        assert_eq!(initial.action, RouteAction::Proxy("edge".into()));
        let explanation = control::explain_route(&ctl, request(0, "suffix.test"))
            .await
            .unwrap();
        assert_eq!(explanation.action, initial.action);
        assert_eq!(explanation.revision, initial.revision);
        assert!(explanation
            .path
            .starts_with("rules[2] -> geosite(\"TEST\")"));
        assert_eq!(explanation.default_action, "block");
        assert_eq!(
            explanation.condition,
            Some(("domain_suffix".into(), "suffix.test".into()))
        );
        let mut second = LocalStream::connect(&route).await.unwrap();
        assert_eq!(
            query(&mut second, &request(1, "allowed.test")).await.action,
            RouteAction::Direct
        );
        assert_eq!(
            query(&mut first, &request(2, "blocked.test")).await.action,
            RouteAction::Block
        );
        assert_eq!(
            query(&mut first, &request(3, "127.0.0.1")).await.action,
            RouteAction::Direct
        );
        assert_eq!(
            query(&mut first, &request(4, "unmatched.test"))
                .await
                .action,
            RouteAction::Block
        );
        std::fs::write(
            &config,
            text.replace("default_action='block'", "default_action='proxy:later'"),
        )
        .unwrap();
        let before_reload = control::explain_route(&ctl, request(0, "unmatched.test"))
            .await
            .unwrap();
        assert_eq!(before_reload.action, RouteAction::Block);
        assert_eq!(
            before_reload.default_action, "block",
            "query must use active configuration, not the edited file"
        );
        control::request(&ctl, control::Command::Reload { path: None })
            .await
            .unwrap();
        let updated = query(&mut first, &request(5, "unmatched.test")).await;
        assert_eq!(updated.action, RouteAction::Proxy("later".into()));
        assert!(updated.revision > initial.revision);
        let explanation = control::explain_route(&ctl, request(0, "unmatched.test"))
            .await
            .unwrap();
        assert_eq!(explanation.action, updated.action);
        assert_eq!(explanation.revision, updated.revision);
        assert_eq!(explanation.path, "default_action");
        assert_eq!(explanation.default_action, "proxy:later");
        for (id, invalid) in [(90, false), (91, true)] {
            let mut stream = LocalStream::connect(&route).await.unwrap();
            let mut req = request(id, "invalid.test");
            if invalid {
                req.target.port = 0;
            } else {
                req.api_version = API_VERSION + 1;
            }
            astra_ipc::send(&mut stream, &req).await.unwrap();
            assert!(tokio::time::timeout(
                Duration::from_secs(2),
                astra_ipc::receive::<RouteResponse>(&mut stream)
            )
            .await
            .unwrap()
            .is_err());
        }
        // Revision subscriptions are control traffic, not user routing requests.
        let mut malformed = LocalStream::connect(&route).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut malformed, &[0, 0, 0, 0])
            .await
            .unwrap();
        assert!(tokio::time::timeout(
            Duration::from_secs(2),
            astra_ipc::receive::<RouteResponse>(&mut malformed)
        )
        .await
        .unwrap()
        .is_err());
        drop(malformed);
        let mut watcher = LocalStream::connect(&route).await.unwrap();
        let mut subscribe = request(0, "");
        subscribe.target.port = 0;
        assert_eq!(query(&mut watcher, &subscribe).await.id, 0);
        drop(watcher);
        drop(first);
        drop(second);
        control::request(&ctl, control::Command::Stop)
            .await
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        let log = std::fs::read_to_string(&logfile).unwrap();
        if level == "info" {
            assert!(!log.contains("core=\"rule\""));
            continue;
        }
        assert!(log.contains(" ROUTE astra_route:"), "{log}");
        for text in [
            "network=Udp",
            "inbound=Tun",
            "browser.exe",
            "rules[2] -> geosite(\"TEST\")",
            "rules[3] -> domain_exact(\"allowed.test\")",
            "direct_local -> DIRECT",
            "default_action",
            "unsupported IPC version",
            "invalid target",
            "decode_failed",
            "elapsed_us=",
        ] {
            assert!(log.contains(text), "missing {text}: {log}");
        }
        let lines: Vec<_> = log
            .lines()
            .filter(|line| line.contains("request_id="))
            .collect();
        let received: HashSet<_> = lines
            .iter()
            .filter(|line| line.contains("phase=\"received\""))
            .map(|line| (field(line, "peer_id="), field(line, "request_id=")))
            .collect();
        assert_eq!(received.len(), 8, "{log}");
        assert_eq!(received.iter().filter(|(_, id)| *id == "1").count(), 2);
        for (peer, id) in received {
            let phases: Vec<_> = lines
                .iter()
                .filter(|line| field(line, "peer_id=") == peer && field(line, "request_id=") == id)
                .map(|line| field(line, "phase="))
                .collect();
            assert_eq!(
                phases,
                if id == "90" || id == "91" {
                    vec!["\"received\"", "\"rejected\""]
                } else {
                    vec!["\"received\"", "\"decision\"", "\"responded\""]
                }
            );
        }
    }
}
