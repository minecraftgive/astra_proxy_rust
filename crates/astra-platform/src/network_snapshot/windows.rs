#![cfg_attr(not(windows), allow(dead_code))]
//! Structured Windows networking cmdlets use the OS CIM networking interfaces.
//! NRPT failure is independent of adapter DNS and is never reported as no policy.
use super::*;
use serde_json::Value;

async fn powershell(script: &str) -> io::Result<Value> {
    let text = command(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ],
    )
    .await?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(io::Error::other)
}
fn rows(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(rows) => rows.iter().collect(),
        Value::Null => vec![],
        _ => vec![value],
    }
}
fn strings(value: &Value) -> Vec<String> {
    rows(value)
        .into_iter()
        .filter_map(Value::as_str)
        .flat_map(|s| s.split([',', ';']).map(str::trim))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
pub(super) async fn network() -> (Vec<Interface>, Vec<Route>, Vec<Issue>) {
    let (adapters, addresses, routes) = tokio::join!(
        powershell("$ErrorActionPreference='Stop'; $items=@(Get-NetAdapter -IncludeHidden | Select-Object InterfaceIndex,Name,Status,HardwareInterface,Virtual); ConvertTo-Json -InputObject $items -Compress -Depth 4"),
        powershell("$ErrorActionPreference='Stop'; $items=@(Get-NetIPAddress | Select-Object InterfaceIndex,IPAddress,PrefixLength); ConvertTo-Json -InputObject $items -Compress -Depth 4"),
        powershell("$ErrorActionPreference='Stop'; $items=@(Get-NetRoute | Select-Object DestinationPrefix,NextHop,InterfaceIndex,RouteMetric,State); ConvertTo-Json -InputObject $items -Compress -Depth 4"));
    let mut issues = vec![];
    let mut interfaces = match adapters {
        Ok(value) => rows(&value)
            .iter()
            .filter_map(|v| {
                let index = u32::try_from(v["InterfaceIndex"].as_u64()?).ok()?;
                Some(Interface {
                    id: index.to_string(),
                    index,
                    name: v["Name"].as_str()?.into(),
                    kind: if v["Virtual"] == true {
                        InterfaceKind::Virtual
                    } else if v["HardwareInterface"] == true {
                        InterfaceKind::Physical
                    } else {
                        InterfaceKind::Unknown
                    },
                    connected: v["Status"].as_str() == Some("Up"),
                    addresses: vec![],
                    source: "Windows Get-NetAdapter CIM".into(),
                })
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            issues.push(issue("interfaces", error));
            vec![]
        }
    };
    match addresses {
        Ok(value) => {
            for v in rows(&value) {
                if let Some(index) = v["InterfaceIndex"].as_u64() {
                    if let Some(interface) = interfaces.iter_mut().find(|i| i.index as u64 == index)
                    {
                        if let Some(address) = v["IPAddress"].as_str() {
                            interface.addresses.push(Address {
                                address: address.into(),
                                prefix_len: v["PrefixLength"]
                                    .as_u64()
                                    .and_then(|v| u8::try_from(v).ok()),
                            });
                        }
                    }
                }
            }
        }
        Err(error) => issues.push(issue("addresses", error)),
    }
    let routes = match routes {
        Ok(value) => rows(&value)
            .into_iter()
            .filter_map(|v| {
                Some(Route {
                    destination: v["DestinationPrefix"].as_str()?.into(),
                    gateway: v["NextHop"].as_str().map(str::to_owned),
                    interface_id: v["InterfaceIndex"].as_u64().map(|v| v.to_string()),
                    flags: vec![format!("metric={}", v["RouteMetric"])],
                    source: "Windows Get-NetRoute CIM".into(),
                })
            })
            .collect(),
        Err(error) => {
            issues.push(issue("routes", error));
            vec![]
        }
    };
    (interfaces, routes, issues)
}
pub(super) async fn dns() -> (Vec<Resolver>, Vec<Issue>) {
    let (adapters, policy) = tokio::join!(
        powershell("$ErrorActionPreference='Stop'; $items=@(Get-DnsClientServerAddress | Select-Object InterfaceIndex,ServerAddresses,AddressFamily); ConvertTo-Json -InputObject $items -Compress -Depth 5"),
        powershell("$ErrorActionPreference='Stop'; $items=@(Get-DnsClientNrptPolicy -Effective | Select-Object Namespace,NameServers,DirectAccessDnsServers,QueryPolicy); ConvertTo-Json -InputObject $items -Compress -Depth 5"));
    let mut issues = vec![];
    let mut resolvers = vec![];
    match adapters {
        Ok(value) => {
            for v in rows(&value) {
                let Some(index) = v["InterfaceIndex"].as_u64() else {
                    issues.push(issue("dns", "invalid adapter DNS interface index"));
                    continue;
                };
                resolvers.push(Resolver {
                    id: format!("windows:{index}:{}", v["AddressFamily"]),
                    source: "Windows Get-DnsClientServerAddress".into(),
                    interface_id: Some(index.to_string()),
                    scope: "link".into(),
                    domains: vec![],
                    servers: strings(&v["ServerAddresses"]),
                    priority: None,
                    flags: vec![],
                });
            }
        }
        Err(error) => issues.push(issue("dns", error)),
    }
    match policy.and_then(|value| nrpt(&value)) {
        Ok(value) => resolvers.extend(value),
        Err(error) => issues.push(issue(
            "nrpt",
            format!("Effective NRPT query failed; policy is unknown: {error}"),
        )),
    }
    (resolvers, issues)
}

fn nrpt(value: &Value) -> io::Result<Vec<Resolver>> {
    rows(value)
        .into_iter()
        .enumerate()
        .map(|(index, v)| {
            if !v.is_object()
                || !v
                    .get("Namespace")
                    .is_some_and(|v| v.is_array() || v.is_string())
            {
                return Err(io::Error::other("invalid effective NRPT policy response"));
            }
            Ok(Resolver {
                id: format!("windows:nrpt:{index}"),
                source: "Windows effective NRPT".into(),
                interface_id: None,
                scope: "policy".into(),
                domains: strings(&v["Namespace"]),
                servers: strings(&v["NameServers"]),
                priority: None,
                flags: vec![
                    format!("QueryPolicy={}", v["QueryPolicy"]),
                    format!("DirectAccessDnsServers={}", v["DirectAccessDnsServers"]),
                ],
            })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effective_nrpt_preserves_namespaces_and_servers() {
        let value = serde_json::json!([{ "Namespace": [".corp.test"], "NameServers": "10.0.0.53,10.0.0.54", "QueryPolicy": 0 }]);
        let rows = nrpt(&value).unwrap();
        assert_eq!(rows[0].domains, [".corp.test"]);
        assert_eq!(rows[0].servers, ["10.0.0.53", "10.0.0.54"]);
        assert_eq!(rows[0].scope, "policy");
        assert!(rows[0].interface_id.is_none());
    }
    #[test]
    fn malformed_policy_is_distinct_from_no_effective_rules() {
        assert!(nrpt(&serde_json::json!([])).unwrap().is_empty());
        assert!(nrpt(&serde_json::json!({"unexpected": true})).is_err());
        assert!(nrpt(&serde_json::json!("denied")).is_err());
    }
}
