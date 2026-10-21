//! Physical default-route discovery; command details are isolated from runtime policy.
use super::egress::PhysicalEgress;
use anyhow::{Context, Result};
use std::process::Command;

pub(super) fn physical_egress(
    selected: Option<&str>,
    exclude: Option<&str>,
) -> Result<PhysicalEgress> {
    #[cfg(target_os = "linux")]
    {
        let mut defaults = vec![];
        for family in ["-4", "-6"] {
            let output = Command::new("ip")
                .args(["-j", family, "route", "show", "default"])
                .output()?;
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
                    && !std::path::Path::new("/sys/class/net")
                        .join(name)
                        .join("device")
                        .exists()
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
        defaults.sort();
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
        return Ok(result);
    }
    #[cfg(target_os = "macos")]
    {
        let mut result = PhysicalEgress {
            interface: selected.unwrap_or("").to_owned(),
            gateway: None,
            gateway6: None,
        };
        for family in ["-inet", "-inet6"] {
            let output = Command::new("/sbin/route")
                .args(["-n", "get", family, "default"])
                .output()?;
            if !output.status.success() {
                continue;
            }
            let text = String::from_utf8(output.stdout)?;
            let field = |key: &str| {
                text.lines()
                    .find_map(|line| line.trim().strip_prefix(key).map(str::trim))
            };
            let Some(interface) = field("interface:") else {
                continue;
            };
            if exclude == Some(interface)
                || interface.starts_with("utun")
                || interface == "lo0"
                || selected.is_some_and(|name| name != interface)
            {
                continue;
            }
            if result.interface.is_empty() {
                result.interface = interface.to_owned();
            }
            if result.interface != interface {
                continue;
            }
            if let Some(gateway) = field("gateway:")
                .and_then(|raw| raw.split('%').next())
                .and_then(|raw| raw.parse::<std::net::IpAddr>().ok())
            {
                match gateway {
                    std::net::IpAddr::V4(ip) => result.gateway = Some(ip),
                    std::net::IpAddr::V6(ip) => result.gateway6 = Some(ip),
                }
            }
        }
        anyhow::ensure!(!result.interface.is_empty() && (result.gateway.is_some() || result.gateway6.is_some()),"no physical default route; specify tun.interface and disable auto_gateway for a static gateway");
        return Ok(result);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("automatic egress discovery unsupported on this OS")
}
