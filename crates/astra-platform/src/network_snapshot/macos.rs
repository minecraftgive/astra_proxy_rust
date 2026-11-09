use super::*;

pub(super) async fn network() -> (Vec<Interface>, Vec<Route>, Vec<Issue>) {
    let (interfaces, hardware, routes) = tokio::join!(
        tokio::task::spawn_blocking(unix::interfaces),
        command("/usr/sbin/networksetup", &["-listallhardwareports"]),
        command("/usr/sbin/netstat", &["-rn"])
    );
    let mut issues = vec![];
    let mut interfaces = match interfaces {
        Ok(Ok(value)) => value,
        error => {
            issues.push(issue("interfaces", format!("{error:?}")));
            vec![]
        }
    };
    match hardware {
        Ok(text) => {
            let physical = parsers::macos_hardware(&text);
            for interface in &mut interfaces {
                if physical.contains(&interface.id) {
                    interface.kind = InterfaceKind::Physical;
                    interface.source = "getifaddrs + networksetup hardware port".into();
                }
            }
        }
        Err(error) => issues.push(issue("interface_kind", error)),
    }
    let routes = match routes {
        Ok(text) => parsers::macos_routes(&text),
        Err(error) => {
            issues.push(issue("routes", error));
            vec![]
        }
    };
    (interfaces, routes, issues)
}
pub(super) async fn dns() -> (Vec<Resolver>, Vec<Issue>) {
    match command("/usr/sbin/scutil", &["--dns"]).await {
        Ok(text) => (parsers::scutil_dns(&text), vec![]),
        Err(error) => (vec![], vec![issue("dns", error)]),
    }
}
