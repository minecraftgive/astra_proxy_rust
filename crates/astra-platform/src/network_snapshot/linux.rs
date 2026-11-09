#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
use super::*;

pub(super) async fn network() -> (Vec<Interface>, Vec<Route>, Vec<Issue>) {
    let (interfaces, ipv4, ipv6) = tokio::join!(
        tokio::task::spawn_blocking(|| -> io::Result<Vec<Interface>> {
            let mut interfaces = unix::interfaces()?;
            for interface in &mut interfaces {
                if interface.kind == InterfaceKind::Loopback {
                    continue;
                }
                let path = std::path::Path::new("/sys/class/net").join(&interface.name);
                if path.join("device").exists() {
                    interface.kind = InterfaceKind::Physical;
                    interface.source = "getifaddrs + sysfs device".into();
                } else if std::fs::canonicalize(&path)
                    .is_ok_and(|path| path.starts_with("/sys/devices/virtual/net"))
                {
                    interface.kind = InterfaceKind::Virtual;
                    interface.source = "getifaddrs + sysfs virtual device".into();
                }
            }
            Ok(interfaces)
        }),
        command("ip", &["-j", "-4", "route", "show", "table", "all"]),
        command("ip", &["-j", "-6", "route", "show", "table", "all"])
    );
    let mut issues = vec![];
    let interfaces = match interfaces {
        Ok(Ok(value)) => value,
        error => {
            issues.push(issue("interfaces", format!("{error:?}")));
            vec![]
        }
    };
    let mut routes = vec![];
    for (family, result) in [("ipv4_routes", ipv4), ("ipv6_routes", ipv6)] {
        match result.and_then(|text| parsers::linux_routes(&text, "ip route JSON")) {
            Ok(value) => routes.extend(value),
            Err(error) => issues.push(issue(family, error)),
        }
    }
    (interfaces, routes, issues)
}
pub(super) async fn dns() -> (Vec<Resolver>, Vec<Issue>) {
    let (dns, domains) = tokio::join!(
        command(
            "busctl",
            &[
                "--system",
                "--json=short",
                "get-property",
                "org.freedesktop.resolve1",
                "/org/freedesktop/resolve1",
                "org.freedesktop.resolve1.Manager",
                "DNS"
            ]
        ),
        command(
            "busctl",
            &[
                "--system",
                "--json=short",
                "get-property",
                "org.freedesktop.resolve1",
                "/org/freedesktop/resolve1",
                "org.freedesktop.resolve1.Manager",
                "Domains"
            ]
        )
    );
    let result = match (dns, domains) {
        (Ok(dns), Ok(domains)) => {
            let interfaces = tokio::task::spawn_blocking(unix::interfaces).await;
            match interfaces {
                Ok(Ok(interfaces)) => parsers::resolved(&dns, &domains, &interfaces),
                _ => parsers::resolved(&dns, &domains, &[]),
            }
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    match result {
        Ok(resolvers) => (resolvers, vec![]),
        Err(error) => {
            let mut issues = vec![issue("dns_policy", format!("systemd-resolved unavailable or invalid: {error}; falling back to global resolv.conf information"))];
            let file = tokio::task::spawn_blocking(|| {
                crate::storage::read_bounded(std::path::Path::new("/etc/resolv.conf"), 64 * 1024)
            })
            .await;
            let resolvers = match file {
                Ok(Ok(bytes)) => match String::from_utf8(bytes) {
                    Ok(text) => parsers::resolv_conf(&text),
                    Err(error) => {
                        issues.push(issue("dns", error));
                        vec![]
                    }
                },
                error => {
                    issues.push(issue("dns", format!("{error:?}")));
                    vec![]
                }
            };
            (resolvers, issues)
        }
    }
}
