use super::*;
#[test]
fn egress_round_trips_and_summary_counts_loss() {
    for text in ["system", "iface:以太网 2", "outbound:HK 01"] {
        let egress: Egress = text.parse().unwrap();
        assert_eq!(egress.to_string(), text);
    }
    assert!("iface:".parse::<Egress>().is_err());
    assert!("bogus".parse::<Egress>().is_err());
    let attempts = [
        Attempt {
            seq: 1,
            rtt_ms: Some(10.0),
            detail: String::new(),
            error: None,
        },
        Attempt {
            seq: 2,
            rtt_ms: None,
            detail: String::new(),
            error: Some("timeout".into()),
        },
        Attempt {
            seq: 3,
            rtt_ms: Some(30.0),
            detail: String::new(),
            error: None,
        },
    ];
    let summary = Summary::from_attempts(&attempts);
    assert_eq!((summary.sent, summary.received), (3, 2));
    assert_eq!(summary.avg_ms, Some(20.0));
    assert_eq!(summary.loss_percent(), 34);
    let json = serde_json::to_string(&Event::Attempt(attempts[0].clone())).unwrap();
    assert_eq!(
        serde_json::from_str::<Event>(&json).unwrap(),
        Event::Attempt(attempts[0].clone())
    );
}
