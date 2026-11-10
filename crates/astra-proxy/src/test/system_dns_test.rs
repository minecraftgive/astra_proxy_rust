use super::*;

fn tun() -> Tun {
    Tun::default()
}
fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}
const ADDRESS: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);

#[test]
fn macos_plan_snapshots_each_enabled_service_and_restores_empty_or_previous() {
    let mut queried = Vec::new();
    let changes = plan(Os::MacOs, "utun9", ADDRESS, |argv| {
        queried.push(argv.to_vec());
        Ok(match argv[1].as_str() {
            "-listallnetworkservices" => "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\n*Bluetooth PAN\nUSB 10/100 LAN\n".into(),
            "-getdnsservers" if argv[2] == "Wi-Fi" => "192.168.1.1\n2001:db8::53\n".into(),
            "-getdnsservers" => format!("There aren't any DNS Servers set on {}.\n", argv[2]),
            _ => unreachable!(),
        })
    })
    .unwrap();
    assert_eq!(
        changes,
        vec![
            Change {
                apply: s(&[NETWORKSETUP, "-setdnsservers", "Wi-Fi", "198.18.0.2"]),
                restore: s(&[
                    NETWORKSETUP,
                    "-setdnsservers",
                    "Wi-Fi",
                    "192.168.1.1",
                    "2001:db8::53"
                ]),
            },
            Change {
                apply: s(&[
                    NETWORKSETUP,
                    "-setdnsservers",
                    "USB 10/100 LAN",
                    "198.18.0.2"
                ]),
                restore: s(&[NETWORKSETUP, "-setdnsservers", "USB 10/100 LAN", "Empty"]),
            },
        ]
    );
    assert!(!queried
        .iter()
        .any(|argv| argv.contains(&"*Bluetooth PAN".to_owned())));
    assert!(changes
        .iter()
        .all(|c| allowed(&c.apply) && allowed(&c.restore)));
    assert!(plan(Os::MacOs, "utun9", ADDRESS, |_| Ok("header only\n".into())).is_err());
}

#[test]
fn linux_plan_uses_per_link_resolved_settings_and_requires_resolvectl() {
    let changes = plan(Os::Linux, "astra0", ADDRESS, |argv| {
        assert_eq!(argv, s(&[RESOLVECTL, "status", "astra0"]));
        Ok(String::new())
    })
    .unwrap();
    let applies: Vec<_> = changes.iter().map(|c| c.apply.clone()).collect();
    assert_eq!(
        applies,
        [
            s(&[RESOLVECTL, "dns", "astra0", "198.18.0.2"]),
            s(&[RESOLVECTL, "domain", "astra0", "~."]),
            s(&[RESOLVECTL, "default-route", "astra0", "yes"]),
        ]
    );
    assert!(changes
        .iter()
        .all(|c| c.restore == s(&[RESOLVECTL, "revert", "astra0"])));
    assert!(changes
        .iter()
        .all(|c| allowed(&c.apply) && allowed(&c.restore)));
    let error = plan(Os::Linux, "astra0", ADDRESS, |_| anyhow::bail!("not found")).unwrap_err();
    assert!(
        format!("{error:#}").contains("systemd-resolved"),
        "{error:#}"
    );
}

#[test]
fn windows_plan_sets_static_dns_on_the_tun_adapter_and_restores_dhcp() {
    let changes = plan(Os::Windows, "Astra TUN", ADDRESS, |_| unreachable!()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].apply[5], "name=Astra TUN");
    assert!(changes[0].apply.contains(&"address=198.18.0.2".to_owned()));
    assert!(changes[0].restore.contains(&"source=dhcp".to_owned()));
    assert!(allowed(&changes[0].apply) && allowed(&changes[0].restore));
}

