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
fn native_batch_shortcuts_keep_selection_and_edit_without_network(cx: &mut TestAppContext) {
    let (model, view, window) = fixture(cx);
    cx.simulate_keystrokes(window.into(), "ctrl-r");
    model.read_with(cx, |model, _| {
        let v = model
            .client
            .diagnostics
            .as_ref()
            .unwrap()
            .tools
            .as_ref()
            .unwrap();
        assert!(v.section == Section::Findings);
        assert_eq!(v.plan.projects.len(), 5);
        assert!(!v.running());
    });
    cx.simulate_keystrokes(window.into(), "c e");
    model.read_with(cx, |model, _| {
        let v = model
            .client
            .diagnostics
            .as_ref()
            .unwrap()
            .tools
            .as_ref()
            .unwrap();
        assert!(v.plan.comparison);
        assert_eq!(v.plan.projects.len(), 4);
        assert!(v.section == Section::Tools);
    });
    cx.simulate_keystrokes(window.into(), "ctrl-p ctrl-l ctrl-l delete");
    model.read_with(cx, |model, _| {
        let v = model
            .client
            .diagnostics
            .as_ref()
            .unwrap()
            .tools
            .as_ref()
            .unwrap();
        assert_eq!(v.plan.projects.len(), 4);
        assert!(!v.running());
    });
    view.update(cx, |view, cx| view.back(cx));
    assert!(model.read_with(cx, |model, _| model.client.diagnostics.is_none()));
}

