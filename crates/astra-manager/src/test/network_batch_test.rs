use super::*;
use astra_model::nettool::{Egress, Request};
fn project(id: &str, path: &str) -> NetworkBatchProject {
    NetworkBatchProject {
        id: id.into(),
        path_id: path.into(),
        request: Request {
            config: String::new(),
            egress: Egress::System,
            probe: Probe::Tcp {
                host: "127.0.0.1".into(),
                port: 80,
            },
            count: 1,
            interval_ms: 0,
            timeout_ms: 100,
        }
        .into(),
    }
}
#[test]
fn batch_bounds_paths_and_duplicate_identity() {
    let mut request = NetworkBatchRequest {
        projects: vec![project("tcp", "path1")],
        comparison: false,
    };
    assert!(validate(&request).is_ok());
    request.projects.push(project("tcp", "path2"));
    assert!(validate(&request)
        .unwrap_err()
        .to_string()
        .contains("Duplicate"));
    request.projects = (0..4)
        .map(|n| project(&format!("tool{n}"), &format!("path{n}")))
        .collect();
    assert!(validate(&request)
        .unwrap_err()
        .to_string()
        .contains("three"));
    request.projects.clear();
    assert!(matches!(
        crate::error::code(&validate(&request).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
    request.projects = (0..33)
        .map(|n| project(&format!("tool{n}"), "path1"))
        .collect();
    assert!(matches!(
        crate::error::code(&validate(&request).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
}
#[test]
fn paths_cannot_hide_different_exits_or_resolvers_but_allow_two_families() {
    let mut request = NetworkBatchRequest {
        projects: vec![project("v4", "same"), project("v6", "same")],
        comparison: false,
    };
    request.projects[0].request.family = Some(astra_model::nettool::AddressFamily::Ipv4);
    request.projects[1].request.family = Some(astra_model::nettool::AddressFamily::Ipv6);
    assert!(validate(&request).is_ok());
    request.projects[1].request.resolver = Some("1.1.1.1".into());
    assert!(matches!(
        crate::error::code(&validate(&request).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
    request.projects[1].request.resolver = None;
    request.projects[1].request.egress = Egress::Interface("en0".into());
    assert!(matches!(
        crate::error::code(&validate(&request).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
}
#[test]
fn nat_in_batch_still_requires_manual_interface_and_matching_exit() {
    let mut p = project("nat", "physical");
    p.request.probe = Probe::Nat {
        request: astra_model::network_nat::Request {
            agent_id: "agent".into(),
            interface_id: String::new(),
            family: Default::default(),
            servers: vec!["127.0.0.1:3478".into()],
            advanced: false,
        },
    };
    let mut request = NetworkBatchRequest {
        projects: vec![p],
        comparison: false,
    };
    assert!(matches!(
        crate::error::code(&validate(&request).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
    if let Probe::Nat { request } = &mut request.projects[0].request.probe {
        request.interface_id = "en0".into();
    }
    assert!(matches!(
        crate::error::code(&validate(&request).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
    request.projects[0].request.egress = Egress::Interface("en0".into());
    assert!(validate(&request).is_ok());
}

#[cfg(unix)]
mod live {
    use super::*;
    use astra_management::config::ManagerConfig;
    use std::{os::unix::fs::PermissionsExt, path::Path};
    async fn agent(root: &Path) -> (Arc<AgentService>, tokio::task::JoinHandle<Result<()>>) {
        let config = ManagerConfig {
            state_dir: root.join("state"),
            config_dir: root.join("config"),
            bin_dir: root.join("bin"),
            runtime_dir: Some(root.join("runtime")),
            ..Default::default()
        };
        std::fs::create_dir_all(&config.bin_dir).unwrap();
        let mut info = astra_platform::program_info::ProgramInfo::collect(
            astra_platform::build_info!(),
            vec![],
        );
        info.build.name = "astra-proxy".into();
        info.features
            .push(astra_platform::program_info::FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_STREAM_CAPABILITY,
                "fixture",
                true,
                "fixture",
            ));
        let info = serde_json::to_string(&info).unwrap().replace('\'', "'\\''");
        let binary = config.bin_dir.join("astra-proxy");
        let pids = root.join("pids");
        let pids = pids.to_string_lossy().replace('\'', "'\\''");
        let script = format!(
            r##"#!/bin/sh
set -eu
if [ "$1" = --check-stdin ]; then cat >/dev/null; exit 0; fi
if [ "$1" = --program-info ]; then printf '%s\n' '{info}'; exit 0; fi
[ "$1" = --network-diagnostics-stream ] || exit 2
payload=$(cat)
printf '%s\n' '{{"event":"attempt","seq":1,"rtt_ms":1.0,"detail":"retained local fixture","error":null}}'
case "$payload" in
  *sleep*) echo $$ >> '{pids}'; exec sleep 30;;
  *forge*) printf '%s\n' '{{"event":"project_status","project_id":"forged","path_id":"forged","round":1,"state":"complete","message":"forged"}}';;
  *fail*) exit 2;;
esac
printf '%s\n' '{{"event":"done"}}'
"##
        );
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let agent = AgentService::new(config).unwrap();
        let task = agent.start_native().await.unwrap();
        let source = root.join("proxy.toml");
        std::fs::write(&source, "standalone=true\n").unwrap();
        let response = agent
            .handle(execute(
                "add",
                Command::Add {
                    name: "proxy".into(),
                    kind: InstanceKind::Proxy,
                    config: source,
                },
            ))
            .await;
        assert!(response.result.is_ok(), "{response:?}");
        (agent, task)
    }
    fn execute(id: &str, command: Command) -> astra_management::v2::Request {
        astra_management::v2::Request::new(
            id,
            RequestBody::Execute {
                operation_id: id.into(),
                command,
            },
        )
    }
    async fn finish(agent: &AgentService, id: &str) -> NetworkReport {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let report = agent.store.network_report(id, None).await.unwrap();
                if report.state.terminal() {
                    return report;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    async fn shutdown(agent: Arc<AgentService>, task: tokio::task::JoinHandle<Result<()>>) {
        let response = agent.handle(execute("shutdown", Command::Shutdown)).await;
        assert!(response.result.is_ok(), "{response:?}");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    fn hosted_project(id: &str, host: &str) -> NetworkBatchProject {
        let mut p = project(id, "path1");
        p.request.probe = Probe::Tcp {
            host: host.into(),
            port: 9,
        };
        p
    }
    #[tokio::test]
    async fn batch_retains_success_and_failure_without_accepting_forged_manager_events() {
        let root = tempfile::tempdir().unwrap();
        let (agent, task) = agent(root.path()).await;
        let request = NetworkBatchRequest {
            comparison: false,
            projects: vec![
                hosted_project("good", "good"),
                hosted_project("bad", "fail"),
                hosted_project("spoof", "forge"),
            ],
        };
        let response = agent
            .handle(execute(
                "batch",
                Command::NetworkDiagnosticsBatch {
                    instance: "proxy".into(),
                    request,
                },
            ))
            .await;
        assert!(response.result.is_ok(), "{response:?}");
        let report = finish(&agent, "batch").await;
        assert_eq!(report.state, NetworkState::Partial);
        assert!(report
            .items
            .iter()
            .any(|item| item.data["event"] == "project_status"
                && item.data["project_id"] == "good"
                && item.data["state"] == "complete"));
        assert!(report
            .items
            .iter()
            .any(|item| item.data["event"] == "project_status"
                && item.data["project_id"] == "bad"
                && item.data["state"] == "failed"));
        assert!(report
            .items
            .iter()
            .any(|item| item.data["event"] == "project_status"
                && item.data["project_id"] == "spoof"
                && item.data["state"] == "failed"));
        assert!(!report
            .items
            .iter()
            .any(|item| item.data.to_string().contains("\"project_id\":\"forged\"")));
        assert!(report
            .items
            .iter()
            .any(|item| item.data["detail"]["detail"] == "retained local fixture"));
        shutdown(agent, task).await;
    }
    #[tokio::test]
    async fn batch_runs_at_most_four_children_and_cancel_reaps_all_before_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        let (agent, task) = agent(root.path()).await;
        let request = NetworkBatchRequest {
            comparison: false,
            projects: (0..8)
                .map(|n| hosted_project(&format!("p{n}"), "sleep"))
                .collect(),
        };
        assert!(agent
            .handle(execute(
                "batch",
                Command::NetworkDiagnosticsBatch {
                    instance: "proxy".into(),
                    request
                }
            ))
            .await
            .result
            .is_ok());
        let pids = root.path().join("pids");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read_to_string(&pids)
                    .unwrap_or_default()
                    .lines()
                    .count()
                    == 4
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(std::fs::read_to_string(&pids).unwrap().lines().count(), 4);
        let response = agent
            .handle(execute(
                "cancel",
                Command::CancelTask {
                    operation_id: "batch".into(),
                },
            ))
            .await;
        let Ok(ResponseBody::NetworkReport(report)) = response.result else {
            panic!("{response:?}")
        };
        assert_eq!(report.state, NetworkState::Cancelled);
        let pids = std::fs::read_to_string(pids).unwrap();
        assert_eq!(pids.lines().count(), 4);
        for pid in pids.lines() {
            assert_eq!(unsafe { libc::kill(pid.parse().unwrap(), 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        assert!(report
            .items
            .iter()
            .any(|item| item.data["state"] == "cancelled"));
        shutdown(agent, task).await;
    }
}