#[test]
fn recovery_only_accepts_expected_resolver_commands() {
    for argv in [
        s(&["/bin/sh", "-c", "true"]),
        s(&[NETWORKSETUP, "-setwebproxy", "Wi-Fi", "127.0.0.1", "8080"]),
        s(&[NETWORKSETUP, "-setdnsservers", "Wi-Fi", "not-an-ip"]),
        s(&[NETWORKSETUP, "-setdnsservers", "Wi-Fi"]),
        s(&[RESOLVECTL, "dns", "astra0", "; rm -rf /"]),
        s(&[RESOLVECTL, "domain", "astra0", "corp.example"]),
        s(&[
            NETSH,
            "interface",
            "ipv4",
            "set",
            "address",
            "name=x",
            "source=dhcp",
        ]),
        s(&[
            NETSH,
            "interface",
            "ipv4",
            "set",
            "dnsservers",
            "name=x",
            "address=evil",
        ]),
        s(&["networksetup", "-setdnsservers", "Wi-Fi", "Empty"]),
        vec![],
    ] {
        assert!(!allowed(&argv), "{argv:?}");
    }
    let snapshot = Snapshot {
        changes: vec![Change {
            apply: s(&[RESOLVECTL, "dns", "astra0", "198.18.0.2"]),
            restore: s(&["/bin/sh", "-c", "true"]),
        }],
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    let parsed: Snapshot = serde_json::from_str(&json).unwrap();
    assert!(recover(&parsed).is_err(), "edited snapshot must not run");
}

#[test]
fn dns_address_is_the_spare_host_of_the_tun_subnet() {
    assert_eq!(dns_address(&tun()), Some(ADDRESS));
    let mut wide = tun();
    wide.prefix = 24;
    wide.address = "10.7.0.1".parse().unwrap();
    assert_eq!(dns_address(&wide), Some("10.7.0.2".parse().unwrap()));
    wide.address = "10.7.0.2".parse().unwrap();
    assert_eq!(dns_address(&wide), Some("10.7.0.1".parse().unwrap()));
    let mut single = tun();
    single.prefix = 32;
    assert_eq!(dns_address(&single), None);
}

#[test]
fn server_routes_skip_gateways_local_and_tun_ranges() {
    let mut config = tun();
    config.gateway = Some("192.168.1.1".parse().unwrap());
    config.bypass = vec!["10.0.0.0/8".into()];
    let servers: Vec<IpAddr> = [
        "192.168.1.1",  // gateway: routing it into TUN would break egress
        "192.168.1.53", // LAN resolver
        "8.8.8.8",      // public resolver
        "8.8.8.8",      // duplicate
        "127.0.0.53",   // local stub
        "169.254.1.1",  // link local
        "198.18.0.2",   // TUN subnet
        "198.19.4.4",   // synthetic range
        "10.1.1.1",     // tun.bypass
        "0.0.0.0",      // unspecified
        "2001:db8::53", // IPv6 without a TUN IPv6 address
        "fe80::1",      // link local
    ]
    .iter()
    .map(|ip| ip.parse().unwrap())
    .collect();
    assert_eq!(
        server_routes(&servers, &config),
        ["192.168.1.53/32", "8.8.8.8/32"]
    );
    config.ipv6_address = Some("fdfe:dcba:9876::1".parse().unwrap());
    config.gateway6 = Some("2001:db8::1".parse().unwrap());
    let v6: Vec<IpAddr> = [
        "2001:db8::53",
        "2001:db8::1",
        "fe80::1",
        "fdfe:dcba:9876::2",
    ]
    .iter()
    .map(|ip| ip.parse().unwrap())
    .collect();
    assert_eq!(server_routes(&v6, &config), ["2001:db8::53/128"]);
}

fn parse(text: &str) -> crate::config::ProxyConfig {
    toml::from_str(text).unwrap()
}
fn tun_key(config: &crate::config::ProxyConfig, name: &str) -> Option<(bool, bool)> {
    match Key::wanted(config, Some(name.into()))? {
        Key::Tun {
            takeover, route, ..
        } => Some((takeover, route)),
        Key::Local => panic!("TUN config produced a local key"),
    }
}

#[test]
fn tun_takeover_requires_a_live_tun_and_an_enabled_hijacking_dns_service() {
    let on = parse("[tun]\n[dns]\n");
    assert_eq!(tun_key(&on, "utun9"), Some((true, true)));
    assert!(Key::wanted(&on, None).is_none(), "no device yet");
    assert!(Key::wanted(&parse("[dns]\n"), Some("utun9".into())).is_none());
    assert!(Key::wanted(&parse("[tun]\n"), Some("utun9".into())).is_none());
    for off in [
        "[tun]\n[dns]\nenable=false\n",
        "[tun]\n[dns]\nhijack=false\n",
        "[tun]\n[dns]\nsystem_takeover=false\nroute_system_dns=false\n",
    ] {
        assert!(tun_key(&parse(off), "utun9").is_none(), "{off}");
    }
    assert_eq!(
        tun_key(&parse("[tun]\n[dns]\nsystem_takeover=false\n"), "utun9"),
        Some((false, true))
    );
    assert_ne!(
        Key::wanted(&on, Some("utun9".into())),
        Key::wanted(&on, Some("utun10".into()))
    );
    // TUN wins over the non-TUN option.
    let both = parse("[tun]\n[dns]\nsystem_takeover_without_tun=true\n");
    assert!(matches!(
        Key::wanted(&both, Some("utun9".into())),
        Some(Key::Tun { .. })
    ));
    assert_eq!(local_listener(&both), None);
}

#[test]
fn non_tun_takeover_is_opt_in_and_macos_only() {
    // Default: a DNS-only or system-proxy config never touches system DNS.
    for text in [
        "[dns]\n",
        "[dns]\nsystem_takeover=true\n",
        "[dns]\nenable=false\nsystem_takeover_without_tun=true\n",
    ] {
        let config = parse(text);
        assert!(Key::wanted(&config, None).is_none(), "{text}");
        assert_eq!(local_listener(&config), None, "{text}");
    }
    let opted = parse("[dns]\nsystem_takeover_without_tun=true\n");
    if cfg!(target_os = "macos") {
        assert_eq!(Key::wanted(&opted, None), Some(Key::Local));
        assert_eq!(local_listener(&opted), Some("0.0.0.0:53".parse().unwrap()));
    } else {
        assert!(Key::wanted(&opted, None).is_none());
        assert_eq!(local_listener(&opted), None);
    }
}

#[test]
fn implicit_port_53_listener_is_loopback_only_unless_listed() {
    let path = std::env::temp_dir().join("astra-system-dns-local-test.toml");
    let implicit =
        crate::App::from_text(&path, "[dns]\nsystem_takeover_without_tun=true\n".into()).unwrap();
    assert!(local_only(&implicit, LOCAL_LISTEN));
    assert!(!local_only(&implicit, "127.0.0.1:1053".parse().unwrap()));
    let listed = crate::App::from_text(
        &path,
        "[dns]\nlisten=['0.0.0.0:53']\nsystem_takeover_without_tun=true\n".into(),
    )
    .unwrap();
    assert!(
        !local_only(&listed, LOCAL_LISTEN),
        "an explicit listen keeps the LAN ACL"
    );
}

#[test]
fn local_takeover_plan_points_every_service_at_loopback() {
    let changes = plan(Os::MacOs, "", Ipv4Addr::LOCALHOST, |argv| {
        Ok(match argv[1].as_str() {
            "-listallnetworkservices" => "header\nWi-Fi\n".into(),
            _ => "223.6.6.6\n1.1.1.1\n".into(),
        })
    })
    .unwrap();
    assert_eq!(
        changes,
        vec![Change {
            apply: s(&[NETWORKSETUP, "-setdnsservers", "Wi-Fi", "127.0.0.1"]),
            restore: s(&[
                NETWORKSETUP,
                "-setdnsservers",
                "Wi-Fi",
                "223.6.6.6",
                "1.1.1.1"
            ]),
        }]
    );
    assert!(changes
        .iter()
        .all(|c| allowed(&c.apply) && allowed(&c.restore)));
}