#[gpui::test]
fn advanced_toggle_is_captured_inside_text_inputs_without_changing_the_request(
    cx: &mut TestAppContext,
) {
    let (model, view, window) = fixture(cx);
    let target = view.read_with(cx, |view, cx| view.tools(cx, |v| v.target.clone()).unwrap());
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                let input = &view
                    .inputs
                    .iter()
                    .find(|(field, _)| *field == Field::Target)
                    .unwrap()
                    .1;
                input.update(cx, |input, cx| input.focus(window, cx));
            });
        })
        .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert!(!view.tools(cx, |v| v.show_advanced).unwrap())
    });
    cx.simulate_keystrokes(window.into(), "ctrl-d");
    view.read_with(cx, |view, cx| {
        assert!(view.tools(cx, |v| v.show_advanced).unwrap());
        assert_eq!(view.tools(cx, |v| v.target.clone()).unwrap(), target);
    });
    cx.simulate_keystrokes(window.into(), "ctrl-d");
    view.read_with(cx, |view, cx| {
        assert!(!view.tools(cx, |v| v.show_advanced).unwrap())
    });
    cx.simulate_keystrokes(window.into(), "f1 f5 f7 f9");
    model.read_with(cx, |model, _| {
        let tools = model
            .client
            .diagnostics
            .as_ref()
            .unwrap()
            .tools
            .as_ref()
            .unwrap();
        assert!(tools.section == Section::Tools);
        assert!(!tools.show_advanced);
        assert!(tools.plan.projects.is_empty());
        assert!(!tools.running());
    });
    cx.simulate_keystrokes(window.into(), "ctrl-l ctrl-d");
    view.read_with(cx, |view, cx| {
        assert!(view.tools(cx, |v| v.section == Section::Paths).unwrap());
        assert!(view.tools(cx, |v| v.show_advanced).unwrap());
        assert_eq!(view.tools(cx, |v| v.target.clone()).unwrap(), target);
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
fn conclusion_and_report_columns_stay_side_by_side_with_independent_scrolling(
    cx: &mut TestAppContext,
) {
    let (model, view, window) = fixture(cx);
    model.update(cx, |model, cx| {
        let tools = model.client.diagnostics.as_mut().unwrap().tools.as_mut().unwrap();
        tools.section = Section::Paths;
        tools.details = topology_fixture();
        for index in 0..80 {
            tools.details.push(serde_json::json!({"event":"context", "config_digest":format!("{index}-{}", "report".repeat(100))}));
        }
        cx.notify();
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(900.), px(700.)));
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    let collapsed = visual.debug_bounds("network-conclusions-column").unwrap();
    assert!(visual.debug_bounds("network-report-column").is_none());
    cx.simulate_keystrokes(window.into(), "ctrl-d");
    for (width, height) in [(900., 700.), (420., 460.)] {
        visual.simulate_resize(gpui::size(px(width), px(height)));
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let left = visual.debug_bounds("network-conclusions-column").unwrap();
        let right = visual.debug_bounds("network-report-column").unwrap();
        let topology = visual.debug_bounds("network-report-topology").unwrap();
        assert_eq!(left.origin.y, right.origin.y, "columns must never stack");
        assert!(left.right() <= right.origin.x, "report stays to the right");
        assert!(
            right.right() <= px(width),
            "long report cannot stretch the columns: left={left:?}, right={right:?}, window width={width}"
        );
        assert!(topology.origin.x >= right.origin.x && topology.right() <= right.right());
        assert!(left.size.width < collapsed.size.width);
    }
    visual.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.report_horizontal
                .set_offset(gpui::point(px(-80.), px(0.)));
            view.report_scroll
                .scroll_to_item(60, gpui::ScrollStrategy::Top);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    view.read_with(cx, |view, _| {
        assert!(view.report_horizontal.offset().x < px(0.));
        assert!(view.report_scroll.0.borrow().base_handle.offset().y < px(0.));
        assert_eq!(
            view.conclusion_horizontal.offset(),
            gpui::point(px(0.), px(0.))
        );
        assert_eq!(
            view.conclusion_scroll.0.borrow().base_handle.offset(),
            gpui::point(px(0.), px(0.))
        );
    });
    cx.simulate_keystrokes(window.into(), "ctrl-d");
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    assert!(visual.debug_bounds("network-report-column").is_none());
}

#[gpui::test]
fn topology_pointer_opens_nodes_edges_and_original_project_evidence(cx: &mut TestAppContext) {
    let (model, view, window) = fixture(cx);
    model.update(cx, |model, cx| {
        let tools = model
            .client
            .diagnostics
            .as_mut()
            .unwrap()
            .tools
            .as_mut()
            .unwrap();
        tools.section = Section::Paths;
        tools.show_advanced = true;
        tools.details = topology_fixture();
        tools.operation_id = Some("topology-report".into());
        cx.notify();
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| {
        window.resize(gpui::size(px(420.), px(460.)));
        window.draw(cx).clear(cx);
    });
    let bounds = visual
        .debug_bounds("network-topology-1")
        .expect("DNS edge has a hit target");
    visual.simulate_click(bounds.center(), Default::default());
    visual.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.topology.selection,
            Some(topology::Selection {
                path: 0,
                item: topology::Item::Edge(0)
            })
        )
    });
    // Close the inspector without changing the report, then click its source node.
    view.update(cx, |view, cx| {
        view.topology.selection = None;
        cx.notify();
    });
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    let bounds = visual
        .debug_bounds("network-topology-0")
        .expect("source node has a hit target");
    visual.simulate_click(bounds.center(), Default::default());
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.topology.selection,
            Some(topology::Selection {
                path: 0,
                item: topology::Item::Node("client".into())
            })
        )
    });
    model.update(cx, |model, cx| {
        model
            .client
            .diagnostics
            .as_mut()
            .unwrap()
            .tools
            .as_mut()
            .unwrap()
            .operation_id = Some("next-report".into());
        cx.notify();
    });
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    view.read_with(cx, |view, _| {
        assert!(
            view.topology.selection.is_none(),
            "a new report clears the old evidence selection"
        )
    });
}

#[test]
fn topology_evidence_preserves_backend_levels_and_never_invents_missing_events() {
    let events = topology_fixture();
    let report = serde_json::from_value(events[1].clone()).unwrap();
    let edge = |index| {
        topology::detail(
            &report,
            &topology::Selection {
                path: 0,
                item: topology::Item::Edge(index),
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
    let missing = topology::detail(
        &report,
        &topology::Selection {
            path: 0,
            item: topology::Item::Edge(0),
        },
        &[],
    )
    .unwrap()
    .join("\n");
    assert!(missing.contains("原始事件未保留"));
    assert!(!missing.contains("192.0.2.10"));
    let attachment = topology::detail(
        &report,
        &topology::Selection {
            path: 0,
            item: topology::Item::Attachment(0),
        },
        &events,
    )
    .unwrap()
    .join("\n");
    assert!(attachment.contains("不计作逐跳设备"));
}
