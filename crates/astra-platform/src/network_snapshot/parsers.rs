#![cfg_attr(not(test), allow(dead_code))]
use super::*;
use serde_json::Value;

pub(super) fn scutil_dns(text: &str) -> Vec<Resolver> {
    let mut result = Vec::<Resolver>::new();
    let mut scope = "global";
    for line in text.lines().map(str::trim) {
        if line.starts_with("DNS configuration") {
            scope = if line.contains("scoped") {
                "scoped"
            } else if line.contains("service") {
                "service"
            } else {
                "global"
            };
        } else if let Some(id) = line.strip_prefix("resolver #") {
            result.push(Resolver {
                id: format!("scutil:{scope}:{id}"),
                source: "scutil --dns".into(),
                interface_id: None,
                scope: scope.into(),
                domains: vec![],
                servers: vec![],
                priority: None,
                flags: vec![],
            });
        } else if let Some(resolver) = result.last_mut() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key.starts_with("nameserver[") {
                resolver.servers.push(value.into());
            } else if key == "domain" || key.starts_with("search domain[") {
                resolver.domains.push(value.into());
            } else if key == "if_index" {
                resolver.interface_id = value
                    .split_once('(')
                    .map(|(_, name)| name.trim_end_matches(')').to_owned());
                if resolver.interface_id.is_none() {
                    resolver
                        .flags
                        .push(format!("unresolved_interface_index={value}"));
                }
            } else if key == "order" {
                resolver.priority = value.parse().ok();
            } else if matches!(key, "flags" | "options" | "reach" | "timeout") {
                resolver.flags.push(format!("{key}: {value}"));
            }
        }
    }
    result
}

pub(super) fn macos_hardware(text: &str) -> Vec<String> {
    // Hardware port classification is deliberately conservative. A device name
    // such as en0, bridge0 or utun0 alone is never evidence of physical hardware.
    let mut physical = false;
    let mut result = vec![];
    for line in text.lines() {
        if let Some(port) = line.strip_prefix("Hardware Port: ") {
            physical = matches!(
                port,
                "Wi-Fi" | "AirPort" | "Ethernet" | "Thunderbolt Ethernet"
            );
        } else if let Some(device) = line.strip_prefix("Device: ") {
            if physical {
                result.push(device.into());
            }
        }
    }
    result
}
pub(super) fn macos_routes(text: &str) -> Vec<Route> {
    let mut columns = None;
    let mut routes = vec![];
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.first() == Some(&"Destination") {
            columns = fields.iter().position(|v| *v == "Netif").map(|netif| {
                (
                    netif,
                    fields.iter().position(|v| *v == "Flags").unwrap_or(2),
                )
            });
            continue;
        }
        let Some((netif, flags)) = columns else {
            continue;
        };
        if fields.len() <= netif || fields.len() <= flags {
            continue;
        }
        routes.push(Route {
            destination: fields[0].into(),
            gateway: Some(fields[1].into()),
            interface_id: Some(fields[netif].into()),
            flags: vec![fields[flags].into()],
            source: "netstat -rn".into(),
        });
    }
    routes
}
pub(super) fn resolv_conf(text: &str) -> Vec<Resolver> {
    let mut resolver = Resolver {
        id: "resolv.conf:global".into(),
        source: "resolv.conf (global fallback; per-link policy unavailable)".into(),
        interface_id: None,
        scope: "global".into(),
        domains: vec![],
        servers: vec![],
        priority: None,
        flags: vec![],
    };
    for line in text.lines() {
        let fields: Vec<_> = line
            .split(['#', ';'])
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        match fields.as_slice() {
            ["nameserver", server, ..] => resolver.servers.push((*server).into()),
            ["search" | "domain", domains @ ..] => {
                resolver.domains.extend(domains.iter().map(|v| (*v).into()))
            }
            ["options", options @ ..] => resolver.flags.extend(options.iter().map(|v| (*v).into())),
            _ => {}
        }
    }
    vec![resolver]
}

