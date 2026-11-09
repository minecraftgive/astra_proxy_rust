use super::*;
use astra_model::{
    nettool::{PinnedEgress, Request},
    network_path::PathContext,
};
use std::sync::Mutex;
fn request(probe: Probe) -> DetailedRequest {
    Request {
        config: String::new(),
        egress: Egress::System,
        probe,
        count: 1,
        interval_ms: 0,
        timeout_ms: 1000,
    }
    .into()
}
#[tokio::test]
async fn tun_echo_does_not_resolve_or_send_and_never_claims_rtt() {
    let mut request = request(Probe::Icmp {
        host: "unresolvable.invalid".into(),
    });
    request.family = Some(AddressFamily::Ipv6);
    request.path_context = Some(PathContext {
        sampled_at_ms: "1".into(),
        core_tun: Some(true),
        physical: None,
    });
    let events = Mutex::new(vec![]);
    let summary = run_icmp(&request, |event| {
        events.lock().unwrap().push(event);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(summary.sent, 0);
    assert_eq!(summary.received, 0);
    let events = events.into_inner().unwrap();
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::Started { .. } | Event::Resolution { .. } | Event::Attempt(_)
    )));
    assert!(events.iter().any(|event|matches!(event,Event::Echo{observation,..} if observation.outcome==PathOutcome::TunLocal && observation.tun_local && !observation.proves_reachability && observation.rtt_ms.is_none() && observation.from.is_none())));
}
#[tokio::test]
async fn system_trace_is_unavailable_without_dns_or_packets() {
    let request = request(Probe::Trace {
        host: "unresolvable.invalid".into(),
        max_hops: 30,
        probes_per_hop: 3,
    });
    let events = Mutex::new(vec![]);
    assert!(run_trace(&request, |event| {
        events.lock().unwrap().push(event);
        Ok(())
    })
    .await
    .is_err());
    let events = events.into_inner().unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0],Event::TraceSummary(value) if !value.capability.supported && value.probes_sent==0 && !value.reached && value.physical.is_none())
    );
}
#[test]
fn proxy_or_unbound_direct_is_not_a_physical_choice() {
    let mut request = request(Probe::Icmp {
        host: "192.0.2.1".into(),
    });
    request.egress = Egress::Outbound("group".into());
    assert!(physical_interface(&request).is_err());
    request.pinned = Some(PinnedEgress {
        version: 1,
        requested_tag: "group".into(),
        leaf_tag: "DIRECT".into(),
        effective_interface: None,
        interface_source: None,
        selection_chain: vec![],
        node: None,
    });
    assert!(physical_interface(&request).is_err());
    request.pinned.as_mut().unwrap().effective_interface = Some("selected".into());
    assert_eq!(physical_interface(&request).unwrap(), "selected");
    request.pinned.as_mut().unwrap().leaf_tag = "proxy".into();
    assert!(physical_interface(&request).is_err());
    request.pinned.as_mut().unwrap().leaf_tag = "DIRECT".into();
    request.pinned.as_mut().unwrap().version = 2;
    assert!(physical_interface(&request).is_err());
}
#[tokio::test]
async fn trace_bounds_are_enforced_before_platform_or_resolution() {
    for (hops, probes, timeout) in [
        (31, 3, 1000),
        (30, 4, 1000),
        (0, 3, 1000),
        (30, 0, 1000),
        (30, 3, 1001),
        (30, 3, 0),
    ] {
        let mut request = request(Probe::Trace {
            host: "unresolvable.invalid".into(),
            max_hops: hops,
            probes_per_hop: probes,
        });
        request.timeout_ms = timeout;
        assert!(
            run_trace(&request, |_| panic!("No events before valid bounds"))
                .await
                .is_err()
        );
    }
}
