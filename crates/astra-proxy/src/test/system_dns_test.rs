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
    let changes = plan(Os::MacOs, "utun9", ADDRESS, None, |argv| {
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
    assert!(plan(Os::MacOs, "utun9", ADDRESS, None, |_| Ok(
        "header only\n".into()
    ))
    .is_err());
}

#[test]
fn linux_plan_uses_per_link_resolved_settings_and_requires_resolvectl() {
    let changes = plan(Os::Linux, "astra0", ADDRESS, None, |argv| {
        assert_eq!(argv[0], RESOLVECTL);
        Ok(match argv[1].as_str() {
            "dns" => "Link 5 (astra0): 9.9.9.9 1.1.1.1".into(),
            "default-route" => "Link 5 (astra0): no".into(),
            "domain" => "Link 5 (astra0):".into(),
            _ => unreachable!(),
        })
    })
    .unwrap();
    let applies: Vec<_> = changes.iter().map(|c| c.apply.clone()).collect();
    assert_eq!(
        applies,
        [
            s(&[RESOLVECTL, "domain", "astra0", "~."]),
            s(&[RESOLVECTL, "dns", "astra0", "198.18.0.2"]),
        ]
    );
    assert_eq!(&changes[0].restore[3..], [""]);
    assert_eq!(&changes[1].restore[3..], ["9.9.9.9", "1.1.1.1"]);
    assert!(changes
        .iter()
        .all(|change| change.apply[1] != "default-route"));
    assert!(changes
        .iter()
        .all(|c| allowed(&c.apply) && allowed(&c.restore)));
    let error = plan(Os::Linux, "astra0", ADDRESS, None, |_| {
        anyhow::bail!("not found")
    })
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("systemd-resolved"),
        "{error:#}"
    );
}

#[test]
fn windows_plan_restores_manual_or_dhcp_by_guid() {
    let id = "12345678-1234-4abc-8abc-123456789012";
    for previous in [vec![], vec!["10.0.0.2", "10.0.0.1"]] {
        let changes = plan(Os::Windows, "Astra TUN", ADDRESS, None, |cmd| {
            assert_eq!(cmd, s(&[search::WIN_SERVERS_READ, "Astra TUN"]));
            Ok(serde_json::to_string(&(id, &previous, ""))?)
        })
        .unwrap();
        assert_eq!(
            changes[0].apply,
            s(&[search::WIN_SERVERS_SET, id, "198.18.0.2"])
        );
        assert_eq!(changes[0].restore[2..], previous);
        assert!(allowed(&changes[0].apply) && allowed(&changes[0].restore));
    }
}

