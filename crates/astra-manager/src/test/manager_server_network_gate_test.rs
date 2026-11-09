use super::*;
use std::os::unix::fs::PermissionsExt;

fn fixture(root: &std::path::Path) -> (Arc<Hub>, Arc<crate::network::Scheduler>, PathBuf) {
    private_dir(&root.join("state")).unwrap();
    let mut manager = Manager::load_with_runtime(
        root.join("state"),
        root.join("config"),
        root.join("bin"),
        Some(root.join("runtime")),
    )
    .unwrap();
    fs::create_dir_all(&manager.bin).unwrap();
    let config = root.join("profile.toml");
    fs::write(&config, "standalone=true").unwrap();
    manager.registry.instances.push(Instance {
        name: "proxy".into(),
        kind: Kind::Proxy,
        config,
    });
    let marker = root.join("probe-pids");
    let marker_text = marker.to_string_lossy().replace('\'', "'\\''");
    let script = format!(
        r##"#!/bin/sh
set -eu
echo $$ >> '{marker_text}'
if [ "$1" = --probe-serve ]; then
 read config
 read request
 printf '{{"outbound":"DIRECT","direct":true,"connect_ms":%s,"elapsed_ms":0,"ip":null,"error":null}}\n' $$
 exec sleep 30
fi
cat >/dev/null
printf '%s\n' '{{"event":"done"}}'
"##
    );
    let binary = manager.bin.join(Kind::Proxy.binary());
    fs::write(&binary, script).unwrap();
    fs::set_permissions(binary, fs::Permissions::from_mode(0o700)).unwrap();
    let hub = Hub::new(&manager).unwrap();
    let db = crate::database::Database::open(root.join("jobs.sqlite"), 10000).unwrap();
    let scheduler = crate::network::Scheduler::new(db, root.to_path_buf());
    hub.network_scheduler.set(scheduler.clone()).ok().unwrap();
    (hub, scheduler, marker)
}

#[tokio::test]
async fn manager_socket_and_header_probes_share_agent_nat_exclusion_and_reap_before_release() {
    let root = tempfile::tempdir().unwrap();
    let (hub, scheduler, marker) = fixture(root.path());
    let (started, mut ready) = watch::channel(false);
    scheduler
        .submit_mode(
            "nat".into(),
            "nat".into(),
            true,
            move |_, mut stop| async move {
                started.send_replace(true);
                crate::network::cancelled(&mut stop).await;
                Ok(())
            },
        )
        .await
        .unwrap();
    ready.wait_for(|v| *v).await.unwrap();
    let (sender, _requests) = mpsc::channel(1);
    let (stopped, _stop) = mpsc::channel(1);
    let native = NativeClient {
        hub: hub.clone(),
        sender: sender.clone(),
        stopped: stopped.clone(),
        diagnostics: Arc::new(Semaphore::new(2)),
        probes: Default::default(),
    };
    let header = tokio::spawn(async move {
        native
            .exit_probe(
                "proxy",
                astra_model::diagnostics::ProbeRequest {
                    outbound: "DIRECT".into(),
                    ip: false,
                },
                Some(Duration::from_secs(60)),
            )
            .await
    });
    let (mut client, server) = astra_platform::platform::stream_pair().await.unwrap();
    let serving = tokio::spawn(serve(
        server,
        hub,
        sender,
        stopped,
        Arc::new(Semaphore::new(2)),
        Arc::new(Semaphore::new(2)),
    ));
    let request = astra_model::nettool::Request {
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
    astra_ipc::send_management(
        &mut client,
        &Request {
            version: astra_ipc::API_VERSION,
            action: Action::NetTool {
                name: "proxy".into(),
                request,
            },
        },
    )
    .await
    .unwrap();
    assert!(timeout(
        Duration::from_millis(30),
        astra_ipc::receive_management::<Reply>(&mut client)
    )
    .await
    .is_err());
    assert!(!marker.exists(), "Legacy/header probe bypassed NAT");
    scheduler.cancel("nat").await.unwrap();
    let reply: Reply = timeout(
        Duration::from_secs(3),
        astra_ipc::receive_management(&mut client),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.error.is_none(), "{reply:?}");
    drop(client);
    timeout(Duration::from_secs(3), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let probe = timeout(Duration::from_secs(3), header)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let pid = probe.connect_ms.unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 2);
    assert!(scheduler.legacy_permit().await.is_ok());
}

#[tokio::test]
async fn dropped_reader_keeps_legacy_permit_until_owned_child_is_reaped() {
    let root = tempfile::tempdir().unwrap();
    let (hub, scheduler, marker) = fixture(root.path());
    let binary = hub.bin.join(Kind::Proxy.binary());
    let marker_text = marker.to_string_lossy().replace('\'', "'\\''");
    fs::write(binary,format!("#!/bin/sh\ncat >/dev/null\necho $$ > '{marker_text}'\nprintf '%s\\n' 'bad JSON'\nexec sleep 30\n")).unwrap();
    let (tx, _rx) = mpsc::channel(1);
    let (stop, _stopped) = mpsc::channel(1);
    let native = NativeClient {
        hub,
        sender: tx,
        stopped: stop,
        diagnostics: Arc::new(Semaphore::new(2)),
        probes: Default::default(),
    };
    let (output, _events) = mpsc::channel(1);
    let request = astra_model::nettool::Request {
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
    assert!(timeout(
        Duration::from_secs(3),
        native.nettool("proxy", request, output)
    )
    .await
    .unwrap()
    .is_err());
    let pid: i32 = fs::read_to_string(marker).unwrap().trim().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert!(scheduler.legacy_permit().await.is_ok());
}
