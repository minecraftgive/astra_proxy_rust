use super::super::shell::{OVERVIEW, PROFILES};
use super::*;
use astra_model::runtime::Connection;
use astra_model::runtime::Group;
use astra_model::runtime::Member;
use ratatui::backend::TestBackend;
fn sample() -> Ui {
    let mut ui = Ui::new(Session::default());
    ui.rows.push(Row {
        instance: Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: "/profiles/proxy.toml".into(),
        },
        pid: Some(42),
        state: "Running".into(),
        uptime: 60,
        changed: false,
    });
    ui.accept_runtime(
        "proxy".into(),
        Snapshot {
            pid: 42,
            upload: 2048,
            download: 8192,
            active: 1,
            total: 8,
            connections: vec![Connection {
                id: 7,
                source: Some("127.0.0.1:50000".into()),
                host: "example.test".into(),
                port: 443,
                network: "Tcp".into(),
                inbound: "Socks5".into(),
                outbound: "Tokyo".into(),
                upload: 2048,
                download: 8192,
                elapsed_ms: 3000,
            }],
            groups: vec![
                Group {
                    name: "PROXY".into(),
                    mode: "select".into(),
                    selected: Some("auto".into()),
                    testing: false,
                    members: vec![
                        Member {
                            name: "auto".into(),
                            delay_ms: None,
                            alive: None,
                        },
                        Member {
                            name: "Singapore".into(),
                            delay_ms: Some(110),
                            alive: Some(true),
                        },
                    ],
                },
                Group {
                    name: "auto".into(),
                    mode: "auto".into(),
                    selected: Some("Tokyo".into()),
                    testing: false,
                    members: vec![Member {
                        name: "Tokyo".into(),
                        delay_ms: Some(80),
                        alive: Some(true),
                    }],
                },
            ],
            ..Default::default()
        },
    );
    ui
}
fn key(ui: &mut Ui, code: KeyCode) -> Input {
    ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
}
fn many_connections() -> Ui {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    let connection = snapshot.connections[0].clone();
    snapshot.connections = (0..80)
        .map(|id| Connection {
            id,
            host: format!("host-{id}.test"),
            elapsed_ms: 10_000,
            ..connection.clone()
        })
        .collect();
    snapshot.active = 80;
    ui.accept_runtime("proxy".into(), snapshot);
    ui.select_connection(0);
    ui
}
/// Return the rendered connection IDs, including the rows used for mouse hit testing.
fn draw_connections(ui: &Ui, terminal: &mut Terminal<TestBackend>) -> Vec<u64> {
    ui.hits.borrow_mut().clear();
    terminal
        .draw(|frame| {
            if ui.page == OVERVIEW {
                ui.render_overview(frame, frame.area());
            } else {
                ui.render_connections(frame, frame.area());
            }
        })
        .unwrap();
    ui.hits
        .borrow()
        .iter()
        .filter_map(|(_, target)| match target {
            Target::Connection(id) => Some(*id),
            _ => None,
        })
        .collect()
}
#[test]
fn connection_lists_scroll_only_when_selection_crosses_visible_edges() {
    for page in [OVERVIEW, CONNECTIONS] {
        for width in [140, 70] {
            let mut ui = many_connections();
            ui.navigate(page);
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            let first = draw_connections(&ui, &mut terminal);
            let capacity = first.len();
            assert!(capacity > 2);
            assert_eq!(first[0], 0);
            for _ in 1..capacity {
                key(&mut ui, KeyCode::Down);
                assert_eq!(draw_connections(&ui, &mut terminal), first);
            }
            key(&mut ui, KeyCode::Down);
            let shifted = draw_connections(&ui, &mut terminal);
            assert_eq!(shifted[0], 1);
            assert_eq!(shifted.last(), Some(&(capacity as u64)));
            for _ in 1..capacity {
                key(&mut ui, KeyCode::Up);
                assert_eq!(draw_connections(&ui, &mut terminal), shifted);
            }
            key(&mut ui, KeyCode::Up);
            assert_eq!(draw_connections(&ui, &mut terminal), first);

            key(&mut ui, KeyCode::PageDown);
            assert_eq!(ui.connection_selection, capacity);
            assert_eq!(draw_connections(&ui, &mut terminal)[0], 1);
            key(&mut ui, KeyCode::End);
            assert_eq!(draw_connections(&ui, &mut terminal).last(), Some(&79));
            let last_page = draw_connections(&ui, &mut terminal);
            key(&mut ui, KeyCode::Up);
            assert_eq!(draw_connections(&ui, &mut terminal), last_page);
            key(&mut ui, KeyCode::Home);
            assert_eq!(draw_connections(&ui, &mut terminal), first);
        }
    }
}
#[test]
fn connection_viewports_follow_resize_refresh_and_shorter_samples() {
    for page in [OVERVIEW, CONNECTIONS] {
        let mut ui = many_connections();
        ui.navigate(page);
        ui.select_connection(40);
        for (width, height) in [(140, 24), (70, 16), (140, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            assert!(draw_connections(&ui, &mut terminal).contains(&40));
        }
        let mut snapshot = ui.live().unwrap().clone();
        snapshot.connections[40].download += 1;
        ui.accept_runtime("proxy".into(), snapshot.clone());
        let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
        assert_eq!(draw_connections(&ui, &mut terminal)[0], 40);
        snapshot.connections.retain(|connection| connection.id < 3);
        ui.accept_runtime("proxy".into(), snapshot);
        assert_eq!(draw_connections(&ui, &mut terminal), [0, 1, 2]);
        key(&mut ui, KeyCode::Home);
        assert_eq!(ui.selected_connection().unwrap().id, 0);
    }
}
#[test]
fn overview_navigation_tracks_refreshed_and_frozen_connection_positions() {
    let mut ui = many_connections();
    ui.navigate(OVERVIEW);
    ui.select_connection(40);
    let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
    draw_connections(&ui, &mut terminal);
    let mut snapshot = ui.live().unwrap().clone();
    snapshot.connections.retain(|c| c.id >= 30 && c.id < 60);
    ui.accept_runtime("proxy".into(), snapshot.clone());
    draw_connections(&ui, &mut terminal);
    key(&mut ui, KeyCode::Down);
    assert_eq!(ui.selected_connection().unwrap().id, 41);

    ui.toggle_live_pause();
    snapshot.connections.truncate(1);
    ui.accept_runtime("proxy".into(), snapshot);
    draw_connections(&ui, &mut terminal);
    key(&mut ui, KeyCode::End);
    assert_eq!(ui.selected_connection().unwrap().id, 59);
    key(&mut ui, KeyCode::Up);
    assert_eq!(ui.selected_connection().unwrap().id, 58);

    let mut ui = many_connections();
    ui.navigate(OVERVIEW);
    ui.select_connection(40);
    draw_connections(&ui, &mut terminal);
    ui.toggle_live_pause();
    let mut snapshot = ui.live().unwrap().clone();
    snapshot.connections.retain(|c| c.id >= 30 && c.id < 60);
    ui.accept_runtime("proxy".into(), snapshot);
    ui.toggle_live_pause();
    draw_connections(&ui, &mut terminal);
    key(&mut ui, KeyCode::Down);
    assert_eq!(ui.selected_connection().unwrap().id, 41);
}
#[test]
fn proxy_group_and_member_lists_keep_the_viewport_when_reversing_direction() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    let mut group = snapshot.groups[0].clone();
    group.members = (0..30)
        .map(|id| Member {
            name: format!("member-{id}"),
            delay_ms: None,
            alive: None,
        })
        .collect();
    group.selected = Some("member-0".into());
    snapshot.groups = (0..30)
        .map(|id| Group {
            name: format!("group-{id}"),
            ..group.clone()
        })
        .collect();
    ui.accept_runtime("proxy".into(), snapshot);
    ui.navigate(PROXIES);
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    for members in [false, true] {
        if members {
            ui.open_runtime_group();
        }
        let mut draw = |ui: &Ui| -> Vec<usize> {
            ui.hits.borrow_mut().clear();
            terminal
                .draw(|f| ui.render_live_groups(f, f.area()))
                .unwrap();
            ui.hits
                .borrow()
                .iter()
                .filter_map(|(_, target)| match target {
                    Target::Member(index) if members => Some(*index),
                    Target::Group(index) if !members => Some(*index),
                    _ => None,
                })
                .collect()
        };
        let first = draw(&ui);
        for _ in 1..first.len() {
            key(&mut ui, KeyCode::Down);
            assert_eq!(draw(&ui), first);
        }
        key(&mut ui, KeyCode::Down);
        let shifted = draw(&ui);
        assert_eq!(shifted[0], 1);
        for _ in 1..first.len() {
            key(&mut ui, KeyCode::Up);
            assert_eq!(draw(&ui), shifted);
        }
        key(&mut ui, KeyCode::Up);
        assert_eq!(draw(&ui), first);
    }
}
#[test]
fn grouped_connections_expand_keep_selection_and_only_close_individual_flows() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    let mut second = snapshot.connections[0].clone();
    second.id = 8;
    second.source = Some("second-source".into());
    snapshot.connections.push(second);
    ui.accept_runtime("proxy".into(), snapshot.clone());
    ui.navigate(CONNECTIONS);
    let rows = ui.connection_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].count, 2);
    assert_eq!(rows[0].connection.upload, 4096);
    assert_eq!(rows[0].connection.download, 16384);
    for width in [140, 70] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        draw_connections(&ui, &mut terminal);
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("(2)") && text.contains("4.0 KiB") && text.contains("16.0 KiB"));
    }
    ui.select_connection(0);
    assert!(matches!(key(&mut ui, KeyCode::Delete), Input::Idle));
    assert!(ui.message.contains("individual") || !ui.message.is_empty());
    key(&mut ui, KeyCode::Enter);
    assert_eq!(ui.connection_rows().len(), 3);
    key(&mut ui, KeyCode::Down);
    let selected_id = ui.selected_connection().unwrap().id;
    assert!(
        matches!(key(&mut ui, KeyCode::Delete), Input::Run(Action::Runtime {
        command: Command::Disconnect(Some(id)), ..
    }, _) if id == selected_id)
    );
    snapshot.connections[0].download += 100;
    ui.accept_runtime("proxy".into(), snapshot);
    assert_eq!(ui.connection_rows().len(), 3);
    assert_eq!(ui.selected_connection().unwrap().id, selected_id);
    key(&mut ui, KeyCode::Left);
    assert_eq!(ui.connection_rows().len(), 1);
    assert!(matches!(
        ui.selected_connection_row_key(),
        Some(ConnectionRowKey::Target(_))
    ));
    ui.connection_filter = "second-source".into();
    ui.select_connection(0);
    assert_eq!(ui.connection_rows().len(), 1);
    assert_eq!(ui.selected_connection().unwrap().id, 8);
}
#[test]
fn fresh_connections_pin_for_five_seconds_without_resetting_on_updates() {
    let mut ui = sample();
    ui.navigate(CONNECTIONS);
    let mut snapshot = ui.live().unwrap().clone();
    let mut new = snapshot.connections[0].clone();
    new.id = 9;
    new.host = "new.test".into();
    new.upload = 1;
    new.download = 2;
    new.elapsed_ms = 0;
    snapshot.connections.push(new);
    let before = Instant::now();
    ui.accept_runtime("proxy".into(), snapshot.clone());
    let deadline = ui.next_connection_expiry().unwrap();
    assert!(deadline >= before + Duration::from_secs(5));
    assert_eq!(ui.connection_rows()[0].connection.id, 9);
    assert!(ui.connection_rows()[0].fresh);
    assert_eq!(ui.selected_connection().unwrap().id, 7);
    assert_eq!(ui.connection_selection, 1);
    ui.accept_runtime("proxy".into(), snapshot);
    assert_eq!(ui.next_connection_expiry(), Some(deadline));
    ui.live_state
        .fresh_connections
        .insert(9, Instant::now() - Duration::from_millis(1));
    assert!(ui.expire_connection_highlights());
    assert_eq!(ui.next_connection_expiry(), None);
    assert_eq!(ui.connection_rows()[0].connection.id, 7);
    assert!(ui.connection_rows().iter().all(|r| !r.fresh));
    assert_eq!(ui.connection_selection, 0);
    assert!(!ui.expire_connection_highlights());
}
#[test]
fn a_new_flow_pins_its_collapsed_target_and_old_sample_entries_do_not_pin() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    let mut old = snapshot.connections[0].clone();
    old.id = 10;
    old.host = "busy.test".into();
    old.download = 1 << 30;
    old.elapsed_ms = 60_000;
    snapshot.connections.push(old);
    ui.accept_runtime("proxy".into(), snapshot.clone());
    assert!(ui.next_connection_expiry().is_none());
    let mut new = snapshot.connections[0].clone();
    new.id = 11;
    new.elapsed_ms = 0;
    snapshot.connections.push(new);
    ui.accept_runtime("proxy".into(), snapshot);
    let rows = ui.connection_rows();
    assert!(rows[0].is_group() && rows[0].fresh);
    assert_eq!(rows[0].count, 2);
    assert_eq!(rows[1].connection.host, "busy.test");
    ui.reset_live_identity();
    assert!(ui.next_connection_expiry().is_none());
    assert!(ui.live_state.expanded_targets.is_empty());
}
fn telemetry(snapshot: Snapshot, boot: &str, sequence: u64, time: u64) -> RuntimeView {
    RuntimeView {
        snapshot,
        telemetry: Some(RuntimeTelemetry {
            host_boot_id: boot.into(),
            active_generation: Some(1),
            sample_sequence: sequence,
            sample_elapsed_ms: time,
            coverage: Coverage::Complete,
            ..Default::default()
        }),
    }
}
#[test]
fn live_paging_tracks_rendered_viewports_and_two_line_rows() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    let connection = snapshot.connections[0].clone();
    snapshot.connections = (0..80)
        .map(|id| {
            let mut row = connection.clone();
            row.id = id;
            row.host = format!("host-{id}.test");
            row.elapsed_ms = 10_000;
            row
        })
        .collect();
    snapshot.active = 80;
    let mut group = snapshot.groups[0].clone();
    group.members = (0..60)
        .map(|id| Member {
            name: format!("member-{id}"),
            delay_ms: None,
            alive: None,
        })
        .collect();
    group.selected = Some("member-0".into());
    snapshot.groups = (0..50)
        .map(|id| {
            let mut row = group.clone();
            row.name = format!("group-{id}");
            row
        })
        .collect();
    ui.accept_runtime("proxy".into(), snapshot);
    ui.navigate(CONNECTIONS);
    for (width, step) in [(120, 17), (70, 9)] {
        ui.select_connection(0);
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
        terminal
            .draw(|f| ui.render_connections(f, f.area()))
            .unwrap();
        key(&mut ui, KeyCode::PageDown);
        assert_eq!(ui.connection_selection, step);
        key(&mut ui, KeyCode::PageUp);
        assert_eq!(ui.connection_selection, 0);
    }
    ui.navigate(PROXIES);
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|f| ui.render_live_groups(f, f.area()))
        .unwrap();
    key(&mut ui, KeyCode::PageDown);
    assert_eq!(ui.group_selection, 9);
    key(&mut ui, KeyCode::PageUp);
    key(&mut ui, KeyCode::Enter);
    terminal
        .draw(|f| ui.render_live_groups(f, f.area()))
        .unwrap();
    key(&mut ui, KeyCode::PageDown);
    assert_eq!(ui.runtime_picker.as_ref().unwrap().1, 18);
    key(&mut ui, KeyCode::PageUp);
    assert_eq!(ui.runtime_picker.as_ref().unwrap().1, 0);
    ui.navigate(OVERVIEW);
    // Narrow Controls places profile actions on a second row.
    for (width, step) in [(140, 18), (70, 5)] {
        ui.overview_cursor = 0;
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|f| ui.render_overview(f, f.area())).unwrap();
        key(&mut ui, KeyCode::PageDown);
        assert_eq!(ui.overview_cursor, step);
        key(&mut ui, KeyCode::PageUp);
        assert_eq!(ui.overview_cursor, 0);
    }
    ui.focus.region = focus::Region::Secondary;
    key(&mut ui, KeyCode::PageDown);
    assert_eq!(ui.group_selection, 3);
}
#[test]
fn live_aliases_respect_opt_in_and_delete_never_repeats_or_edits_filters() {
    let mut ui = sample();
    ui.navigate(PROXIES);
    ui.live_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(ui.group_selection, 0);
    ui.live_secondary_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(ui.group_selection, 0);
    ui.vim_keys = true;
    ui.live_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(ui.group_selection, 1);
    assert!(ui
        .live_key(event::KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE))
        .is_none());
    ui.navigate(CONNECTIONS);
    assert!(matches!(
        key(&mut ui, KeyCode::Delete),
        Input::Run(
            Action::Runtime {
                command: Command::Disconnect(Some(7)),
                ..
            },
            _
        )
    ));
    let mut repeated = event::KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
    repeated.kind = KeyEventKind::Repeat;
    assert!(matches!(ui.key(repeated), Input::Idle));
    ui.vim_keys = false;
    ui.connection_filtering = true;
    ui.live_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(ui.connection_filter, "j");
    assert!(matches!(key(&mut ui, KeyCode::Delete), Input::Idle));
}
#[test]
fn hosted_source_clock_deduplicates_cached_responses_and_resets_on_boot() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    snapshot.proxy_upload = 100;
    ui.accept_runtime_view(
        "proxy".into(),
        telemetry(snapshot.clone(), "first", 1, 1000),
    );
    let count = ui.history[0].len();
    ui.accept_runtime_view(
        "proxy".into(),
        telemetry(snapshot.clone(), "first", 1, 1000),
    );
    assert_eq!(ui.history[0].len(), count);
    snapshot.proxy_upload = 1100;
    ui.accept_runtime_view(
        "proxy".into(),
        telemetry(snapshot.clone(), "first", 2, 2000),
    );
    assert_eq!(ui.rates[0], 1000);
    assert!(ui.runtime_rates_available());
    let count = ui.history[0].len();
    ui.accept_runtime_view(
        "proxy".into(),
        telemetry(snapshot.clone(), "first", 2, 2000),
    );
    assert_eq!(ui.history[0].len(), count);
    ui.accept_runtime_view("proxy".into(), telemetry(snapshot, "restarted", 1, 10));
    assert!(ui.history[0].is_empty());
    assert!(!ui.runtime_rates_available());
}
#[test]
fn feed_identity_change_clears_frozen_targets_before_legacy_snapshot_arrives() {
    for enhanced in [false, true] {
        let mut ui = sample();
        let mut next = ui.live().unwrap().clone();
        if enhanced {
            ui.accept_runtime_view("proxy".into(), telemetry(next.clone(), "old-boot", 1, 1000));
        }
        ui.toggle_live_pause();
        assert_eq!(ui.selected_connection().unwrap().id, 7);
        // Feeds calls this exactly when its (profile, host PID) key changes.
        ui.reset_live_identity();
        assert!(ui.live_state.selected.is_none());
        assert!(ui.live_state.telemetry.is_none());
        assert!(!ui.connections_paused());
        assert!(ui.history.iter().all(|lane| lane.is_empty()));
        ui.rows[0].pid = Some(84);
        next.pid = 84;
        next.connections[0].host = "replacement.test".into();
        ui.accept_runtime("proxy".into(), next);
        assert_eq!(ui.connection_snapshot().unwrap().pid, 84);
        assert_eq!(ui.selected_connection().unwrap().host, "replacement.test");
        assert!(!ui.runtime_rates_available());
    }
}
#[test]
fn same_host_reconnect_and_worker_generation_change_preserve_frozen_view() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    ui.accept_runtime_view("proxy".into(), telemetry(snapshot.clone(), "boot", 1, 1000));
    ui.toggle_live_pause();
    let frozen_at = ui.live_state.frozen_at.clone();
    ui.runtime_disconnected("reconnecting".into());
    snapshot.connections[0].host = "latest.test".into();
    let mut changed_worker = telemetry(snapshot, "boot", 2, 2000);
    changed_worker.telemetry.as_mut().unwrap().active_generation = Some(2);
    ui.accept_runtime_view("proxy".into(), changed_worker);
    assert!(ui.connections_paused());
    assert_eq!(ui.live_state.frozen_at, frozen_at);
    assert_eq!(ui.selected_connection().unwrap().host, "example.test");
    assert_eq!(ui.live().unwrap().connections[0].host, "latest.test");
    assert_eq!(
        ui.live_state.telemetry.as_ref().unwrap().active_generation,
        Some(2)
    );
}
#[test]
fn selection_survives_reorder_and_missing_sample_never_closes_another_flow() {
    let mut ui = sample();
    ui.page = CONNECTIONS;
    let mut snapshot = ui.live().unwrap().clone();
    let mut newer = snapshot.connections[0].clone();
    newer.id = 99;
    newer.host = "newer.test".into();
    newer.download = u64::MAX / 2;
    snapshot.connections.insert(0, newer);
    ui.accept_runtime("proxy".into(), snapshot.clone());
    assert_eq!(ui.selected_connection().unwrap().id, 7);
    assert_eq!(ui.connection_selection, 1);
    snapshot.connections.retain(|c| c.id != 7);
    snapshot.truncated = true;
    ui.accept_runtime("proxy".into(), snapshot);
    assert!(ui.selected_connection().is_none());
    assert!(matches!(
        ui.live_key(event::KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        Some(Input::Idle)
    ));
    assert!(ui.message.contains("not present"));
}
#[test]
fn overview_selection_is_revealed_on_connections_despite_an_old_filter() {
    let mut ui = sample();
    ui.connection_filter = "unrelated.test".into();
    ui.navigate(OVERVIEW);
    assert_eq!(ui.visible_connections().len(), 1);
    ui.select_connection_id(7);
    ui.navigate(CONNECTIONS);
    assert_eq!(ui.selected_connection().unwrap().id, 7);
    assert!(ui.connection_filter.is_empty());
    key(&mut ui, KeyCode::Enter);
    assert!(ui.live_detail_open());
    key(&mut ui, KeyCode::Esc);
    assert!(!ui.live_detail_open());
    assert_eq!(ui.page, CONNECTIONS);
}
#[test]
fn explicit_pause_freezes_rows_while_health_and_latest_data_continue() {
    let mut ui = sample();
    ui.toggle_live_pause();
    let frozen_at = ui.live_state.frozen_at.clone();
    assert!(frozen_at.is_some());
    let mut snapshot = ui.live().unwrap().clone();
    snapshot.connections[0].host = "updated.example".into();
    ui.accept_runtime("proxy".into(), snapshot);
    assert_eq!(ui.live_state.frozen_at, frozen_at);
    assert_eq!(ui.visible_connections()[0].host, "example.test");
    assert_eq!(ui.live().unwrap().connections[0].host, "updated.example");
    ui.runtime_disconnected("lost transport".into());
    crate::i18n::with_language(crate::i18n::Language::English, || {
        assert!(ui.runtime_health().contains("Reconnecting"));
    });
    assert!(!ui.runtime_rates_available());
    ui.toggle_live_pause();
    assert_eq!(ui.visible_connections()[0].host, "updated.example");
}
#[test]
fn partial_samples_do_not_fabricate_rates_and_late_samples_do_not_rewind() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    ui.accept_runtime_view("proxy".into(), telemetry(snapshot.clone(), "boot", 1, 1000));
    snapshot.proxy_download = 2000;
    ui.accept_runtime_view("proxy".into(), telemetry(snapshot.clone(), "boot", 2, 2000));
    let count = ui.history[1].len();
    let mut partial = telemetry(snapshot.clone(), "boot", 3, 3000);
    partial.telemetry.as_mut().unwrap().coverage = Coverage::Partial;
    ui.accept_runtime_view("proxy".into(), partial);
    assert!(!ui.runtime_rates_available());
    assert_eq!(ui.history[1].len(), count);
    snapshot.proxy_download = 1;
    ui.accept_runtime_view("proxy".into(), telemetry(snapshot, "boot", 2, 2000));
    assert_eq!(ui.live().unwrap().proxy_download, 2000);
    assert_eq!(ui.live_state.telemetry.as_ref().unwrap().sample_sequence, 3);
}
#[test]
fn narrow_views_keep_pause_and_transport_health_visible() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = sample();
        ui.toggle_live_pause();
        ui.runtime_disconnected("transport lost".into());
        for page in [OVERVIEW, CONNECTIONS] {
            ui.navigate(page);
            let text = screen(&ui, 70, 24);
            assert!(
                text.contains("PAUSED") && text.contains("RECONNECTING"),
                "{text}"
            );
            assert!(
                text.contains("example.test"),
                "last received rows remain visible"
            );
        }
    });
}
#[test]
fn cursor_tabs_and_states_are_highlighted_on_every_list_page() {
    let selected = crate::theme::selected();
    let mut ui = sample();
    ui.rows.push(Row {
        instance: Instance {
            name: "rules".into(),
            kind: Kind::Rule,
            config: "/r.toml".into(),
        },
        pid: None,
        state: "Stopped".into(),
        uptime: 0,
        changed: false,
    });
    let draw = |ui: &Ui| {
        let mut t = ratatui::Terminal::new(TestBackend::new(110, 30)).unwrap();
        t.draw(|f| ui.render(f)).unwrap();
        t.backend().buffer().clone()
    };
    // Styles of the cells spelling `text` on the first row that shows it.
    let styles = |buffer: &ratatui::buffer::Buffer, text: &str| {
        (0..buffer.area.height)
            .find_map(|y| {
                let line: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                let at = line.find(text)?;
                let column = line[..at].chars().count() as u16;
                Some(
                    (column..column + text.chars().count() as u16)
                        .map(|x| buffer[(x, y)].style())
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default()
    };
    crate::i18n::with_language(crate::i18n::Language::English, || {
        ui.navigate(PROFILES);
        let buffer = draw(&ui);
        assert!(styles(&buffer, "Profiles 1")
            .iter()
            .all(|s| s.bg == selected.bg));
        assert!(styles(&buffer, "Rule cores 1")
            .iter()
            .all(|s| s.bg != selected.bg));
        let state = styles(&buffer, "State: Running");
        assert!(
            state.iter().any(|s| s.fg == Some(Color::Green)),
            "state is colored"
        );
        key(&mut ui, KeyCode::Tab);
        key(&mut ui, KeyCode::Right);
        key(&mut ui, KeyCode::Enter);
        let buffer = draw(&ui);
        assert!(styles(&buffer, "Rule cores 1")
            .iter()
            .all(|s| s.bg == selected.bg));
        // With focus on members, the group cursor stays visible without competing.
        ui.navigate(PROXIES);
        key(&mut ui, KeyCode::Enter);
        let buffer = draw(&ui);
        assert!(styles(&buffer, "● auto")
            .iter()
            .all(|s| s.bg == selected.bg));
        // The selected group's second line, in the groups pane on the left.
        let group = styles(&buffer, "→ auto");
        assert!(!group.is_empty());
        assert!(group
            .iter()
            .all(|s| s.add_modifier.contains(Modifier::UNDERLINED) && s.bg != selected.bg));
        // Connections: exit, network and volume are colored on unselected rows.
        let mut snapshot = ui.live().unwrap().clone();
        snapshot.connections.push(Connection {
            id: 8,
            source: None,
            host: "quiet.test".into(),
            port: 53,
            network: "Udp".into(),
            inbound: "Tun".into(),
            outbound: "DIRECT".into(),
            upload: 100,
            download: 3 * 1024 * 1024,
            elapsed_ms: 1000,
        });
        ui.accept_runtime("proxy".into(), snapshot);
        ui.navigate(CONNECTIONS);
        let buffer = draw(&ui);
        let fg = |text: &str| styles(&buffer, text).first().and_then(|s| s.fg);
        assert_eq!(fg("DIRECT"), Some(Color::Green));
        assert_eq!(fg("Udp"), Some(Color::Magenta));
        assert_eq!(fg(":53"), Some(MUTED));
        assert_eq!(fg("100 B"), Some(MUTED), "idle counters fade");
        assert!(styles(&buffer, "3.0 MiB")[0]
            .add_modifier
            .contains(Modifier::BOLD));
        // The page's key hints color keys apart from what they do.
        assert_eq!(fg("x close"), Some(Color::Cyan));
        assert_eq!(styles(&buffer, "x close")[2].fg, Some(MUTED));
        // An active filter is marked in the footer and in unselected matching rows.
        ui.connection_filter = "test".into();
        let buffer = draw(&ui);
        let matched = crate::theme::matched().bg;
        assert_eq!(styles(&buffer, "“test”")[0].bg, matched);
        let row = styles(&buffer, "quiet.test");
        assert!(row[..6].iter().all(|s| s.bg != matched) && row[6].bg == matched);
    });
}
#[test]
fn status_bar_colors_state_and_drops_detail_instead_of_cutting_words() {
    let mut ui = sample();
    let mut snapshot = ui.live().unwrap().clone();
    snapshot.tun = true;
    ui.accept_runtime("proxy".into(), snapshot);
    // Overview folds its status into the Astra header; the standalone status bar
    // still serves every other page, so test it there.
    ui.navigate(PROXIES);
    ui.rates = [0, 48_000];
    let bar = |ui: &Ui, width: u16| {
        let mut t = ratatui::Terminal::new(TestBackend::new(width, 12)).unwrap();
        t.draw(|f| ui.render(f)).unwrap();
        let buffer = t.backend().buffer().clone();
        let cells: Vec<_> = (0..width * 2)
            .map(|index| buffer[(index % width, index / width)].clone())
            .collect();
        let text: String = cells.iter().map(|c| c.symbol()).collect();
        (text, cells)
    };
    let color_of = |cells: &[ratatui::buffer::Cell], text: &str, word: &str| {
        let at = text.find(word).unwrap();
        cells[text[..at].chars().count()].fg
    };
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let (text, cells) = bar(&ui, 220);
        assert_eq!(color_of(&cells, &text, "TUN"), Color::Green);
        assert_eq!(color_of(&cells, &text, "LAN"), MUTED);
        assert_eq!(color_of(&cells, &text, "Rule"), Color::Cyan);
        assert_eq!(
            color_of(&cells, &text, "↑        0 B/s"),
            MUTED,
            "idle rate is muted"
        );
        assert_eq!(color_of(&cells, &text, "↓"), Color::Cyan);
        let up: Vec<_> = (0..220).filter(|&x| cells[x].symbol() == "↑").collect();
        let down: Vec<_> = (0..220)
            .filter(|&x| cells[x + 220].symbol() == "↓")
            .collect();
        assert_eq!(up.len(), 2);
        assert_eq!(up, down, "standalone rates align across both rows");
        assert!(
            text.contains("Proxy") && text.contains("Direct") && text.trim_end().ends_with("B/s"),
            "rates are right-aligned: {text}"
        );
        // Narrower bars lose whole parts in order, never half a word.
        let (text, _) = bar(&ui, 70);
        assert!(
            !text.contains("B/s") && text.contains("TUN") && !text.contains("LAN"),
            "{text}"
        );
        for width in [48, 36, 24] {
            let (text, _) = bar(&ui, width);
            let words: Vec<_> = text.split_whitespace().collect();
            for word in words {
                assert!(
                    [
                        "Astra",
                        "│",
                        "●",
                        "proxy",
                        "Connected",
                        "Profile",
                        "connected",
                        "Rule",
                        "TUN"
                    ]
                    .contains(&word)
                        || word
                            .chars()
                            .all(|c| c.is_ascii_digit() || "hms".contains(c)),
                    "partial word {word:?} at {width}: {text}"
                );
            }
        }
    });
}
#[test]
fn find_jumps_to_matching_groups_and_members() {
    let mut ui = sample();
    ui.navigate(PROXIES);
    let find = |ui: &mut Ui, text: &str| {
        key(ui, KeyCode::Char('/'));
        assert!(matches!(ui.prompt, Some(Prompt::Find(PROXIES))));
        ui.key(event::KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ));
        for c in text.chars() {
            key(ui, KeyCode::Char(c));
        }
        key(ui, KeyCode::Enter);
    };
    find(&mut ui, "sing");
    assert_eq!(ui.group_selection, 0);
    assert_eq!(ui.runtime_picker, Some(("PROXY".into(), 1)));
    // Inside the member list, / searches again and finds members of other groups.
    find(&mut ui, "tokyo");
    assert_eq!(ui.group_selection, 1);
    assert_eq!(ui.runtime_picker, Some(("auto".into(), 0)));
    // "auto" is a member of PROXY and a group; after the last match the search wraps.
    find(&mut ui, "auto");
    assert_eq!(ui.runtime_picker, Some(("PROXY".into(), 0)));
    find(&mut ui, "auto");
    assert_eq!(ui.runtime_picker, None);
    assert_eq!(ui.group_selection, 1);
    find(&mut ui, "nothing");
    assert!(ui.message.contains("nothing"));
}
#[test]
fn exit_chain_follows_group_selections_to_the_final_node() {
    let ui = sample();
    let (chain, delay, alive) = exit_chain(ui.live().unwrap());
    assert_eq!(chain, ["PROXY", "auto", "Tokyo"]);
    assert_eq!((delay, alive), (Some(80), Some(true)));
}
#[test]
fn selecting_nodes_and_closing_filtered_connections_use_runtime_commands_with_pid() {
    let mut ui = sample();
    key(&mut ui, KeyCode::Char('2'));
    key(&mut ui, KeyCode::Enter);
    assert!(ui.runtime_picker.is_some());
    // Editing shortcuts cannot act on the profile behind the member list.
    assert!(matches!(key(&mut ui, KeyCode::Char('e')), Input::Idle));
    assert!(ui.editor.is_none());
    key(&mut ui, KeyCode::Down);
    assert!(
        matches!(key(&mut ui, KeyCode::Enter), Input::Run(Action::Runtime { command: Command::Select {group, member}, expected_pid: Some(42), .. }, _) if group == "PROXY" && member == "Singapore")
    );
    // Automatic groups stay read-only.
    key(&mut ui, KeyCode::Esc);
    key(&mut ui, KeyCode::Down);
    key(&mut ui, KeyCode::Enter);
    assert!(matches!(key(&mut ui, KeyCode::Enter), Input::Idle));
    key(&mut ui, KeyCode::Esc);
    key(&mut ui, KeyCode::Char('3'));
    key(&mut ui, KeyCode::Char('/'));
    for c in "example".chars() {
        key(&mut ui, KeyCode::Char(c));
    }
    key(&mut ui, KeyCode::Enter);
    assert_eq!(ui.visible_connections().len(), 1);
    // d never closes connections; x does.
    assert!(matches!(key(&mut ui, KeyCode::Char('d')), Input::Idle));
    assert!(matches!(
        key(&mut ui, KeyCode::Char('x')),
        Input::Run(
            Action::Runtime {
                command: Command::Disconnect(Some(7)),
                expected_pid: Some(42),
                ..
            },
            _
        )
    ));
    // Closing everything asks first.
    assert!(matches!(key(&mut ui, KeyCode::Char('X')), Input::Idle));
    assert!(matches!(ui.prompt, Some(Prompt::CloseAll { .. })));
    assert!(matches!(key(&mut ui, KeyCode::Enter), Input::Idle));
    assert!(ui.prompt.is_none(), "confirmation defaults to cancel");
    key(&mut ui, KeyCode::Char('X'));
    key(&mut ui, KeyCode::Tab);
    assert!(matches!(
        key(&mut ui, KeyCode::Enter),
        Input::Run(
            Action::Runtime {
                command: Command::Disconnect(None),
                expected_pid: Some(42),
                ..
            },
            _
        )
    ));
    assert!(matches!(
        key(&mut ui, KeyCode::Char('m')),
        Input::Run(
            Action::Runtime {
                command: Command::SetMode(Mode::Global),
                ..
            },
            _
        )
    ));
}
#[test]
fn runtime_navigation_never_changes_hidden_instances_or_disconnects_on_ctrl_d() {
    let mut ui = sample();
    ui.client.rows.push(ui.client.rows[0].clone());
    ui.rows[1].instance.name = "other".into();
    for page in [PROXIES, CONNECTIONS] {
        ui.navigate(page);
        for code in [
            KeyCode::End,
            KeyCode::Home,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Char('G'),
        ] {
            assert!(matches!(key(&mut ui, code), Input::Idle));
            assert_eq!(ui.selections[0], 0);
        }
        assert!(matches!(
            ui.key(event::KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL
            )),
            Input::Idle
        ));
        assert_eq!(ui.selections[0], 0);
    }
    ui.runtime_snapshot = None;
    assert!(matches!(key(&mut ui, KeyCode::Char('x')), Input::Idle));
    assert!(ui.prompt.is_none());
    key(&mut ui, KeyCode::Down);
    assert_eq!(ui.selections[0], 0);
    // Rule cores selected in Profiles never replace the active profile's runtime.
    ui.navigate(PROFILES);
    ui.switch_core(Kind::Rule);
    assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy");
}

