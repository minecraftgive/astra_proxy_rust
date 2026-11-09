use super::*;
use serde_json::json;

fn view() -> View {
    View::new(Path::new("unused.sock"), "test".into(), &[])
}
fn control(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

#[test]
fn conclusions_are_default_and_toggle_keeps_evidence_and_request_unchanged() {
    let mut v = view();
    v.details = vec![
        json!({"event":"tcp","result":{"host":"example.test","port":443,"outcome":"connected","connect_ms":18,"message":"PRIVATE TECHNICAL DETAIL"}}),
        json!({"event":"context","config_digest":"PRIVATE DIGEST"}),
    ];
    let request = serde_json::to_value(v.request().unwrap()).unwrap();
    let export = v.export_json().unwrap();
    assert!(!v.show_advanced);
    assert!(v.output().join("\n").contains("连接成功"));
    assert!(!v.output().join("\n").contains("PRIVATE"));
    assert_eq!(v.output_rows()[0].tone, OutputTone::Success);
    v.scroll = 70;
    assert!(v.key(control(KeyCode::Char('d'))));
    assert!(v.show_advanced);
    assert_eq!(v.scroll, 0);
    assert!(v.output().join("\n").contains("PRIVATE DIGEST"));
    assert!(v.output().join("\n").contains("PRIVATE TECHNICAL DETAIL"));
    assert_eq!(export, v.export_json().unwrap());
    assert_eq!(request, serde_json::to_value(v.request().unwrap()).unwrap());
    assert!(v.key(control(KeyCode::Char('d'))));
    assert!(!v.output().join("\n").contains("PRIVATE"));
}

#[test]
fn result_columns_keep_conclusions_separate_from_complete_report() {
    let mut v = view();
    v.details = vec![
        json!({"event":"tcp","result":{"host":"example.test","port":443,"outcome":"connected","message":"REPORT ONLY DETAIL"}}),
        json!({"event":"context","config_digest":"REPORT ONLY DIGEST"}),
    ];
    let conclusions = v.conclusion_rows();
    let report = v.report_rows();
    assert!(conclusions.iter().any(|r| r.text.contains("连接成功")));
    assert!(!conclusions.iter().any(|r| r.text.contains("REPORT ONLY")));
    assert!(report.iter().any(|r| r.text.contains("REPORT ONLY DETAIL")));
    assert!(report.iter().any(|r| r.text.contains("REPORT ONLY DIGEST")));
    v.toggle_advanced();
    assert_eq!(conclusions, v.conclusion_rows());
    assert_eq!(report, v.report_rows());
    assert!(v.export_json().unwrap().contains("REPORT ONLY DIGEST"));
}

#[test]
fn errors_warnings_and_partial_evidence_are_visible_without_expanding() {
    let mut v = view();
    v.error = Some("取消清理尚未确认".into());
    v.context.notice = Some("旧版不支持取消确认".into());
    v.report_state = Some(astra_management::v2::NetworkState::Partial);
    v.details.push(json!({"event":"analysis","incomplete":true,"findings":[{"severity":"needs_verification","title":"DNS 不一致","explanation":"需要对照","recommendation":"换一条路径复测","evidence":[{"event_index":77}]}]}));
    let rows = v.output_rows();
    for (tone, fragment) in [
        (OutputTone::Error, "取消清理"),
        (OutputTone::Warning, "旧版"),
        (OutputTone::Warning, "部分完成"),
        (OutputTone::Warning, "DNS 不一致"),
        (OutputTone::Info, "换一条路径"),
    ] {
        assert!(
            rows.iter()
                .any(|r| r.tone == tone && r.text.contains(fragment)),
            "{fragment}"
        );
    }
    assert!(!v.output().join("\n").contains("event_index"));
}

#[test]
fn nat_type_names_require_complete_stable_capability_evidence() {
    for (code, label) in [
        ("symmetric_observed", "对称型"),
        ("full_cone_observed", "完全锥型"),
        ("restricted_cone_observed", "受限锥型"),
        ("port_restricted_cone_observed", "端口受限锥型"),
        ("no_translation_observed", "未观察到地址转换"),
    ] {
        let mut v = view();
        let report = json!({"event":"nat","kind":"report","report":{"traditional_type":code,"advanced_capable":true,"unstable":false,"partial":false,"physical_path":{"binding_verified":true,"route_verified":true},"interface_name":"en0"}});
        v.details = vec![report.clone()];
        assert!(v.output().join("\n").contains(label));
        assert!(!v.output().join("\n").contains(code));
        for (field, value) in [
            ("advanced_capable", false),
            ("unstable", true),
            ("partial", true),
        ] {
            v.details = vec![report.clone()];
            v.details[0]["report"][field] = json!(value);
            let output = v.output().join("\n");
            assert!(output.contains("未能确定"));
            assert!(!output.contains(&format!("NAT 类型：{label}")));
        }
    }
    let mut v = view();
    v.set_kind(Kind::Nat);
    assert!(
        v.nat_advanced,
        "type detection must be requested by default"
    );
    v.toggle_advanced();
    assert!(
        v.nat_advanced,
        "display and detection settings are independent"
    );
}

#[test]
fn non_function_shortcuts_work_and_f_keys_do_nothing() {
    let mut v = view();
    v.field = Field::Target;
    let target = v.target.clone();
    for n in 1..=12 {
        assert!(v.key(KeyCode::F(n).into()));
        assert!(v.section == Section::Tools);
        assert!(v.plan.projects.is_empty());
        assert!(!v.show_advanced && !v.running());
    }
    v.key(control(KeyCode::Char('l')));
    assert!(v.section == Section::Paths);
    v.key(control(KeyCode::Left));
    assert!(v.section == Section::Tools);
    v.key(control(KeyCode::Up));
    assert_eq!(v.scroll, 10);
    v.key(control(KeyCode::Down));
    assert_eq!(v.scroll, 0);
    v.key(control(KeyCode::Char('p')));
    assert_eq!(v.plan.projects.len(), 1);
    v.key(control(KeyCode::Char('r')));
    assert_eq!(v.plan.projects.len(), 6);
    assert!(v.section == Section::Findings);
    assert_eq!(v.target, target);
}

#[test]
fn completed_execution_never_overrides_failed_probe_color() {
    let mut v = view();
    v.report_state = Some(astra_management::v2::NetworkState::Complete);
    v.details = vec![
        json!({"event":"tcp","result":{"host":"example.test","port":443,"outcome":"refused"}}),
    ];
    let rows = v.output_rows();
    assert!(!rows.iter().any(|r| r.tone == OutputTone::Success));
    assert!(rows
        .iter()
        .any(|r| r.tone == OutputTone::Error && r.text.contains("拒绝")));
}

#[test]
fn project_context_keeps_different_paths_and_rounds_separate() {
    let mut v = view();
    for (path, round) in [("path-one", 1), ("path-two", 2)] {
        v.details.push(json!({"event":"project","project":{"project_id":"tcp","path_id":path,"round":round},"detail":{"event":"tcp","result":{"host":"a.test","port":443,"outcome":"connected"}}}));
    }
    let rows = v.output_rows();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].text.contains("path-one · 第 1 轮"));
    assert!(rows[1].text.contains("path-two · 第 2 轮"));
}

