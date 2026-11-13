use super::*;
use astra_model::diagnostics::Probe;

fn exit(outbound: &str) -> Exit {
    Exit {
        profile: "proxy".into(),
        pid: 42,
        outbound: outbound.into(),
    }
}

fn plan(ip: bool, latency: Option<u64>) -> Plan {
    Plan {
        ip,
        latency: latency.map(Duration::from_secs),
        keep_alive: true,
    }
}

fn probe(connect_ms: Option<u64>, ip: Option<&str>) -> Outcome {
    Ok(Probe {
        outbound: "Tokyo".into(),
        direct: false,
        connect_ms,
        elapsed_ms: 0,
        ip: ip.map(|ip| ip.parse().unwrap()),
        error: None,
    })
}

/// A prober with a round in flight whose answer the test supplies.
async fn answered(exit: Exit, asked_ip: bool, outcome: Outcome) -> Prober {
    let (sender, receiver) = mpsc::channel(1);
    sender.send(outcome).await.unwrap();
    Prober {
        exit: Some(exit),
        round: Some(Round {
            task: tokio::spawn(async {}),
            receiver,
            started: Instant::now(),
            ip: asked_ip,
            initial: false,
        }),
        ..Default::default()
    }
}

fn tick(prober: &mut Prober, target: Option<Exit>, plan: Plan) -> bool {
    // An unreachable socket: any round that does start fails on its own.
    prober.tick(
        Path::new("/nonexistent/astra.sock"),
        target,
        plan,
        &Default::default(),
    )
}

#[test]
fn keep_alive_outlasts_the_gap_between_rounds() {
    assert_eq!(
        plan(false, Some(30)).keep_alive(),
        Some(Duration::from_secs(60))
    );
    let off = Plan {
        keep_alive: false,
        ..plan(false, Some(30))
    };
    assert_eq!(off.keep_alive(), None, "one probe process per round");
}

#[tokio::test]
async fn header_waits_for_configurable_probe_budget_but_still_bounds_a_missing_reply() {
    let (_sender, receiver) = mpsc::channel(1);
    let started = Instant::now();
    let mut prober = Prober {
        exit: Some(exit("Tokyo")),
        round: Some(Round {
            task: tokio::spawn(std::future::pending()),
            receiver,
            started,
            ip: false,
            initial: false,
        }),
        ..Default::default()
    };
    assert!(!prober.collect(plan(false, Some(30)), started + Duration::from_secs(72)));
    assert!(prober.running());
    prober.collect(plan(false, Some(30)), started + Duration::from_secs(78));
    assert!(!prober.running());
    assert_eq!(prober.next, Some(started + Duration::from_secs(88)));
}

#[tokio::test]
async fn first_round_records_latency_and_ip_then_later_rounds_skip_the_ip() {
    let mut prober = answered(exit("Tokyo"), true, probe(Some(42), Some("203.0.113.7"))).await;
    assert!(tick(&mut prober, Some(exit("Tokyo")), plan(true, Some(30))));
    assert_eq!(
        prober.latency("proxy", "Tokyo"),
        Some(Latency::Measured(42))
    );
    assert_eq!(
        prober.latency("proxy", "Osaka"),
        None,
        "only the probed node"
    );
    let (node, ip) = prober.found_ip.take().unwrap();
    assert_eq!(
        (node.outbound.as_str(), ip.to_string().as_str()),
        ("Tokyo", "203.0.113.7")
    );
    assert!(!prober.running());
    let wait = prober.next.unwrap() - Instant::now();
    assert!(
        wait > Duration::from_secs(25),
        "next round after the interval"
    );

    // The next round on the same node times the handshake only.
    prober.next = Some(Instant::now());
    tick(&mut prober, Some(exit("Tokyo")), plan(true, Some(30)));
    assert!(prober.running());
    assert!(!prober.round.as_ref().unwrap().ip, "IP only after a switch");
}

#[tokio::test]
async fn a_failed_handshake_shows_failure_and_a_missing_ip_is_retried_soon() {
    let mut prober = answered(exit("Tokyo"), true, probe(None, None)).await;
    tick(&mut prober, Some(exit("Tokyo")), plan(true, Some(300)));
    assert_eq!(prober.latency("proxy", "Tokyo"), Some(Latency::Failed));
    assert!(prober.found_ip.is_none());
    assert!(prober.next.unwrap() - Instant::now() <= RETRY);
}

#[tokio::test]
async fn ip_only_probes_once_and_latency_off_hides_the_old_value() {
    let mut prober = answered(exit("Tokyo"), true, probe(Some(80), Some("203.0.113.7"))).await;
    prober.latency = Some(Latency::Measured(80));
    prober.periodic = true;
    assert!(tick(&mut prober, Some(exit("Tokyo")), plan(true, None)));
    assert_eq!(prober.latency("proxy", "Tokyo"), None);
    prober.next = Some(Instant::now());
    tick(&mut prober, Some(exit("Tokyo")), plan(true, None));
    assert!(!prober.running(), "the IP is known and latency is off");
}

