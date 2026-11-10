use super::*;
use gpui::{TestAppContext, WindowHandle};
use gpui_component::Root;
use std::path::Path;

fn fixture(cx: &mut TestAppContext) -> (Entity<AppModel>, Entity<View>, WindowHandle<Root>) {
    cx.update(gpui_component::init);
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(astra_client::protocol::Kind::Proxy, commands));
    model.update(cx, |model, _| {
        model.tokio(|client| {
            let mut diagnostics = astra_client::diagnostics::View::start(
                Path::new("unused-native-nettools.sock"),
                "fixture".into(),
            );
            if let Some(task) = diagnostics.task.take() {
                task.abort();
            }
            diagnostics.receiver = None;
            diagnostics.open_tools();
            client.diagnostics = Some(diagnostics);
        })
    });
    let mut view = None;
    let window = cx.add_window(|window, cx| {
        let entity = cx.new(|cx| View::new(model.clone(), window, cx));
        view = Some(entity.clone());
        window.focus(&entity.focus_handle(cx), cx);
        Root::new(entity, window, cx)
    });
    cx.run_until_parked();
    (model, view.unwrap(), window)
}

fn tools_mut(
    model: &Entity<AppModel>,
    cx: &mut TestAppContext,
    edit: impl FnOnce(&mut astra_client::nettools::View),
) {
    model.update(cx, |model, cx| {
        edit(
            model
                .client
                .diagnostics
                .as_mut()
                .unwrap()
                .tools
                .as_mut()
                .unwrap(),
        );
        cx.notify();
    });
}

