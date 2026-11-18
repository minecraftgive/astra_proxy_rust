//! Physical default-route discovery; command details are isolated from runtime policy.
use super::egress::PhysicalEgress;
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use std::process::Command;

pub(super) fn physical_egress(
    selected: Option<&str>,
    exclude: Option<&str>,
) -> Result<PhysicalEgress> {
    #[cfg(target_os = "linux")]
    {
        let mut defaults = vec![];
        for family in ["-4", "-6"] {
            let output = astra_platform::command::output(
                Command::new("ip").args(["-j", family, "route", "show", "default"]),
            )?;
            anyhow::ensure!(
                output.status.success(),
                "read physical default routes failed"
            );
            let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
            for row in rows {
                let Some(name) = row["dev"].as_str() else {
                    continue;
                };
                if name == "lo"
                    || exclude == Some(name)
                    || selected.is_some_and(|value| value != name)
                {
                    continue;
                }
                let flags = row["flags"].as_array();
                if flags
                    .is_some_and(|flags| flags.iter().any(|flag| flag.as_str() == Some("linkdown")))
                {
                    continue;
                }
                // Auto mode must not select an existing tunnel or virtual VPN.
                if selected.is_none()
                    && !hardware_backed(&std::path::Path::new("/sys/class/net").join(name), 0)
                {
                    continue;
                }
                defaults.push((
                    family == "-6",
                    row["metric"].as_u64().unwrap_or(0),
                    name.to_owned(),
                    row["gateway"]
                        .as_str()
                        .and_then(|raw| raw.parse::<std::net::IpAddr>().ok()),
                ));
            }
        }
        defaults.sort_by_key(|(ipv6, metric, _, _)| (*ipv6, *metric));
        let interface = defaults
            .first()
            .context("no active physical default route; specify tun.interface")?
            .2
            .clone();
        let mut result = PhysicalEgress {
            interface: interface.clone(),
            gateway: None,
            gateway6: None,
        };
        for (_, _, name, gateway) in defaults {
            if name == interface {
                match gateway {
                    Some(std::net::IpAddr::V4(ip)) if result.gateway.is_none() => {
                        result.gateway = Some(ip)
                    }
                    Some(std::net::IpAddr::V6(ip)) if result.gateway6.is_none() => {
                        result.gateway6 = Some(ip)
                    }
                    _ => {}
                }
            }
        }
        Ok(result)
    }
    #[cfg(target_os = "macos")]
    {
        let mut result = PhysicalEgress {
            interface: String::new(),
            gateway: None,
            gateway6: None,
        };
        for family in ["inet", "inet6"] {
            // A route lookup can return the capturing tunnel. Inspect actual
            // /0 rows, including scoped defaults for the physical adapter.
            let output = astra_platform::command::output(
                Command::new("/usr/sbin/netstat").args(["-rn", "-f", family]),
            )?;
            anyhow::ensure!(
                output.status.success(),
                "read {family} default routes failed"
            );
            let table = String::from_utf8(output.stdout)?;
            let wanted = selected
                .or_else(|| (!result.interface.is_empty()).then_some(result.interface.as_str()));
            if let Some((interface, gateway)) = mac_default(&table, wanted, exclude) {
                result.interface = interface.to_owned();
                match gateway {
                    Some(std::net::IpAddr::V4(ip)) => result.gateway = Some(ip),
                    Some(std::net::IpAddr::V6(ip)) => result.gateway6 = Some(ip),
                    None => {}
                }
            }
        }
        anyhow::ensure!(
            !result.interface.is_empty(),
            "no active default route for the physical egress; specify tun.interface"
        );
        Ok(result)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("automatic egress discovery unsupported on this OS")
}

/// Linux routes through VLANs, bridges and bonds name the logical interface.
#[cfg(any(target_os = "linux", test))]
fn hardware_backed(link: &std::path::Path, depth: usize) -> bool {
    link.join("device").exists()
        || (depth < 8
            && std::fs::read_dir(link).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry.file_name().as_encoded_bytes().starts_with(b"lower_")
                        && hardware_backed(&entry.path(), depth + 1)
                })
            }))
}

#[cfg(target_os = "macos")]
fn mac_default<'a>(
    table: &'a str,
    selected: Option<&str>,
    exclude: Option<&str>,
) -> Option<(&'a str, Option<std::net::IpAddr>)> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != "default" {
                return None;
            }
            let gateway = fields.next()?;
            let flags = fields.next()?;
            let interface = fields.next()?;
            if !flags.contains('U')
                || flags.contains(['R', 'B'])
                || interface == "lo0"
                || exclude == Some(interface)
                || selected.is_some_and(|wanted| wanted != interface)
                || (selected.is_none() && interface.starts_with("utun"))
            {
                return None;
            }
            Some((
                flags.contains('I'),
                interface,
                gateway.split('%').next()?.parse().ok(),
            ))
        })
        .min_by_key(|(scoped, _, _)| *scoped)
        .map(|(_, interface, gateway)| (interface, gateway))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_logical_link_can_reach_hardware_through_lower_devices() {
        let root = std::env::temp_dir().join(format!("astra-netdev-{}", uuid::Uuid::new_v4()));
        let bridge = root.join("br0");
        let vlan = root.join("eth0.10");
        let nic = root.join("eth0");
        std::fs::create_dir_all(nic.join("device")).unwrap();
        std::fs::create_dir(&bridge).unwrap();
        std::fs::create_dir(&vlan).unwrap();
        std::os::unix::fs::symlink(&vlan, bridge.join("lower_eth0.10")).unwrap();
        std::os::unix::fs::symlink(&nic, vlan.join("lower_eth0")).unwrap();
        assert!(hardware_backed(&bridge, 0));
        assert!(!hardware_backed(&root, 0));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_default_uses_underlying_route_and_keeps_gateway_on_selected_link() {
        let table = "default link#42 UCSg utun9\ndefault 10.0.0.1 UGScIg en7\ndefault 192.0.2.1 UGScg en0\n0/1 link#42 USc utun9";
        assert_eq!(
            mac_default(table, None, None),
            Some(("en0", Some("192.0.2.1".parse().unwrap())))
        );
        assert_eq!(mac_default(table, Some("en7"), None).unwrap().0, "en7");
        assert!(mac_default(table, Some("utun9"), Some("utun9")).is_none());
        let v6 = "default fe80::1%en7 UGcIg en7";
        assert_eq!(
            mac_default(v6, None, None),
            Some(("en7", Some("fe80::1".parse().unwrap())))
        );
        assert!(mac_default(v6, Some("en0"), None).is_none());
    }
}