#[tokio::test]
async fn a_new_exit_or_connection_starts_over() {
    let mut prober = answered(exit("Tokyo"), false, probe(Some(80), None)).await;
    prober.ip_found = true;
    prober.latency = Some(Latency::Measured(80));
    // Default plan: latency only; the switch still looks up the IP once.
    tick(&mut prober, Some(exit("Osaka")), plan(false, Some(30)));
    assert_eq!(prober.latency("proxy", "Osaka"), None);
    assert!(!prober.ip_found);
    assert!(prober.running(), "a new node is measured right away");
    let round = prober.round.as_ref().unwrap();
    assert!(round.ip && round.initial, "every switch looks up the IP");

    let reconnected = Exit {
        pid: 43,
        ..exit("Osaka")
    };
    tick(&mut prober, Some(reconnected.clone()), plan(false, None));
    assert_eq!(prober.exit, Some(reconnected));
    assert!(
        prober.running(),
        "a reconnect probes once even with both off"
    );

    tick(&mut prober, None, plan(true, Some(30)));
    assert!(prober.exit.is_none() && !prober.running());
}

#[tokio::test]
async fn an_older_manager_is_not_asked_again_for_the_same_node() {
    let mut prober = answered(
        exit("Tokyo"),
        false,
        Err("unknown variant `exit_probe`, expected one of ...".into()),
    )
    .await;
    tick(&mut prober, Some(exit("Tokyo")), plan(false, Some(30)));
    prober.next = Some(Instant::now());
    tick(&mut prober, Some(exit("Tokyo")), plan(false, Some(30)));
    assert!(!prober.running());
    assert_eq!(prober.latency("proxy", "Tokyo"), None, "no false timeout");
}

/// An initial round in flight for `exit`, answered by `outcome`.
async fn initial(exit: Exit, outcome: Outcome) -> Prober {
    let mut prober = answered(exit, true, outcome).await;
    prober.initial = true;
    prober.round.as_mut().unwrap().initial = true;
    prober
}

#[tokio::test]
async fn connect_or_switch_probes_ip_and_latency_once_with_both_settings_off() {
    let mut prober = Prober::default();
    tick(&mut prober, Some(exit("Tokyo")), plan(false, None));
    let round = prober.round.as_ref().expect("a connect starts a round");
    assert!(round.ip && round.initial);

    let mut prober = initial(exit("Tokyo"), probe(Some(55), Some("203.0.113.7"))).await;
    assert!(tick(&mut prober, Some(exit("Tokyo")), plan(false, None)));
    assert_eq!(
        prober.latency("proxy", "Tokyo"),
        Some(Latency::Measured(55))
    );
    assert_eq!(prober.found_ip.take().unwrap().1.to_string(), "203.0.113.7");
    // The result stays (latency off hides interval rounds only) and no more rounds run.
    prober.next = Some(Instant::now());
    tick(&mut prober, Some(exit("Tokyo")), plan(false, None));
    assert!(!prober.running());
    assert_eq!(
        prober.latency("proxy", "Tokyo"),
        Some(Latency::Measured(55))
    );

    // A failed IP lookup is not retried while the IP setting is off.
    let mut prober = initial(exit("Tokyo"), probe(Some(55), None)).await;
    tick(&mut prober, Some(exit("Tokyo")), plan(false, None));
    prober.next = Some(Instant::now());
    tick(&mut prober, Some(exit("Tokyo")), plan(false, None));
    assert!(!prober.running());
}

#[tokio::test]
async fn an_unanswered_switch_probe_is_retried_until_it_answers() {
    let mut prober = initial(exit("Tokyo"), Err("manager busy".into())).await;
    tick(&mut prober, Some(exit("Tokyo")), plan(false, None));
    assert!(prober.initial, "still owed");
    assert!(prober.next.unwrap() - Instant::now() <= RETRY);
    prober.next = Some(Instant::now());
    tick(&mut prober, Some(exit("Tokyo")), plan(false, None));
    assert!(prober
        .round
        .as_ref()
        .is_some_and(|round| round.initial && round.ip));
}

#[tokio::test]
async fn switching_again_drops_the_old_nodes_round_and_result() {
    let (sender, receiver) = mpsc::channel(1);
    let mut prober = Prober {
        exit: Some(exit("Tokyo")),
        initial: true,
        round: Some(Round {
            task: tokio::spawn(std::future::pending()),
            receiver,
            started: Instant::now(),
            ip: true,
            initial: true,
        }),
        ..Default::default()
    };
    // Switch before Tokyo answers: its round is aborted and Osaka starts fresh.
    tick(&mut prober, Some(exit("Osaka")), plan(false, Some(30)));
    assert!(sender.is_closed(), "the old round's receiver is dropped");
    assert!(sender
        .try_send(probe(Some(9), Some("198.51.100.1")))
        .is_err());
    assert_eq!(prober.latency("proxy", "Osaka"), None);
    assert_eq!(prober.latency("proxy", "Tokyo"), None);
    assert!(prober.found_ip.is_none());
    assert!(prober.round.as_ref().is_some_and(|round| round.initial));
}

#[tokio::test]
async fn latency_off_hides_interval_results_but_keeps_the_switch_result() {
    let mut prober = answered(exit("Tokyo"), false, probe(Some(70), None)).await;
    tick(&mut prober, Some(exit("Tokyo")), plan(false, Some(30)));
    assert_eq!(
        prober.latency("proxy", "Tokyo"),
        Some(Latency::Measured(70))
    );
    assert!(tick(&mut prober, Some(exit("Tokyo")), plan(false, None)));
    assert_eq!(prober.latency("proxy", "Tokyo"), None);
}
