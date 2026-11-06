use super::*;
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}
fn view() -> View {
    let mut view = View::new(
        Path::new("unused-nettool.sock"),
        "proxy".into(),
        &["DIRECT".into(), "HK 01".into(), "Auto".into()],
    );
    view.egresses = vec![
        Egress::System,
        Egress::Interface("Wi-Fi".into()),
        Egress::Outbound("DIRECT".into()),
        Egress::Outbound("HK 01".into()),
    ];
    view
}

#[tokio::test]
async fn form_builds_requests_for_each_probe_and_egress() {
    let mut view = view();
    // Type-ahead jumps to the matching outbound.
    for c in "hk".chars() {
        view.key(key(KeyCode::Char(c)));
    }
    assert_eq!(view.egresses[view.egress], Egress::Outbound("HK 01".into()));
    view.key(key(KeyCode::Right));
    assert_eq!(view.egress, 0, "←→ wraps around");
    view.key(key(KeyCode::Left));
    view.key(key(KeyCode::Left));
    assert_eq!(
        view.egresses[view.egress],
        Egress::Outbound("DIRECT".into())
    );
    let request = view.request().unwrap();
    assert_eq!(
        request.probe,
        Probe::Tcp {
            host: "www.gstatic.com".into(),
            port: 443
        }
    );
    view.key(key(KeyCode::Down));
    view.key(key(KeyCode::Char('d')));
    assert_eq!(view.kind, Kind::Dns);
    assert_eq!(view.target, "www.gstatic.com");
    assert_eq!(view.fields().len(), 6);
    view.key(key(KeyCode::Down));
    view.key(key(KeyCode::Down));
    view.key(key(KeyCode::Right));
    let request = view.request().unwrap();
    assert!(
        matches!(&request.probe, Probe::Dns { record, server, .. } if record == "AAAA" && server == "1.1.1.1")
    );
    assert_eq!(request.count, 1);
    view.field = Field::Kind;
    view.key(key(KeyCode::Char('i')));
    assert_eq!(view.fields().len(), 4);
    view.field = Field::Count;
    for c in "120".chars() {
        view.key(key(KeyCode::Char(c)));
    }
    assert!(view.count <= MAX_COUNT);
    view.field = Field::Target;
    view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(view.request().is_err());
    view.key(key(KeyCode::Enter));
    assert!(view.error.is_some() && !view.running());
    assert!(
        !view.key(key(KeyCode::Esc)),
        "Esc while idle leaves the page"
    );
}
#[tokio::test]
async fn late_outbounds_keep_the_selected_egress() {
    let mut view = view();
    view.egress = 1;
    view.set_outbounds(&["DIRECT".into(), "JP".into()]);
    assert_eq!(
        view.egresses[view.egress],
        Egress::Interface("Wi-Fi".into())
    );
    assert_eq!(view.egresses.last(), Some(&Egress::Outbound("JP".into())));
    view.egress = view.egresses.len() - 1;
    view.set_outbounds(&[]);
    assert_eq!(view.egresses[view.egress], Egress::System);
}
#[test]
fn host_part_strips_ports_but_not_ipv6() {
    assert_eq!(host_part("example.com:443"), "example.com");
    assert_eq!(host_part("[2001:db8::1]:443"), "2001:db8::1");
    assert_eq!(host_part("2001:db8::1"), "2001:db8::1");
    assert_eq!(host_part("1.1.1.1"), "1.1.1.1");
}