#[test]
fn page_navigation_clears_search_capture() {
    let mut ui = sample();
    ui.connection_filtering = true;
    ui.navigate(CONNECTIONS);
    assert!(!ui.connection_filtering);
    key(&mut ui, KeyCode::Char('2'));
    assert_eq!(ui.page, PROXIES);
    key(&mut ui, KeyCode::Char('1'));
    assert_eq!(ui.page, OVERVIEW);
}

fn screen(ui: &Ui, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui.render(f)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn proxies_show_members_beside_groups_and_connections_show_details() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = sample();
        ui.navigate(PROXIES);
        ui.open_runtime_group();
        let proxies = screen(&ui, 120, 24);
        for value in [
            "Proxy groups",
            "PROXY",
            "Singapore",
            "110 ms",
            "Enter selects",
        ] {
            assert!(proxies.contains(value), "{value}\n{proxies}");
        }
        ui.navigate(CONNECTIONS);
        let connections = screen(&ui, 130, 24);
        for value in ["example.test:443", "Tokyo", "Details", "127.0.0.1:50000"] {
            assert!(connections.contains(value), "{value}\n{connections}");
        }
        if std::env::var_os("ASTRA_TUI_PREVIEW").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../target/tui-pages-preview.txt"
                ),
                format!("{proxies}\n\n{connections}"),
            )
            .unwrap();
        }
    });
}
#[test]
fn tun_toggle_asks_for_elevation_only_when_supported() {
    let mut ui = sample();
    ui.page = OVERVIEW;
    let details = crate::protocol::InstanceDetails {
        elevation_supported: true,
        ..Default::default()
    };
    ui.details.insert("proxy".into(), details);

    assert!(matches!(ui.runtime_action(Command::Tun(true)), Input::Idle));
    assert!(matches!(
        &ui.prompt,
        Some(Prompt::ElevateTun { name, pid }) if name == "proxy" && *pid == Some(42)
    ));
    let enter = event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(ui.key(enter), Input::Idle));
    assert!(ui.prompt.is_none(), "elevation also defaults to cancel");
    ui.runtime_action(Command::Tun(true));
    key(&mut ui, KeyCode::Tab);
    assert!(matches!(
        ui.key(enter),
        Input::Run(Action::PrepareElevation { name, expected_pid: Some(42) }, _) if name == "proxy"
    ));

    // Turning TUN off never needs sudo; the running core handles it.
    assert!(matches!(
        ui.runtime_action(Command::Tun(false)),
        Input::Run(Action::Runtime { .. }, _)
    ));

    let mut ui = sample();
    ui.page = OVERVIEW;
    ui.details
        .insert("proxy".into(), crate::protocol::InstanceDetails::default());
    assert!(matches!(
        ui.runtime_action(Command::Tun(true)),
        Input::Run(Action::Runtime { .. }, _)
    ));
    assert!(ui.prompt.is_none());
}

