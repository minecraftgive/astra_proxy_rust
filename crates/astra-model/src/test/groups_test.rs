use super::*;
#[test]
fn dynamic_members_merge_filter_deduplicate_and_recompute_without_materializing() {
    let mut groups = BTreeMap::from([(
        "region".into(),
        Group {
            members: vec!["manual".into(), "JP-a".into()],
            include_all: true,
            filter: Some("^JP-".into()),
            exclude_filter: Some("expired".into()),
            ..Default::default()
        },
    )]);
    let nodes = HashSet::from([
        "manual".into(),
        "JP-a".into(),
        "JP-b".into(),
        "JP-expired".into(),
        "US-c".into(),
    ]);
    ensure_default(&mut groups, &nodes).unwrap();
    let expanded = expand(&groups, &nodes).unwrap();
    assert_eq!(expanded["region"].members, ["manual", "JP-a", "JP-b"]);
    assert_eq!(
        groups["region"].members,
        ["manual", "JP-a"],
        "do not persist expansion"
    );
    let mut updated = nodes.clone();
    updated.remove("JP-b");
    updated.insert("JP-new".into());
    let expanded = expand(&groups, &updated).unwrap();
    assert_eq!(expanded["region"].members, ["manual", "JP-a", "JP-new"]);
    assert!(validate(&groups, &updated).is_ok());
    groups.get_mut("region").unwrap().filter = Some("[".into());
    assert!(validate(&groups, &nodes).is_err());
}
#[test]
fn dynamic_group_references_detect_cycles_and_empty_results_are_safe() {
    let nodes = HashSet::new();
    let mut groups = BTreeMap::new();
    ensure_default(&mut groups, &nodes).unwrap();
    groups.insert(
        "all-groups".into(),
        Group {
            include_all_groups: true,
            ..Default::default()
        },
    );
    assert_eq!(
        expand(&groups, &nodes).unwrap()["all-groups"].members,
        ["PROXY"]
    );
    assert!(validate(&groups, &nodes).is_ok());
    groups.insert(
        "cycle".into(),
        Group {
            members: vec!["all-groups".into()],
            ..Default::default()
        },
    );
    assert!(validate(&groups, &nodes).is_err());
    assert!(ensure_default(&mut BTreeMap::new(), &HashSet::from(["PROXY".into()])).is_err());
}
#[test]
fn direct_and_reject_are_builtin_groups_ending_in_their_own_outbound() {
    let nodes = HashSet::from(["a".into()]);
    let mut groups = BTreeMap::new();
    ensure_default(&mut groups, &nodes).unwrap();
    assert_eq!(groups["DIRECT"].members, ["DIRECT"]);
    assert_eq!(groups["REJECT"].members, ["REJECT"]);
    groups.insert(
        "all".into(),
        Group {
            include_all_groups: true,
            ..Default::default()
        },
    );
    assert_eq!(expand(&groups, &nodes).unwrap()["all"].members, ["PROXY"]);
    groups.get_mut("all").unwrap().members = vec!["DIRECT".into()];
    assert!(validate(&groups, &nodes).is_ok());
    assert_eq!(
        member_path(&groups, "all", "DIRECT").unwrap(),
        ["DIRECT"],
        "another group's DIRECT member is the DIRECT group"
    );
    // DIRECT → PROXY → DIRECT is a real cycle once DIRECT stops ending in itself.
    groups.get_mut("DIRECT").unwrap().members = vec!["PROXY".into()];
    groups.get_mut("PROXY").unwrap().members = vec!["DIRECT".into()];
    assert!(validate(&groups, &nodes).is_err());
    groups.get_mut("PROXY").unwrap().members.clear();
    groups.get_mut("all").unwrap().members = vec!["all".into()];
    assert!(
        validate(&groups, &nodes).is_err(),
        "only built-ins may list themselves"
    );
    for name in ["DIRECT", "REJECT", "PROXY"] {
        assert!(ensure_default(&mut BTreeMap::new(), &HashSet::from([name.into()])).is_err());
    }
}
#[test]
fn connection_policies_preserve_backward_compatible_defaults() {
    let group = Group::default();
    assert!(
        !group.disconnect_on_manual
            && !group.disconnect_on_auto
            && !group.disconnect_on_failure
            && !group.disconnect_on_recovery
            && !group.disconnect_on_config_change
    );
}
#[test]
fn references_cycles_and_selection_are_validated() {
    let nodes = HashSet::from(["a".into(), "b".into()]);
    let mut groups = BTreeMap::from([
        (
            "auto".into(),
            Group {
                mode: Mode::Auto,
                members: vec!["a".into(), "b".into()],
                ..Default::default()
            },
        ),
        (
            "main".into(),
            Group {
                members: vec!["auto".into(), "DIRECT".into()],
                selected: Some("auto".into()),
                ..Default::default()
            },
        ),
    ]);
    assert_eq!(validate(&groups, &nodes).unwrap(), ["auto", "main"]);
    groups.get_mut("auto").unwrap().members.push("main".into());
    assert!(validate(&groups, &nodes).is_err());
    groups.get_mut("auto").unwrap().members = vec!["missing".into()];
    assert!(validate(&groups, &nodes).is_err());
    groups.get_mut("auto").unwrap().members = vec!["a".into()];
    groups.get_mut("main").unwrap().selected = Some("b".into());
    assert!(validate(&groups, &nodes).is_err());
}