#[test]
fn packet_counts_do_not_override_tool_outcomes() {
    let mut v = view();
    v.details = vec![
        json!({"event":"nat","kind":"report","report":{"advanced_capable":false,"partial":true,"unstable":false}}),
        json!({"event":"summary","sent":2,"received":2}),
    ];
    let rows = v.output_rows();
    assert!(rows
        .iter()
        .any(|r| r.tone == OutputTone::Warning && r.text.contains("未能确定")));
    assert!(!rows.iter().any(|r| r.tone == OutputTone::Success));
    v.details = vec![
        json!({"event":"trace_summary","target":"example.test","reached":true}),
        json!({"event":"summary","sent":0,"received":0}),
    ];
    let rows = v.output_rows();
    assert!(rows
        .iter()
        .any(|r| r.tone == OutputTone::Success && r.text.contains("已到达")));
    assert!(!rows
        .iter()
        .any(|r| matches!(r.tone, OutputTone::Warning | OutputTone::Error)));
}

#[test]
fn only_current_project_status_is_shown_but_history_is_retained() {
    let mut v = view();
    for state in ["queued", "running", "complete"] {
        v.details.push(json!({"event":"project_status","project_id":"one","path_id":"path-1","round":1,"state":state,"message":state}));
    }
    let output = v.output().join("\n");
    assert!(output.contains("complete"));
    assert!(!output.contains("queued") && !output.contains("running"));
    v.toggle_advanced();
    let output = v.output().join("\n");
    assert!(output.contains("queued") && output.contains("running"));
    assert_eq!(v.details.len(), 3);
}

#[test]
fn explicit_stun_rejection_and_incomplete_dns_remain_visible() {
    let mut v = view();
    v.details = vec![
        json!({"event":"stun","observation":{"response_received":true,"mapped_endpoint":null,"error":"STUN permission denied"}}),
    ];
    let rows = v.output_rows();
    assert!(rows
        .iter()
        .any(|r| r.tone == OutputTone::Error && r.text.contains("STUN permission denied")));
    for (parsed, truncated) in [(false, false), (true, true)] {
        v.details = vec![
            json!({"event":"dns","detail":{"kind":"response","outcome":"resolved","parsed":parsed,"truncated":truncated,"answer_count":1}}),
        ];
        let rows = v.output_rows();
        assert!(!rows.iter().any(|r| r.tone == OutputTone::Success));
        assert!(rows.iter().any(|r| r.tone == OutputTone::Warning));
    }
}
