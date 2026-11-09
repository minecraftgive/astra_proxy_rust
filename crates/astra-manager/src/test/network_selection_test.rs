use super::*;
use astra_model::runtime::{Group, Member};

const CONFIG: &str = r#"
standalone=true
[[outbounds]]
kind="socks5"
tag="node-a"
server="127.0.0.1:1080"
[[outbounds]]
kind="socks5"
tag="node-b"
server="127.0.0.1:1081"
[proxy_groups.outer]
mode="select"
members=["inner"]
interface="outer0"
[proxy_groups.inner]
mode="auto"
members=["node-a","node-b"]
interface="inner0"
[tun]
interface="default0"
auto_interface=false
"#;

fn group(name: &str, mode: &str, selected: &str, members: &[&str]) -> Group {
    Group {
        name: name.into(),
        mode: mode.into(),
        selected: Some(selected.into()),
        testing: false,
        members: members
            .iter()
            .map(|name| Member {
                name: (*name).into(),
                delay_ms: None,
                alive: None,
            })
            .collect(),
    }
}
fn runtime() -> Snapshot {
    Snapshot {
        pid: std::process::id(),
        tun: true,
        groups: vec![
            group("outer", "select", "inner", &["inner"]),
            group("inner", "url-test", "node-a", &["node-a", "node-b"]),
        ],
        ..Default::default()
    }
}

#[test]
fn reconstructs_every_layer_and_outer_binding_wins() {
    let pinned = rebuild(CONFIG, "outer", Some(&runtime())).unwrap();
    assert_eq!(pinned.leaf_tag, "node-a");
    assert_eq!(pinned.effective_interface.as_deref(), Some("outer0"));
    assert_eq!(pinned.interface_source.as_deref(), Some("group:outer"));
    assert_eq!(
        pinned
            .selection_chain
            .iter()
            .map(|h| h.group.as_str())
            .collect::<Vec<_>>(),
        ["outer", "inner"]
    );
    assert_eq!(
        pinned.selection_chain[1].interface.as_deref(),
        Some("inner0")
    );
    assert_eq!(pinned.node.as_ref().unwrap().tag(), "node-a");
    let no_outer = CONFIG.replace("interface=\"outer0\"", "");
    assert_eq!(
        rebuild(&no_outer, "outer", Some(&runtime()))
            .unwrap()
            .effective_interface
            .as_deref(),
        Some("inner0")
    );
    let no_groups = no_outer.replace("interface=\"inner0\"", "");
    assert_eq!(
        rebuild(&no_groups, "outer", Some(&runtime()))
            .unwrap()
            .interface_source
            .as_deref(),
        Some("config.tun.interface")
    );
}

#[test]
fn outer_descendant_pin_keeps_skipped_group_bindings_without_using_its_selection() {
    let mut snapshot = runtime();
    snapshot.groups[0].selected = Some("node-a".into());
    snapshot.groups[1].selected = Some("node-b".into());
    let pinned = rebuild(
        &CONFIG.replace("interface=\"outer0\"", ""),
        "outer",
        Some(&snapshot),
    )
    .unwrap();
    assert_eq!(pinned.leaf_tag, "node-a");
    assert_eq!(pinned.selection_chain[0].selected, "inner");
    assert_eq!(pinned.selection_chain[1].selected, "node-a");
    assert_eq!(
        pinned.selection_chain[1].source,
        "outer_selection:outer:node-a"
    );
    assert_eq!(pinned.effective_interface.as_deref(), Some("inner0"));
}