pub(super) fn linux_routes(text: &str, source: &str) -> io::Result<Vec<Route>> {
    let values: Vec<Value> = serde_json::from_str(text).map_err(io::Error::other)?;
    Ok(values
        .iter()
        .map(|v| Route {
            destination: v["dst"].as_str().unwrap_or("default").into(),
            gateway: v["gateway"].as_str().map(str::to_owned),
            interface_id: v["dev"].as_str().map(str::to_owned),
            flags: v["flags"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            source: source.into(),
        })
        .collect())
}

/// Manager DNS/Domains properties include both global and per-link entries.
/// https://www.freedesktop.org/software/systemd/man/org.freedesktop.resolve1.html
pub(super) fn resolved(
    dns: &str,
    domains: &str,
    interfaces: &[Interface],
) -> io::Result<Vec<Resolver>> {
    let dns: Value = serde_json::from_str(dns).map_err(io::Error::other)?;
    let domains: Value = serde_json::from_str(domains).map_err(io::Error::other)?;
    if dns["type"] != "a(iiay)" || domains["type"] != "a(isb)" {
        return Err(io::Error::other(
            "unexpected systemd-resolved D-Bus signature",
        ));
    }
    let dns = dns["data"][0]
        .as_array()
        .ok_or_else(|| io::Error::other("invalid resolved DNS array"))?;
    let domains = domains["data"][0]
        .as_array()
        .ok_or_else(|| io::Error::other("invalid resolved domain array"))?;
    let mut result = std::collections::BTreeMap::<i64, Resolver>::new();
    let make = |index: i64| Resolver {
        id: format!("resolved:{index}"),
        source: "systemd-resolved D-Bus".into(),
        interface_id: interfaces
            .iter()
            .find(|i| i.index as i64 == index)
            .map(|i| i.id.clone()),
        scope: if index == 0 { "global" } else { "link" }.into(),
        domains: vec![],
        servers: vec![],
        priority: None,
        flags: if index != 0 {
            vec![format!("interface_index={index}")]
        } else {
            vec![]
        },
    };
    for entry in dns {
        let index = entry[0]
            .as_i64()
            .ok_or_else(|| io::Error::other("invalid DNS link index"))?;
        let family = entry[1]
            .as_i64()
            .ok_or_else(|| io::Error::other("invalid DNS address family"))?;
        let octets: Vec<u8> = entry[2]
            .as_array()
            .ok_or_else(|| io::Error::other("invalid DNS address"))?
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| io::Error::other("invalid DNS address octet"))
            })
            .collect::<io::Result<_>>()?;
        let address = match (family, octets.len()) {
            (2, 4) => std::net::Ipv4Addr::from(<[u8; 4]>::try_from(octets).unwrap()).to_string(),
            (10, 16) => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(octets).unwrap()).to_string(),
            _ => return Err(io::Error::other("unsupported DNS address family or size")),
        };
        result
            .entry(index)
            .or_insert_with(|| make(index))
            .servers
            .push(address);
    }
    for entry in domains {
        let index = entry[0]
            .as_i64()
            .ok_or_else(|| io::Error::other("invalid domain link index"))?;
        let name = entry[1]
            .as_str()
            .ok_or_else(|| io::Error::other("invalid domain name"))?;
        let routing = entry[2]
            .as_bool()
            .ok_or_else(|| io::Error::other("invalid routing-domain flag"))?;
        result
            .entry(index)
            .or_insert_with(|| make(index))
            .domains
            .push(if routing {
                format!("~{name}")
            } else {
                name.into()
            });
    }
    Ok(result.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scutil_preserves_split_dns_scope_order_and_interface() {
        let output = "DNS configuration\nresolver #1\n  search domain[0] : corp.test\n  nameserver[0] : 10.0.0.53\n  order : 200000\nDNS configuration (for scoped queries)\nresolver #1\n domain : corp.test\n nameserver[0] : fe80::1%en3\n if_index : 7 (en3)\n flags : Scoped, Request A records\n order : 100200";
        let rows = scutil_dns(output);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].scope, "global");
        assert_eq!(rows[1].scope, "scoped");
        assert_eq!(rows[1].interface_id.as_deref(), Some("en3"));
        assert_eq!(rows[1].priority, Some(100200));
        assert_eq!(rows[1].servers, ["fe80::1%en3"]);
        assert_ne!(rows[0].id, rows[1].id);
    }
    #[test]
    fn names_do_not_imply_physical_hardware() {
        let devices = macos_hardware("Hardware Port: Wi-Fi\nDevice: en0\nHardware Port: Thunderbolt Bridge\nDevice: bridge0\nHardware Port: VPN\nDevice: en7");
        assert_eq!(devices, ["en0"]);
    }
    #[test]
    fn global_fallback_never_claims_per_link_policy() {
        let rows = resolv_conf("# generated\nsearch corp.test example.test\nnameserver 127.0.0.53 # stub\noptions edns0");
        assert_eq!(rows[0].servers, ["127.0.0.53"]);
        assert_eq!(rows[0].domains.len(), 2);
        assert!(rows[0].interface_id.is_none());
        assert!(rows[0].source.contains("fallback"));
    }
    #[test]
    fn resolved_global_and_link_domains_are_not_conflated() {
        let dns = r#"{"type":"a(iiay)","data":[[[0,2,[1,1,1,1]],[7,2,[10,0,0,53]]]]}"#;
        let domains =
            r#"{"type":"a(isb)","data":[[[7,"corp.test",true],[0,"example.test",false]]]}"#;
        let rows = resolved(dns, domains, &[]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].scope, "global");
        assert_eq!(rows[1].scope, "link");
        assert_eq!(rows[1].domains, ["~corp.test"]);
        assert!(rows[1].interface_id.is_none());
        assert!(resolved("{}", domains, &[]).is_err());
    }
    #[test]
    fn macos_route_columns_are_read_from_header() {
        let rows = macos_routes("Destination Gateway Flags Netif Expire\ndefault 10.0.0.1 UGScg en0\n10.0.0.2 aa:bb UHLWI en0 20");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].interface_id.as_deref(), Some("en0"));
    }
}
