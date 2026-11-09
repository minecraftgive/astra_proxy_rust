use super::*;
use astra_model::network_state::{Interface, InterfaceKind, Issue, Resolver, Snapshot};

fn snapshot() -> Snapshot {
    Snapshot {
        schema_version: 1,
        agent_id: "target".into(),
        sampled_at_ms: "1".into(),
        dns_sampled_at_ms: "1".into(),
        truncated: false,
        routes: vec![],
        issues: vec![],
        interfaces: vec![Interface {
            id: "17".into(),
            name: "adapter0".into(),
            index: 17,
            kind: InterfaceKind::Physical,
            connected: true,
            addresses: vec![],
            source: "fixture".into(),
        }],
        dns: vec![Resolver {
            id: "resolver".into(),
            source: "fixture".into(),
            interface_id: Some("17".into()),
            scope: "link".into(),
            domains: vec![],
            servers: vec!["127.0.0.1".into()],
            priority: None,
            flags: vec![],
        }],
    }
}

#[test]
fn scoped_resolver_pool_requires_complete_policy_and_confirmed_adapter() {
    let pool = resolver_pool(snapshot(), "adapter0");
    assert_eq!(pool.len(), 1);
    assert_eq!(pool[0].interface_id.as_deref(), Some("adapter0"));
    for component in ["dns", "dns_policy", "nrpt"] {
        let mut sample = snapshot();
        sample.issues.push(Issue {
            component: component.into(),
            message: "unavailable".into(),
        });
        assert!(resolver_pool(sample, "adapter0").is_empty());
    }
    for scope in ["policy", "scoped", "service"] {
        let mut sample = snapshot();
        let mut unknown = sample.dns[0].clone();
        unknown.scope = scope.into();
        unknown.interface_id = None;
        unknown.domains = vec!["corp.invalid".into()];
        sample.dns.push(unknown);
        assert!(resolver_pool(sample, "adapter0").is_empty());
    }
    let mut sample = snapshot();
    sample.truncated = true;
    assert!(resolver_pool(sample, "adapter0").is_empty());
    let mut sample = snapshot();
    sample.interfaces[0].connected = false;
    assert!(resolver_pool(sample, "adapter0").is_empty());
    assert!(resolver_pool(snapshot(), "other-adapter").is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn revalidation_refuses_reload_restart_and_selection_switch_before_probe() {
    use astra_model::control::{Applied, Command, Request as ControlRequest, Response};
    use astra_model::{
        nettool::{Probe, Request},
        runtime::{Group, Member, Snapshot as Runtime},
    };
    const CONFIG: &str = "standalone=true\n[[outbounds]]\nkind='socks5'\ntag='a'\nserver='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='b'\nserver='127.0.0.1:1081'\n[proxy_groups.outer]\nmembers=['a','b']\n";
    for change in ["stable", "restart", "reload", "switch"] {
        let root = tempfile::Builder::new()
            .prefix("pin-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("c.sock");
        let path = root.path().join("config.toml");
        let listener = astra_platform::platform::bind_listener(&socket).unwrap();
        astra_platform::platform::set_private(
            astra_platform::platform::endpoint_path(&socket),
            0o600,
        )
        .unwrap();
        let captured = astra_platform::lifecycle::identity(Some(Applied {
            path: path.clone(),
            digest: astra_ipc::control::config_digest(CONFIG),
        }))
        .unwrap();
        let mut observed = captured.clone();
        if change == "restart" {
            observed.boot_id = "restarted".into();
        }
        if change == "reload" {
            observed.applied.as_mut().unwrap().digest += 1;
        }
        let runtime = Runtime {
            pid: std::process::id(),
            groups: vec![Group {
                name: "outer".into(),
                mode: "select".into(),
                selected: Some("a".into()),
                testing: false,
                members: ["a", "b"]
                    .into_iter()
                    .map(|name| Member {
                        name: name.into(),
                        delay_ms: None,
                        alive: None,
                    })
                    .collect(),
            }],
            ..Default::default()
        };
        let pinned = selection::rebuild(CONFIG, "outer", Some(&runtime)).unwrap();
        let mut next = runtime;
        if change == "switch" {
            next.groups[0].selected = Some("b".into());
        }
        let server = tokio::spawn(async move {
            for index in 0..if matches!(change, "restart" | "reload") {
                1
            } else {
                3
            } {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = astra_ipc::receive::<ControlRequest>(&mut stream)
                    .await
                    .unwrap();
                if index == 1 {
                    assert!(matches!(request.command, Command::Session));
                    astra_ipc::send_management(
                        &mut stream,
                        &astra_ipc::session::Event::<Response>::Ready(astra_ipc::API_VERSION),
                    )
                    .await
                    .unwrap();
                    let request = astra_ipc::receive_management::<ControlRequest>(&mut stream)
                        .await
                        .unwrap();
                    assert!(matches!(
                        request.command,
                        Command::Runtime(astra_model::runtime::Command::Snapshot)
                    ));
                    astra_ipc::send_management(
                        &mut stream,
                        &astra_ipc::session::Event::Reply(Response::Runtime(Ok(next.clone()))),
                    )
                    .await
                    .unwrap();
                } else {
                    assert!(matches!(request.command, Command::Identity));
                    astra_ipc::send_management(&mut stream, &observed)
                        .await
                        .unwrap();
                }
            }
        });
        let mut plan = DiagnosticPlan {
            runtime_dir: root.path().into(),
            log_path: root.path().join("log"),
            executable: root.path().join("unused"),
            config_path: path,
            text: CONFIG.into(),
            running: true,
            control: None,
        };
        let request = Request {
            config: String::new(),
            egress: Egress::Outbound("outer".into()),
            probe: Probe::Tcp {
                host: "target.invalid".into(),
                port: 443,
            },
            count: 1,
            interval_ms: 0,
            timeout_ms: 100,
        };
        let mut context = plan
            .diagnostic_context(
                &request,
                astra_platform::program_info::ProgramInfo::collect(
                    astra_platform::build_info!(),
                    vec![],
                ),
                super::super::StreamMode::Detailed,
            )
            .await;
        context.running_core = Some(captured);
        plan.control = Some((socket, Some(std::process::id())));
        let mut request: DetailedRequest = request.into();
        request.pinned = Some(pinned);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            plan.verify_pinned(&request, &context),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(result.is_ok(), change == "stable", "{change}: {result:?}");
    }
}