#[test]
fn incomplete_unknown_balanced_or_changed_paths_are_never_guessed() {
    let mut bad = runtime();
    bad.truncated = true;
    assert!(rebuild(CONFIG, "outer", Some(&bad))
        .unwrap_err()
        .to_string()
        .contains("truncated"));
    bad = runtime();
    bad.groups.pop();
    assert!(rebuild(CONFIG, "outer", Some(&bad))
        .unwrap_err()
        .to_string()
        .contains("Missing runtime"));
    bad = runtime();
    bad.groups[1].selected = None;
    assert!(rebuild(CONFIG, "outer", Some(&bad)).is_err());
    bad = runtime();
    bad.groups[1].selected = Some("subscription-not-in-source".into());
    assert!(rebuild(CONFIG, "outer", Some(&bad)).is_err());
    bad = runtime();
    bad.groups[1].members.pop();
    assert!(rebuild(CONFIG, "outer", Some(&bad))
        .unwrap_err()
        .to_string()
        .contains("membership"));
    bad = runtime();
    bad.groups[1].mode = "fallback".into();
    assert!(rebuild(CONFIG, "outer", Some(&bad))
        .unwrap_err()
        .to_string()
        .contains("mode"));
    bad.groups[1].mode = "load-balance".into();
    assert!(rebuild(
        &CONFIG.replace("mode=\"auto\"", "mode=\"load_balance\""),
        "outer",
        Some(&bad)
    )
    .unwrap_err()
    .to_string()
    .contains("Load-balance"));
    let first = rebuild(CONFIG, "outer", Some(&runtime())).unwrap();
    bad = runtime();
    bad.groups[1].selected = Some("node-b".into());
    assert!(!same_path(
        &first,
        &rebuild(CONFIG, "outer", Some(&bad)).unwrap()
    ));
}

#[test]
fn concrete_node_and_direct_need_no_group_choice_but_keep_default_binding() {
    let mut snapshot = runtime();
    snapshot.truncated = true;
    let pinned = rebuild(CONFIG, "node-a", Some(&snapshot)).unwrap();
    assert!(pinned.selection_chain.is_empty());
    assert_eq!(pinned.effective_interface.as_deref(), Some("default0"));
    assert_eq!(rebuild(CONFIG, "node-a", None).unwrap().leaf_tag, "node-a");
    let direct = rebuild(CONFIG, "DIRECT", None).unwrap();
    assert_eq!(direct.leaf_tag, "DIRECT");
    assert!(direct.node.is_none());
    assert!(rebuild(CONFIG, "outer", None).is_err());
    assert!(rebuild(CONFIG, "REJECT", None).is_err());
}

#[test]
fn runtime_tun_state_controls_fallback_and_auto_interface_is_not_invented() {
    let mut snapshot = runtime();
    snapshot.tun = false;
    assert_eq!(
        rebuild(CONFIG, "node-a", Some(&snapshot))
            .unwrap()
            .effective_interface,
        None
    );
    let automatic = CONFIG.replace("auto_interface=false", "auto_interface=true");
    assert!(rebuild(&automatic, "node-a", Some(&runtime()))
        .unwrap_err()
        .to_string()
        .contains("automatic"));
    assert_eq!(
        rebuild(&automatic, "outer", Some(&runtime()))
            .unwrap()
            .effective_interface
            .as_deref(),
        Some("outer0")
    );
    let no_tun = CONFIG.split("[tun]").next().unwrap();
    assert!(rebuild(no_tun, "node-a", Some(&runtime())).is_err());
}

#[test]
fn unversioned_subscription_nodes_and_parse_errors_do_not_expose_credentials() {
    let source = CONFIG.to_owned()
        + "\n[subscription]\nurl='https://subscription.invalid/feed'\nmanaged_nodes=['node-a']\n";
    assert!(rebuild(&source, "node-a", Some(&runtime()))
        .unwrap_err()
        .to_string()
        .contains("effective configuration version"));
    let error = rebuild(
        "[[outbounds]]\npassword=\"NEVER_REPORT_THIS_SECRET",
        "node",
        None,
    )
    .unwrap_err();
    assert!(!format!("{error:#}").contains("NEVER_REPORT_THIS_SECRET"));
    assert!(!format!("{error:?}").contains("NEVER_REPORT_THIS_SECRET"));
}
