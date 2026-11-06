use super::*;

#[test]
fn query_inputs_cover_domains_ports_v6_udp_and_optional_process() {
    let query = request("api.ip.sb:443 tcp msedge.exe").unwrap();
    assert_eq!(query.target.host, "api.ip.sb");
    assert_eq!(query.target.port, 443);
    assert_eq!(query.process.unwrap().name, "msedge.exe");
    assert!(matches!(
        request("[2001:db8::1]:53 udp").unwrap().network,
        Network::Udp
    ));
    assert_eq!(request("2001:db8::1").unwrap().target.port, 443);
    for text in [
        "",
        "host:0",
        "host:65536",
        "https://api.ip.sb",
        "host quic",
        "host tcp process extra",
    ] {
        assert!(request(text).is_err(), "{text}");
    }
}
#[test]
fn query_process_paths_preserve_spaces_and_windows_separators() {
    for identity in [
        r"C:\Program Files\Browser\browser.exe",
        r#""C:\Program Files\Browser\browser.exe""#,
        "/opt/My Browser/browser",
    ] {
        let query = request(&format!("example.test   tcp   {identity}")).unwrap();
        let process = query.process.unwrap();
        assert!(process.path.contains(' '));
        assert!(process.name == "browser.exe" || process.name == "browser");
    }
    assert_eq!(
        request("host udp 'My Browser'")
            .unwrap()
            .process
            .unwrap()
            .name,
        "My Browser"
    );
    for identity in ["relative/path", "'unterminated", "''"] {
        assert!(request(&format!("host tcp {identity}")).is_err());
    }
}
#[test]
fn single_label_names_are_flagged_but_domains_and_addresses_are_not() {
    for host in ["bilibili", "cloudflare", "escapefromtarkov."] {
        assert!(single_label(host).is_some(), "{host}");
    }
    for host in ["bilibili.com", "localhost", "8.8.8.8", "2001:db8::1"] {
        assert!(single_label(host).is_none(), "{host}");
    }
}
