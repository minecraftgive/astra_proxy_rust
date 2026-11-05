use super::*;
fn row(cidr: &str, index: u32, gateway: &str) -> MIB_IPFORWARD_ROW2 {
    let net: ipnet::IpNet = cidr.parse().unwrap();
    MIB_IPFORWARD_ROW2 {
        InterfaceIndex: index,
        DestinationPrefix: IP_ADDRESS_PREFIX {
            Prefix: sockaddr(net.network(), index),
            PrefixLength: net.prefix_len(),
        },
        NextHop: sockaddr(gateway.parse().unwrap(), index),
        Metric: 5,
        Protocol: MIB_IPPROTO_NETMGMT,
        ..Default::default()
    }
}
#[test]
fn interface_handoff_allows_only_exact_process_owned_routes() {
    let prefix = "172.67.166.176/32".parse().unwrap();
    let wired = row("172.67.166.176/32", 26, "192.0.2.1");
    let wifi = row("172.67.166.176/32", 7, "192.0.2.254");
    let owned = vec![RouteIdentity::from_row(&wired).unwrap()];
    assert!(validate_prefix_rows(prefix, &[wired], &[]).is_err());
    assert!(validate_prefix_rows(prefix, &[wired], &owned).is_ok());
    assert!(validate_prefix_rows(prefix, &[wired, wifi], &owned).is_err());
    let both = vec![
        RouteIdentity::from_row(&wired).unwrap(),
        RouteIdentity::from_row(&wifi).unwrap(),
    ];
    assert!(validate_prefix_rows(prefix, &[wired, wifi], &both).is_ok());
    let mut foreign = wired;
    foreign.Metric += 1;
    assert!(validate_prefix_rows(prefix, &[foreign], &owned).is_err());
    foreign = wired;
    foreign.NextHop = sockaddr("192.0.2.99".parse().unwrap(), 26);
    assert!(validate_prefix_rows(prefix, &[foreign], &owned).is_err());
    let unrelated = row("203.0.113.8/32", 26, "192.0.2.1");
    assert!(validate_prefix_rows(prefix, &[unrelated], &[]).is_ok());
    let v6 = row("2001:db8::8/128", 26, "fe80::1");
    assert!(validate_prefix_rows(
        "2001:db8::8/128".parse().unwrap(),
        &[v6],
        &[RouteIdentity::from_row(&v6).unwrap()]
    )
    .is_ok());
}

#[test]
fn discovered_default_is_live_hardware_and_can_be_excluded() {
    // A disconnected CI host may legitimately have no physical default.
    let Ok(alias) = physical_default_interface(None) else {
        return;
    };
    let index = interface_index(&alias).unwrap();
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    check(unsafe { GetIfEntry2(&mut row) }).unwrap();
    assert_ne!(row.InterfaceAndOperStatusFlags._bitfield & 1, 0);
    assert_eq!(
        row.OperStatus,
        windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp
    );
    assert_eq!(interface_alias(&index.to_string()).unwrap(), alias);
    assert_ne!(
        physical_default_interface(Some(&alias)).ok(),
        Some(alias.clone())
    );
    eprintln!("physical egress: {alias} (index {index})");
}
