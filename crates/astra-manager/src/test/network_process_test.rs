use super::*;

fn info(detailed: bool) -> ProgramInfo {
    let mut info = ProgramInfo::collect(astra_platform::build_info!(), vec![]);
    info.build.name = "astra-proxy".into();
    if detailed {
        info.features
            .push(astra_platform::program_info::FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_STREAM_CAPABILITY,
                "diagnostics",
                true,
                "test",
            ));
    }
    info
}

fn request() -> Request {
    Request {
        config: "must be cleared".into(),
        egress: Egress::System,
        probe: Probe::Tcp {
            host: "127.0.0.1".into(),
            port: 9,
        },
        count: 1,
        interval_ms: 0,
        timeout_ms: 100,
    }
}

#[test]
fn compatibility_uses_explicit_binary_capability_and_never_relaxes_paths() {
    let mut request = request();
    assert_eq!(
        stream_mode(&info(true), &request).unwrap(),
        StreamMode::Detailed
    );
    assert_eq!(
        stream_mode(&info(false), &request).unwrap(),
        StreamMode::Legacy
    );
    request.egress = Egress::Interface("test0".into());
    assert!(stream_mode(&info(false), &request)
        .unwrap_err()
        .to_string()
        .starts_with("Unsupported:"));
    request.egress = Egress::System;
    for server in ["127.0.0.1", "udp://127.0.0.1:53", "tcp://localhost:53"] {
        request.probe = Probe::Dns {
            name: "test.invalid".into(),
            record: "A".into(),
            server: server.into(),
        };
        assert_eq!(
            stream_mode(&info(false), &request).unwrap(),
            StreamMode::Legacy
        );
    }
    for server in [
        "tls://localhost",
        "https://localhost/dns-query",
        "udp://localhost#bootstrap=127.0.0.1",
    ] {
        request.probe = Probe::Dns {
            name: "test.invalid".into(),
            record: "A".into(),
            server: server.into(),
        };
        assert!(stream_mode(&info(false), &request).is_err());
    }
    let mut malformed = info(true);
    malformed.features.push(malformed.features[0].clone());
    assert!(stream_mode(&malformed, &request)
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
    malformed = info(true);
    malformed.build.name = "astra-server-core".into();
    assert!(stream_mode(&malformed, &request)
        .unwrap_err()
        .to_string()
        .contains("identity"));
}

#[test]
fn explicit_and_constrained_resolution_require_their_own_capability() {
    let mut request: DetailedRequest = request().into();
    assert!(!requires_resolution(&request));
    request.resolver = Some("udp://127.0.0.1:53".into());
    assert!(requires_resolution(&request));
    assert!(require_capability(
        &info(true),
        astra_model::control::NETWORK_DIAGNOSTICS_RESOLUTION_CAPABILITY
    )
    .is_err());
    request.resolver = None;
    request.egress = Egress::Interface("adapter0".into());
    assert!(!requires_resolution(&request));
    request.probe = Probe::Tcp {
        host: "target.invalid".into(),
        port: 443,
    };
    assert!(requires_resolution(&request));
    request.probe = Probe::Dns {
        name: "target.invalid".into(),
        record: "A".into(),
        server: "https://dns.invalid/dns-query#bootstrap=127.0.0.1".into(),
    };
    assert!(requires_resolution(&request));
    request.probe = Probe::Dns {
        name: "target.invalid".into(),
        record: "A".into(),
        server: "https://dns.invalid/dns-query".into(),
    };
    assert!(requires_resolution(&request));
}

#[tokio::test]
async fn cancellation_before_start_does_not_spawn_a_core() {
    let root = tempfile::tempdir().unwrap();
    let plan = DiagnosticPlan {
        runtime_dir: root.path().into(),
        log_path: root.path().join("core.log"),
        executable: root.path().join("nonexistent-core"),
        config_path: root.path().join("config.toml"),
        text: String::new(),
        running: false,
        control: None,
    };
    let request = Request {
        config: String::new(),
        egress: astra_model::nettool::Egress::System,
        probe: astra_model::nettool::Probe::Tcp {
            host: "127.0.0.1".into(),
            port: 9,
        },
        count: 1,
        interval_ms: 0,
        timeout_ms: 100,
    };
    let (output, _events) = mpsc::channel(1);
    let (_cancel, cancellation) = watch::channel(true);
    plan.network_diagnostic(request, output, cancellation)
        .await
        .unwrap();
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::time::Duration;

    fn fixture(script: &str) -> Child {
        tokio::process::Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    fn assert_reaped(child: &mut Child, pid: u32) {
        assert!(child.try_wait().unwrap().is_some());
        assert!(child.id().is_none());
        // The process is gone, rather than an uncollected zombie.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn capability_preflight_cancellation_reaps_the_process() {
        let mut child = fixture("exec sleep 30");
        let pid = child.id().unwrap();
        let (cancel, mut cancellation) = watch::channel(false);
        let operation = program_info(&mut child, &mut cancellation);
        let controller = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.send_replace(true);
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(operation, controller)
        })
        .await
        .unwrap();
        assert!(result.unwrap().is_none());
        assert_reaped(&mut child, pid);
    }

    #[tokio::test]
    async fn capability_output_failures_are_not_unsupported_or_legacy() {
        for (script, expected) in [
            ("printf 'bad JSON'; exit 0", "Invalid program-info protocol"),
            ("printf '%016385d' 0; exec sleep 30", "exceeds 16 KiB"),
            ("printf 'unknown argument' >&2; exit 2", "Unable to confirm"),
        ] {
            let mut child = fixture(script);
            let pid = child.id().unwrap();
            let (_cancel, mut cancellation) = watch::channel(false);
            let error = tokio::time::timeout(
                Duration::from_secs(3),
                program_info(&mut child, &mut cancellation),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert!(!error.to_string().starts_with("Unsupported:"));
            assert_reaped(&mut child, pid);
        }
    }

    #[tokio::test]
    async fn actual_binary_selects_stream_and_context_keeps_configuration_unused() {
        use std::os::unix::fs::PermissionsExt;
        for detailed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let binary = root.path().join("probe-core");
            let metadata = serde_json::to_string(&info(detailed))
                .unwrap()
                .replace('\'', "'\\''");
            let flag = if detailed {
                "--network-diagnostics-stream"
            } else {
                "--nettool-stream"
            };
            let script = format!("#!/bin/sh\nset -eu\nif [ \"$1\" = --program-info ]; then pwd > \"$ASTRA_RUNTIME_DIR/capability-cwd\"; printf '%s\\n' '{metadata}'; exit 0; fi\n[ \"$1\" = {flag} ]\n[ \"$(cat \"$ASTRA_RUNTIME_DIR/capability-cwd\")\" = \"$(pwd)\" ]\n[ -d \"$TMPDIR\" ]\npayload=$(cat)\ncase \"$payload\" in *'\"config\":\"\"'*) ;; *) exit 9;; esac\nprintf '%s\\n' '{{\"event\":\"done\"}}'\n");
            std::fs::write(&binary, script).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let plan = DiagnosticPlan {
                runtime_dir: root.path().into(),
                log_path: root.path().join("core.log"),
                executable: binary,
                config_path: root.path().join("config.toml"),
                text: "sensitive config".into(),
                running: false,
                control: None,
            };
            let (output, mut events) = mpsc::channel(4);
            let (_cancel, cancellation) = watch::channel(false);
            tokio::time::timeout(
                Duration::from_secs(3),
                plan.network_diagnostic(request(), output, cancellation),
            )
            .await
            .unwrap()
            .unwrap();
            let Event::Context(context) = events.recv().await.unwrap() else {
                panic!("missing context")
            };
            assert_eq!(context.probe_binary.name, "astra-proxy");
            assert_eq!(
                context.config_digest,
                Some(astra_ipc::control::config_digest("sensitive config").to_string())
            );
            assert_eq!(context.config_observation, "saved_not_applied");
            assert_eq!(context.egress_source, "requested");
            assert!(!context.config_used);
            assert!(context.selection_chain.is_empty());
            assert_eq!(context.legacy_details, !detailed);
            assert!(context.running_core.is_none());
            assert!(matches!(events.recv().await, Some(Event::Done)));
            assert!(!root.path().join("capability-cwd").exists());
            assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("nettool-")
            }));
        }
    }

    #[tokio::test]
    async fn context_pairs_live_snapshot_with_unchanged_core_identity() {
        use astra_model::control::{
            Applied, Command, ConfigSnapshot, Request as ControlRequest, Response,
        };
        for changed in [false, true] {
            let root = tempfile::Builder::new()
                .prefix("ctx-")
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
            let before = astra_platform::lifecycle::identity(Some(Applied {
                path: path.clone(),
                digest: astra_ipc::control::config_digest("active"),
            }))
            .unwrap();
            let mut after = before.clone();
            if changed {
                after.boot_id = "restarted".into();
            }
            let config = path.clone();
            let server = tokio::spawn(async move {
                for index in 0..3 {
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
                        assert!(matches!(request.command, Command::GetConfig));
                        astra_ipc::send_management(
                            &mut stream,
                            &astra_ipc::session::Event::Reply(Response::Config(Ok(
                                ConfigSnapshot {
                                    api_version: astra_ipc::API_VERSION,
                                    pid: std::process::id(),
                                    path: config.clone(),
                                    text: "active".into(),
                                },
                            ))),
                        )
                        .await
                        .unwrap();
                    } else {
                        assert!(matches!(request.command, Command::Identity));
                        astra_ipc::send_management(
                            &mut stream,
                            if index == 0 { &before } else { &after },
                        )
                        .await
                        .unwrap();
                    }
                }
            });
            let mut plan = DiagnosticPlan {
                runtime_dir: root.path().into(),
                log_path: root.path().join("core.log"),
                executable: root.path().join("unused"),
                config_path: path,
                text: "old cached text".into(),
                running: true,
                control: Some((socket, Some(std::process::id()))),
            };
            let context = tokio::time::timeout(
                Duration::from_secs(3),
                plan.diagnostic_context(&request(), info(true), StreamMode::Detailed),
            )
            .await
            .unwrap();
            server.await.unwrap();
            assert!(context.running_core.is_some());
            if changed {
                assert!(context.config_observation.starts_with("unconfirmed:"));
                assert_eq!(context.config_digest, None);
            } else {
                assert_eq!(context.config_observation, "matched_running");
                assert_eq!(
                    context.config_digest,
                    Some(astra_ipc::control::config_digest("active").to_string())
                );
            }
        }
    }

    #[tokio::test]
    async fn subprocess_cannot_inject_manager_context() {
        let root = tempfile::tempdir().unwrap();
        let mut plan = DiagnosticPlan {
            runtime_dir: root.path().into(),
            log_path: root.path().join("core.log"),
            executable: root.path().join("unused"),
            config_path: root.path().join("config.toml"),
            text: String::new(),
            running: false,
            control: None,
        };
        let context = plan
            .diagnostic_context(&request(), info(true), StreamMode::Detailed)
            .await;
        let injected = serde_json::to_string(&Event::Context(Box::new(context)))
            .unwrap()
            .replace('\'', "'\\''");
        let mut child = fixture(&format!("printf '%s\\n' '{injected}'; exec sleep 30"));
        let pid = child.id().unwrap();
        let (output, mut events) = mpsc::channel(1);
        let (_cancel, cancellation) = watch::channel(false);
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            run_child(&mut child, vec![], output, false, cancellation),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("cannot supply manager context"));
        assert!(events.recv().await.is_none());
        assert_reaped(&mut child, pid);
    }

    #[tokio::test]
    async fn cancellation_reaps_process_and_retains_received_results() {
        let mut child = fixture("printf '%s\n' '{\"event\":\"attempt\",\"seq\":1,\"rtt_ms\":1.0,\"detail\":\"local\",\"error\":null}'; exec sleep 30");
        let pid = child.id().unwrap();
        let (output, mut events) = mpsc::channel(4);
        let (cancel, cancellation) = watch::channel(false);
        let operation = run_child(&mut child, vec![], output, false, cancellation);
        let controller = async {
            assert!(matches!(events.recv().await, Some(Event::Attempt(_))));
            cancel.send_replace(true);
            assert!(events.recv().await.is_none());
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(operation, controller)
        })
        .await
        .unwrap();
        result.unwrap();
        assert_reaped(&mut child, pid);
    }

    #[tokio::test]
    async fn blocked_input_is_cancelled_and_reaped() {
        let mut child = fixture("exec sleep 30");
        let pid = child.id().unwrap();
        let (output, _events) = mpsc::channel(1);
        let (cancel, cancellation) = watch::channel(false);
        let operation = run_child(
            &mut child,
            vec![b'x'; 1024 * 1024],
            output,
            false,
            cancellation,
        );
        let controller = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.send_replace(true);
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(operation, controller)
        })
        .await
        .unwrap();
        result.unwrap();
        assert_reaped(&mut child, pid);
    }

    #[tokio::test]
    async fn protocol_failure_reaps_the_core() {
        for script in [
            "printf 'invalid\\n'; exec sleep 30",
            "printf '%061441d' 0; exec sleep 30",
        ] {
            let mut child = fixture(script);
            let pid = child.id().unwrap();
            let (output, _events) = mpsc::channel(1);
            let (_cancel, cancellation) = watch::channel(false);
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                run_child(&mut child, vec![], output, false, cancellation),
            )
            .await
            .unwrap();
            assert!(result.is_err());
            assert_reaped(&mut child, pid);
        }
    }

    #[tokio::test]
    async fn receiver_failure_reaps_the_core() {
        let mut child = fixture("printf '%s\n' '{\"event\":\"summary\",\"sent\":0,\"received\":0,\"min_ms\":null,\"avg_ms\":null,\"max_ms\":null}'; exec sleep 30");
        let pid = child.id().unwrap();
        let (output, events) = mpsc::channel(1);
        drop(events);
        let (_cancel, cancellation) = watch::channel(false);
        assert!(tokio::time::timeout(
            Duration::from_secs(3),
            run_child(&mut child, vec![], output, false, cancellation)
        )
        .await
        .unwrap()
        .is_err());
        assert_reaped(&mut child, pid);
    }

    #[tokio::test]
    async fn done_is_not_published_before_process_exit() {
        let mut child = fixture("printf '%s\n' '{\"event\":\"done\"}'; exec sleep 30");
        let pid = child.id().unwrap();
        let (output, mut events) = mpsc::channel(1);
        let (cancel, cancellation) = watch::channel(false);
        let operation = run_child(&mut child, vec![], output, false, cancellation);
        let controller = async {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), events.recv())
                    .await
                    .is_err()
            );
            // Closing the control channel is also a cancellation request.
            drop(cancel);
            assert!(events.recv().await.is_none());
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(operation, controller)
        })
        .await
        .unwrap();
        result.unwrap();
        assert_reaped(&mut child, pid);
    }

    #[tokio::test]
    async fn stderr_is_drained_beyond_the_retained_prefix() {
        let mut child = fixture("i=0; while [ \"$i\" -lt 64 ]; do printf '%04096d' 0 >&2; i=$((i+1)); done; printf '%s\n' '{\"event\":\"done\"}'");
        let pid = child.id().unwrap();
        let (output, mut events) = mpsc::channel(1);
        let (_cancel, cancellation) = watch::channel(false);
        tokio::time::timeout(
            Duration::from_secs(3),
            run_child(&mut child, vec![], output, false, cancellation),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(events.recv().await, Some(Event::Done)));
        assert_reaped(&mut child, pid);
    }

    #[tokio::test]
    async fn exit_failure_never_publishes_done() {
        let mut child =
            fixture("printf 'failure evidence' >&2; printf '%s\n' '{\"event\":\"done\"}'; exit 2");
        let (output, mut events) = mpsc::channel(1);
        let (_cancel, cancellation) = watch::channel(false);
        let error = run_child(&mut child, vec![], output, false, cancellation)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failure evidence"));
        assert!(events.recv().await.is_none());
    }
}
