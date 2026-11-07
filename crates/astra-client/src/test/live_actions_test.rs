use super::*;
use crate::{
    protocol::{Instance, Kind, Row},
    state::Operation,
};
use astra_model::runtime::{Connection, Group, Member};
fn client() -> Client {
    let mut client = Client::new(Kind::Proxy);
    client.rows = vec![Row {
        instance: Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: "proxy.toml".into(),
        },
        pid: Some(42),
        state: "Running".into(),
        uptime: 0,
        changed: false,
    }];
    client.accept_runtime(
        "proxy".into(),
        Snapshot {
            pid: 42,
            groups: vec![Group {
                name: "PROXY".into(),
                mode: "select".into(),
                selected: Some("first".into()),
                testing: false,
                members: vec![
                    Member {
                        name: "first".into(),
                        delay_ms: Some(10),
                        alive: Some(true),
                    },
                    Member {
                        name: "second".into(),
                        delay_ms: None,
                        alive: None,
                    },
                ],
            }],
            connections: vec![Connection {
                id: 7,
                host: "example.com".into(),
                port: 443,
                source: None,
                network: "Tcp".into(),
                inbound: "mixed".into(),
                outbound: "first".into(),
                upload: 1,
                download: 2,
                elapsed_ms: 3000,
            }],
            ..Default::default()
        },
    );
    client
}
#[test]
fn optimistic_member_selection_is_bound_to_the_instance_pid_and_confirmed_by_snapshot() {
    let mut client = client();
    let action = match client.select_proxy_member("PROXY", "second") {
        Input::Run(action, _) => action,
        _ => panic!("selection action"),
    };
    assert!(
        matches!(&action,Action::Runtime{name,expected_pid:Some(42),command:Command::Select{group,member}}if name=="proxy"&&group=="PROXY"&&member=="second")
    );
    assert!(client.proxy_member_pending("PROXY", "second"));
    let snapshot = client.live().unwrap().clone();
    client.accept_runtime("proxy".into(), snapshot.clone());
    assert!(client.proxy_member_pending("PROXY", "second"));
    let mut confirmed = snapshot;
    confirmed.groups[0].selected = Some("second".into());
    client.accept_runtime("proxy".into(), confirmed);
    assert!(client.pending_proxy_selection.is_none());
    client.select_proxy_member("PROXY", "first");
    client.reject_proxy_selection(&action);
    assert!(client.proxy_member_pending("PROXY", "first"));
    client.activate_profile("different".into());
    assert!(client.pending_proxy_selection.is_none());
}
#[test]
fn automatic_groups_do_not_dispatch_manual_selection_or_mark_it_pending() {
    let mut client = client();
    client.runtime_snapshot.as_mut().unwrap().groups[0].mode = "url-test".into();
    assert!(matches!(
        client.select_proxy_member("PROXY", "second"),
        Input::Idle
    ));
    assert!(client.pending_proxy_selection.is_none());
    assert_eq!(
        client.message,
        crate::i18n::t("Automatic groups choose members by latency")
    );
}
#[test]
fn connection_context_menu_target_cannot_close_a_reused_id_after_host_restart() {
    let mut client = client();
    let target = client
        .connection_action_target(&ConnectionRowKey::Connection(7))
        .unwrap();
    assert!(matches!(
        client.close_connection_target(target.clone()),
        Input::Run(
            Action::Runtime {
                expected_pid: Some(42),
                command: Command::Disconnect(Some(7)),
                ..
            },
            _
        )
    ));
    client.rows[0].pid = Some(43);
    let mut replacement = client
        .live()
        .cloned()
        .unwrap_or_else(|| client.runtime_snapshot.clone().unwrap());
    replacement.pid = 43;
    client.accept_runtime("proxy".into(), replacement);
    assert!(matches!(
        client.close_connection_target(target),
        Input::Idle
    ));
    assert!(client
        .connection_action_target(&ConnectionRowKey::Target(ConnectionTarget {
            host: "example.com".into(),
            port: 443
        }))
        .is_none());
    client.runtime_error = "offline".into();
    assert!(matches!(client.close_connection(7), Input::Idle));
}
#[test]
fn paused_connection_keeps_identity_and_revalidates_against_current_live_sample() {
    let mut client = client();
    client.toggle_live_pause();
    let target = client
        .connection_action_target(&ConnectionRowKey::Connection(7))
        .unwrap();
    client
        .runtime_snapshot
        .as_mut()
        .unwrap()
        .connections
        .clear();
    assert!(matches!(
        client.close_connection_target(target),
        Input::Idle
    ));
}
#[test]
fn server_actions_never_fall_back_to_an_active_proxy() {
    let mut client = client();
    assert!(matches!(
        client.server_action(crate::intents::ServerAction::Toggle),
        Input::Idle
    ));
    client.rows.push(Row {
        instance: Instance {
            name: "server".into(),
            kind: Kind::Server,
            config: "server.toml".into(),
        },
        pid: Some(9),
        state: "Running".into(),
        uptime: 0,
        changed: false,
    });
    client.select_instance(Kind::Server, "server");
    assert!(
        matches!(client.server_action(crate::intents::ServerAction::Toggle),Input::Run(Action::Stop(name),_)if name=="server")
    );
    assert!(client.operations().iter().all(|operation| !matches!(
        operation,
        Operation::Bind | Operation::Diagnostics | Operation::NetTools | Operation::ForceStart
    )));
}