#[test]
fn recovery_only_accepts_expected_resolver_commands() {
    for argv in [
        s(&["/bin/sh", "-c", "true"]),
        s(&[NETWORKSETUP, "-setwebproxy", "Wi-Fi", "127.0.0.1", "8080"]),
        s(&[NETWORKSETUP, "-setdnsservers", "Wi-Fi", "not-an-ip"]),
        s(&[NETWORKSETUP, "-setdnsservers", "Wi-Fi"]),
        s(&[RESOLVECTL, "dns", "astra0", "; rm -rf /"]),
        s(&[RESOLVECTL, "domain", "astra0", "--json=off"]),
        s(&[
            "netsh",
            "interface",
            "ipv4",
            "set",
            "address",
            "name=x",
            "source=dhcp",
        ]),
        s(&[
            "netsh",
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
        "172.27.0.53",
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
        server_routes(&servers, &config, &["172.27.0.0/24".parse().unwrap()]),
        ["192.168.1.53/32", "8.8.8.8/32"]
    );
    config.ipv6_address = Some("fdfe:dcba:9876::1".parse().unwrap());
    config.gateway6 = Some("2001:db8::1".parse().unwrap());
    let v6: Vec<IpAddr> = [
        "2001:db8::53",
        "2001:db8::1",
        "fe80::1",
        "fdfe:dcba:9876::2",
        "fdab::53",
    ]
    .iter()
    .map(|ip| ip.parse().unwrap())
    .collect();
    assert_eq!(
        server_routes(&v6, &config, &["fdab::/48".parse().unwrap()]),
        ["2001:db8::53/128"]
    );
}

fn parse(text: &str) -> crate::config::ProxyConfig {
    toml::from_str(text).unwrap()
}
fn tun_key(config: &crate::config::ProxyConfig, name: &str) -> Option<(bool, bool)> {
    match Key::wanted(config, Some(name.into()))? {
        Key::Tun {
            takeover, route, ..
        } => Some((takeover, route)),
        Key::Local(_) => panic!("TUN config produced a local key"),
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
        assert_eq!(Key::wanted(&opted, None), Some(Key::Local(None)));
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
    let changes = plan(Os::MacOs, "", Ipv4Addr::LOCALHOST, None, |argv| {
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

#[test]
fn macos_plan_preserves_existing_synthetic_tun_dns_servers() {
    let only_synthetic = plan(Os::MacOs, "utun9", ADDRESS, None, |argv| {
        Ok(match argv[1].as_str() {
            "-listallnetworkservices" => "header\nWi-Fi\n".into(),
            _ => "198.18.0.2\n".into(),
        })
    })
    .unwrap();
    assert_eq!(
        only_synthetic[0].restore,
        s(&[NETWORKSETUP, "-setdnsservers", "Wi-Fi", "198.18.0.2"])
    );
    let mixed = plan(Os::MacOs, "utun9", ADDRESS, None, |argv| {
        Ok(match argv[1].as_str() {
            "-listallnetworkservices" => "header\nWi-Fi\n".into(),
            _ => "198.18.0.2\n1.1.1.1\n".into(),
        })
    })
    .unwrap();
    assert_eq!(
        mixed[0].restore,
        s(&[
            NETWORKSETUP,
            "-setdnsservers",
            "Wi-Fi",
            "198.18.0.2",
            "1.1.1.1"
        ])
    );
}

#[test]
fn magic_dns_mac_search_preserves_manual_and_dhcp_domains_and_skips_duplicates() {
    let changes = plan(
        Os::MacOs,
        "",
        Ipv4Addr::LOCALHOST,
        Some("astra.home.arpa"),
        |cmd| {
            Ok(match cmd[1].as_str() {
                "-listallnetworkservices" => "header\nWi-Fi\nUSB LAN\nVPN".into(),
                "-getdnsservers" => format!("There aren't any DNS Servers set on {}.", cmd[2]),
                "-getsearchdomains" if cmd[2] == "Wi-Fi" => "corp.test\nlab.test".into(),
                "-getsearchdomains" if cmd[2] == "VPN" => "ASTRA.HOME.ARPA.".into(),
                "-getsearchdomains" => "There aren't any Search Domains set on USB LAN.".into(),
                "USB LAN" if cmd[0] == search::MAC_DYNAMIC => "dhcp.test".into(),
                _ => unreachable!("{cmd:?}"),
            })
        },
    )
    .unwrap();
    let search_changes: Vec<_> = changes
        .iter()
        .filter(|change| change.apply[1] == "-setsearchdomains")
        .collect();
    assert_eq!(search_changes.len(), 2);
    assert_eq!(
        search_changes[0].apply[3..],
        ["astra.home.arpa", "corp.test", "lab.test"]
    );
    assert_eq!(search_changes[0].restore[3..], ["corp.test", "lab.test"]);
    assert_eq!(
        search_changes[1].apply[3..],
        ["astra.home.arpa", "dhcp.test"]
    );
    assert_eq!(search_changes[1].restore[3..], ["Empty"]);
    assert!(changes
        .iter()
        .all(|c| allowed(&c.apply) && allowed(&c.restore)));
}

#[test]
fn magic_dns_linux_retains_search_and_route_domains() {
    let previous = "Link 5 (astra0): corp.test ~split.test ~ASTRA.home.arpa ~.";
    let changes = plan(
        Os::Linux,
        "astra0",
        ADDRESS,
        Some("astra.home.arpa"),
        |cmd| {
            Ok(match cmd[1].as_str() {
                "dns" => "Link 5 (astra0):",
                "default-route" => "Link 5 (astra0): no",
                _ => previous,
            }
            .into())
        },
    )
    .unwrap();
    assert_eq!(
        changes[0].apply[3..],
        ["astra.home.arpa", "corp.test", "~split.test", "~."]
    );
    assert_eq!(
        changes[0].restore[3..],
        ["corp.test", "~split.test", "~ASTRA.home.arpa", "~."]
    );
    assert!(changes
        .iter()
        .all(|c| allowed(&c.apply) && allowed(&c.restore)));
}

#[test]
fn magic_dns_windows_restores_manual_or_automatic_suffix_by_guid() {
    let id = "12345678-1234-4abc-8abc-123456789012";
    for old in ["", "corp.test", "ASTRA.HOME.ARPA"] {
        let changes = plan(
            Os::Windows,
            "Astra TUN",
            ADDRESS,
            Some("astra.home.arpa"),
            |cmd| {
                assert_eq!(cmd, s(&[search::WIN_SERVERS_READ, "Astra TUN"]));
                Ok(serde_json::to_string(&(
                    format!("{{{id}}}"),
                    Vec::<String>::new(),
                    old,
                ))?)
            },
        )
        .unwrap();
        if old.eq_ignore_ascii_case("astra.home.arpa") {
            assert_eq!(changes.len(), 1);
        } else {
            assert_eq!(
                changes[1].apply,
                s(&[search::WIN_SET, id, "astra.home.arpa"])
            );
            assert_eq!(changes[1].restore, s(&[search::WIN_SET, id, old]));
            assert!(changes
                .iter()
                .all(|c| allowed(&c.apply) && allowed(&c.restore)));
        }
    }
    assert!(!allowed(&s(&[search::WIN_SET, "alias", "corp.test"])));
    assert!(!allowed(&s(&[search::WIN_SET, id, "--bad"])));
}

#[test]
fn magic_dns_takeover_key_changes_only_when_the_system_suffix_changes() {
    let text = "[tun]\n[dns.magic_dns]\nenable=true\ndomain='devices.test'\n[dns.magic_dns.hosts]\nnas=['192.168.1.20']";
    let key = |text: &str| Key::wanted(&parse(text), Some("astra0".into()));
    assert_eq!(
        key(text),
        key(&text.replace("192.168.1.20", "192.168.1.21"))
    );
    assert_eq!(
        key(text),
        key(&text.replace("devices.test", "DEVICES.Test."))
    );
    assert_ne!(key(text), key(&text.replace("devices.test", "lab.test")));
    assert_ne!(key(text), key(&text.replace("enable=true", "enable=false")));
    let routing_only = text.replace("[tun]", "[tun]\n[dns]\nsystem_takeover=false");
    assert_eq!(
        key(&routing_only),
        key(&routing_only.replace("devices.test", "lab.test"))
    );
}

#[test]
fn resolved_dns_endpoints_preserve_ports_scopes_and_server_names() {
    for value in [
        "1.1.1.1:853#dns.example",
        "[2001:db8::1]:9953%eth0#dns.example",
        "fe80::53%eth0",
    ] {
        assert!(allowed(&s(&[RESOLVECTL, "dns", "astra0", value])));
    }
    for value in ["--help", "1.2.3.4#", "10.0.0.1:xyz", "10.0.0.1%bad name"] {
        assert!(!allowed(&s(&[RESOLVECTL, "dns", "astra0", value])));
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "read-only snapshot of enabled network services on a real macOS machine"]
fn macos_native_plans_match_live_service_identity_and_values() {
    let changes = plan(Os::MacOs, "", ADDRESS, Some("test.astra.invalid"), run).unwrap();
    assert!(!changes.is_empty());
    for change in changes {
        let (id, _) = search::mac_service(&change.apply[2]).unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert_eq!(change.current().unwrap().unwrap(), change.restore);
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes only to the explicitly named disposable macOS network service"]
fn macos_disposable_service_restores_dns_and_domains() -> Result<()> {
    let service =
        std::env::var("ASTRA_TEST_MACOS_SERVICE").context("set the disposable service name")?;
    ensure!(
        !service.is_empty() && !service.contains(['\r', '\n']) && !service.starts_with('*'),
        "invalid service name"
    );
    let (id, _) = search::mac_service(&service)?;
    let changes = plan(Os::MacOs, "", ADDRESS, Some("audit.astra.invalid"), |cmd| {
        if cmd[1] == "-listallnetworkservices" {
            return Ok(format!("test service\n{service}"));
        }
        run(cmd)
    })?;
    ensure!(
        changes
            .iter()
            .any(|c| c.restore[1] == "-setdnsservers" && c.restore.len() >= 5)
            && changes
                .iter()
                .any(|c| c.restore[1] == "-setsearchdomains" && c.restore[3] != "Empty"),
        "fixture requires at least two manual DNS servers and a custom search domain"
    );
    let guard = Guard::apply_settings(changes.clone())?;
    for change in &changes {
        assert_eq!(change.current()?.unwrap(), change.apply);
    }
    assert!(
        Guard::apply_settings(changes.clone()).is_err(),
        "overlapping takeover accepted"
    );
    drop(guard);
    assert_eq!(search::mac_service(&service)?.0, id);
    for change in &changes {
        assert_eq!(change.current()?.unwrap(), change.restore);
    }
    Ok(())
}