#[gpui::test]
fn all_tools_sync_inputs_and_render_in_compact_native_window(cx: &mut TestAppContext) {
    let (model, view, window) = fixture(cx);
    for kind in Kind::ALL {
        window
            .update(cx, |_, window, cx| {
                window.resize(gpui::size(px(420.), px(460.)));
                view.update(cx, |view, cx| view.set_kind(kind, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        model.read_with(cx, |model, _| {
            assert_eq!(
                model
                    .client
                    .diagnostics
                    .as_ref()
                    .unwrap()
                    .tools
                    .as_ref()
                    .unwrap()
                    .kind,
                kind
            )
        });
        view.read_with(cx, |view, cx| {
            for (field, input) in &view.inputs {
                assert_eq!(
                    input.read(cx).value().as_ref(),
                    view.tools(cx, |v| v.field_value(*field)).unwrap().as_str(),
                    "{kind:?}/{field:?}"
                );
            }
        });
    }
}

#[gpui::test]
fn live_chain_and_single_result_column_render_together(cx: &mut TestAppContext) {
    let (model, _view, window) = fixture(cx);
    tools_mut(&model, cx, |tools| {
        tools.context.runtime = Some(serde_json::json!({
            "mode": "rule", "tun": true, "system_proxy": false,
            "groups": [{"name": "PROXY", "mode": "select", "selected": "hk-01"}]
        }));
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| {
        window.resize(gpui::size(px(420.), px(560.)));
        window.draw(cx).clear(cx);
    });
    let chain = visual
        .debug_bounds("network-live-chain")
        .expect("live chain is always visible");
    let result = visual
        .debug_bounds("network-result")
        .expect("one result column");
    assert!(chain.origin.y < result.origin.y);
    model.read_with(&visual, |model, _| {
        let tools = model
            .client
            .diagnostics
            .as_ref()
            .unwrap()
            .tools
            .as_ref()
            .unwrap();
        assert_eq!(
            tools.live_chain().text(),
            "系统 › TUN › 规则 › PROXY → hk-01 › hk-01"
        );
    });
}

fn topology_fixture() -> Vec<serde_json::Value> {
    use serde_json::json;
    let reference = json!({"project_id":"dns-local","path_id":"physical","round":1,"event_index":3,"sampled_at_ms":"1794220733827","source":"core DNS response"});
    vec![
        json!({"event":"project","project":{"project_id":"dns-local","path_id":"physical","round":1,"index":3,"sampled_at_ms":"1794220733827","comparison_requested":false},"detail":{"event":"dns","seq":1,"detail":{"kind":"records","records":[{"text":"192.0.2.10"}]}}}),
        json!({"event":"analysis","rules_version":1,"comparison_requested":false,"comparison_status":"未请求对照","findings":[],"topology":[{"path_id":"physical","nodes":[{"id":"client","label":"Agent 物理网卡","role":"客户端"},{"id":"resolver","label":"192.0.2.53","role":"解析器"},{"id":"target","label":"example.test","role":"业务目标"}],"edges":[{"from":"client","to":"resolver","section":"dns","level":"measured","description":"实际 UDP 查询","evidence":[reference]},{"from":"client","to":"target","section":"connection","level":"configuration","description":"所选 DIRECT 配置；未确认连接","evidence":[]},{"from":"client","to":"target","section":"trace","level":"unknown","description":"逐跳未测量","evidence":[]}],"attachments":[{"kind":"nat","label":"所测映射相同（非 NAT 类型）","evidence":[]}],"truncated":false}],"visibility":[],"incomplete":true,"truncated":false,"notes":[]}),
    ]
}

#[gpui::test]
fn probe_path_pointer_opens_nodes_edges_and_original_project_evidence(cx: &mut TestAppContext) {
    let (model, view, window) = fixture(cx);
    tools_mut(&model, cx, |tools| {
        tools.details = topology_fixture();
        tools.operation_id = Some("topology-report".into());
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| {
        window.resize(gpui::size(px(420.), px(560.)));
        window.draw(cx).clear(cx);
    });
    // Connection row first: [client] [┄›] [target]; then the DNS row.
    let bounds = visual
        .debug_bounds("network-path-1")
        .expect("connection edge has a hit target");
    visual.simulate_click(bounds.center(), Default::default());
    visual.run_until_parked();
    view.read_with(&visual, |view, _| {
        assert_eq!(
            view.path.selection,
            Some(path::Selection {
                path: 0,
                item: path::Item::Edge(1)
            })
        )
    });
    view.update(&mut visual, |view, cx| {
        view.path.selection = None;
        cx.notify();
    });
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    let bounds = visual
        .debug_bounds("network-path-0")
        .expect("source node has a hit target");
    visual.simulate_click(bounds.center(), Default::default());
    view.read_with(&visual, |view, _| {
        assert_eq!(
            view.path.selection,
            Some(path::Selection {
                path: 0,
                item: path::Item::Node("client".into())
            })
        )
    });
    tools_mut(&model, &mut visual, |tools| {
        tools.operation_id = Some("next-report".into());
    });
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    view.read_with(&visual, |view, _| {
        assert!(
            view.path.selection.is_none(),
            "a new report clears the old evidence selection"
        )
    });
}

#[test]
fn path_evidence_preserves_backend_levels_and_never_invents_missing_events() {
    let events = topology_fixture();
    let report = events[1].clone();
    let edge = |index| {
        path::detail(
            &report,
            &path::Selection {
                path: 0,
                item: path::Item::Edge(index),
            },
            &events,
        )
        .unwrap()
        .join("\n")
    };
    let measured = edge(0);
    for text in [
        "实测",
        "dns-local",
        "1794220733827",
        "core DNS response",
        "192.0.2.10",
        "证据不完整",
    ] {
        assert!(measured.contains(text), "{text}");
    }
    assert!(edge(1).contains("配置推断（非实测）"));
    assert!(edge(2).contains("未知边不代表探测成功"));
    let missing = path::detail(
        &report,
        &path::Selection {
            path: 0,
            item: path::Item::Edge(0),
        },
        &[],
    )
    .unwrap()
    .join("\n");
    assert!(missing.contains("原始事件未保留"));
    assert!(!missing.contains("192.0.2.10"));
    let attachment = path::detail(
        &report,
        &path::Selection {
            path: 0,
            item: path::Item::Attachment(0),
        },
        &events,
    )
    .unwrap()
    .join("\n");
    assert!(attachment.contains("不计作逐跳设备"));
}

#[test]
fn path_evidence_accepts_browser_encoded_numbers() {
    // Stored reports encode integers as decimal strings; evidence must still match.
    let mut events = topology_fixture();
    events[0]["project"]["round"] = "1".into();
    events[0]["project"]["index"] = "3".into();
    events[1]["rules_version"] = "1".into();
    events[1]["topology"][0]["edges"][0]["evidence"][0]["round"] = "1".into();
    events[1]["topology"][0]["edges"][0]["evidence"][0]["event_index"] = "3".into();
    assert!(latest_for_test(&events).is_some());
    let lines = path::detail(
        &events[1],
        &path::Selection {
            path: 0,
            item: path::Item::Edge(0),
        },
        &events,
    )
    .unwrap()
    .join("\n");
    assert!(lines.contains("192.0.2.10"), "{lines}");
}
fn latest_for_test(events: &[serde_json::Value]) -> Option<serde_json::Value> {
    path::latest_report(events)
}