#[test]
fn live_cards_groups_and_connections_resize_and_old_process_data_is_hidden() {
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let mut ui = sample();
            for page in [OVERVIEW, PROXIES, CONNECTIONS] {
                ui.page = page;
                for (width, height) in [(140, 40), (90, 24), (40, 12), (1, 1)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|f| ui.render(f)).unwrap();
                }
            }
            if language == crate::i18n::Language::English
                && std::env::var_os("ASTRA_TUI_PREVIEW").is_some()
            {
                ui.page = OVERVIEW;
                ui.rates = [1024, 8192];
                ui.history = [
                    std::collections::VecDeque::from(vec![0, 100, 300, 800, 1024]),
                    std::collections::VecDeque::from(vec![0, 600, 2800, 4096, 8192]),
                ];
                let mut preview = Terminal::new(TestBackend::new(140, 40)).unwrap();
                preview.draw(|f| ui.render(f)).unwrap();
                let cells: Vec<_> = preview.backend().buffer().content.iter().map(|cell| serde_json::json!({"text":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg)})).collect();
                std::fs::write(
                    concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../target/tui-live-preview.json"
                    ),
                    serde_json::to_vec(&cells).unwrap(),
                )
                .unwrap();
            }
            ui.page = PROXIES;
            ui.open_runtime_group();
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal.draw(|f| ui.render(f)).unwrap();
            assert!(ui
                .hits
                .borrow()
                .iter()
                .any(|(_, target)| matches!(target, Target::Member(1))));
            ui.rows[0].pid = Some(99);
            assert!(ui.live().is_none());
        });
    }
}
