//! Search domains are saved with the resolver takeover so both roll back together.
use super::{args, Change, NETWORKSETUP, RESOLVECTL};
use anyhow::{ensure, Context, Result};
use astra_model::config::dns::valid_search_domain;
use std::process::Command;

pub(super) const MAC_DYNAMIC: &str = "mac-service-search-domains";
pub(super) const WIN_READ: &str = "windows-read-dns-suffix";
pub(super) const WIN_SET: &str = "windows-set-dns-suffix";

fn same_domain(left: &str, right: &str) -> bool {
    left.trim_end_matches('.')
        .eq_ignore_ascii_case(right.trim_end_matches('.'))
}

pub(super) fn macos(
    service: &str,
    suffix: &str,
    query: &mut impl FnMut(&[String]) -> Result<String>,
) -> Result<Option<Change>> {
    let raw = query(&args(&[NETWORKSETUP, "-getsearchdomains", service]))?;
    let raw = raw.trim();
    let base = args(&[NETWORKSETUP, "-setsearchdomains", service]);
    let mut restore = base.clone();
    let dynamic;
    let old = if raw == format!("There aren't any Search Domains set on {service}.") {
        restore.push("Empty".into());
        dynamic = query(&args(&[MAC_DYNAMIC, service]))?;
        dynamic.as_str()
    } else {
        ensure!(
            !raw.is_empty(),
            "could not read search domains for {service}"
        );
        restore.extend(raw.lines().map(str::trim).map(str::to_owned));
        raw
    };
    let old: Vec<_> = old.lines().map(str::trim).collect();
    ensure!(
        old.iter().all(|value| valid_search_domain(value)),
        "invalid search domains for {service}"
    );
    if old.iter().any(|value| same_domain(value, suffix)) {
        return Ok(None);
    }
    let mut apply = base;
    apply.push(suffix.to_owned());
    apply.extend(old.into_iter().map(str::to_owned));
    Ok(Some(Change { apply, restore }))
}

pub(super) fn linux(
    link: &str,
    suffix: Option<&str>,
    query: &mut impl FnMut(&[String]) -> Result<String>,
) -> Result<Change> {
    let base = args(&[RESOLVECTL, "domain", link]);
    let raw = query(&base)?;
    let (_, values) = raw
        .split_once(':')
        .context("unrecognized resolvectl domain output")?;
    let old: Vec<_> = values.split_whitespace().collect();
    ensure!(
        old.iter().all(|value| linux_domain(value)),
        "invalid resolved link domains"
    );
    let mut apply = base.clone();
    if let Some(suffix) = suffix {
        apply.push(suffix.to_owned());
    }
    for value in &old {
        // An existing route-only entry for the same zone must become searchable.
        if !suffix.is_some_and(|s| same_domain(s, value.trim_start_matches('~'))) {
            apply.push((*value).to_owned());
        }
    }
    if !old.contains(&"~.") {
        apply.push("~.".into());
    }
    let restore = if old.is_empty() {
        args(&[RESOLVECTL, "revert", link])
    } else {
        let mut restore = base;
        restore.extend(old.into_iter().map(str::to_owned));
        restore
    };
    Ok(Change { apply, restore })
}

pub(super) fn linux_domain(value: &str) -> bool {
    value == "~." || valid_search_domain(value.strip_prefix('~').unwrap_or(value))
}

pub(super) fn windows(
    interface: &str,
    suffix: &str,
    query: &mut impl FnMut(&[String]) -> Result<String>,
) -> Result<Option<Change>> {
    let raw = query(&args(&[WIN_READ, interface]))?;
    let (guid, old): (uuid::Uuid, String) = serde_json::from_str(raw.trim())?;
    ensure!(
        old.is_empty() || valid_search_domain(&old),
        "invalid previous Windows DNS suffix"
    );
    if same_domain(&old, suffix) {
        return Ok(None);
    }
    let guid = guid.to_string();
    Ok(Some(Change {
        apply: args(&[WIN_SET, &guid, suffix]),
        restore: args(&[WIN_SET, &guid, &old]),
    }))
}

// Use the interface GUID for replay. An empty previous manual suffix means
// reset to the system/DHCP default. Values travel in the environment, never
// as interpolated PowerShell source.
pub(super) fn windows_command(argv: &[String]) -> Result<Command> {
    let mut command = Command::new("powershell.exe");
    let script = match argv {
        [op, interface] if op == WIN_READ => {
            command.env("ASTRA_DNS_LINK", interface);
            r#"$ErrorActionPreference='Stop'
$a = Get-NetAdapter -IncludeHidden | Where-Object Name -eq $env:ASTRA_DNS_LINK
if (@($a).Count -ne 1) { throw 'Cannot identify TUN adapter' }
$id = ([guid]$a.InterfaceGuid).ToString('B')
$settings = Get-ItemProperty -LiteralPath ('HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\' + $id)
ConvertTo-Json -Compress -InputObject @($id, [string]$settings.Domain)"#
        }
        [op, guid, domain] if op == WIN_SET => {
            command
                .env("ASTRA_DNS_GUID", guid)
                .env("ASTRA_DNS_DOMAIN", domain);
            r#"$ErrorActionPreference='Stop'
$a = Get-NetAdapter -IncludeHidden | Where-Object { ([guid]$_.InterfaceGuid) -eq ([guid]$env:ASTRA_DNS_GUID) }
if (@($a).Count -ne 1) { throw 'Cannot identify TUN adapter' }
if ($env:ASTRA_DNS_DOMAIN) {
    Set-DnsClient -InterfaceIndex $a.ifIndex -ConnectionSpecificSuffix $env:ASTRA_DNS_DOMAIN
} else {
    Set-DnsClient -InterfaceIndex $a.ifIndex -ResetConnectionSpecificSuffix
}"#
        }
        _ => anyhow::bail!("unexpected Windows DNS suffix operation"),
    };
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    Ok(command)
}

// networksetup only reports manual settings. Read the service's live DHCP
// search list before overriding it, then restore automatic mode on exit.
#[cfg(target_os = "macos")]
pub(super) fn mac_dynamic(service: &str) -> Result<String> {
    use system_configuration::{
        core_foundation::{
            array::CFArray,
            base::{CFType, TCFType, ToVoid},
            dictionary::CFDictionary,
            string::CFString,
        },
        dynamic_store::SCDynamicStoreBuilder,
    };
    fn field(dict: &CFDictionary, name: &str) -> Option<CFType> {
        let key = CFString::new(name);
        let value = dict.find(key.as_concrete_TypeRef().to_void())?;
        Some(unsafe { CFType::wrap_under_get_rule(*value) })
    }
    let store = SCDynamicStoreBuilder::new("Astra DNS search domains").build();
    let services = store
        .get_keys("Setup:/Network/Service/[^/]+$")
        .context("list macOS network services")?;
    for key in &services {
        let Some(setup) = store
            .get(key.clone())
            .and_then(|v| v.downcast_into::<CFDictionary>())
        else {
            continue;
        };
        if field(&setup, "UserDefinedName")
            .and_then(|v| v.downcast_into::<CFString>())
            .is_none_or(|v| v.to_string() != service)
        {
            continue;
        }
        let state = key.to_string().replacen("Setup:", "State:", 1) + "/DNS";
        let Some(dns) = store
            .get(state.as_str())
            .and_then(|v| v.downcast_into::<CFDictionary>())
        else {
            break;
        };
        if let Some(domains) =
            field(&dns, "SearchDomains").and_then(|v| v.downcast_into::<CFArray>())
        {
            let mut values = Vec::new();
            for value in &domains {
                let text = unsafe { CFType::wrap_under_get_rule(*value) }
                    .downcast_into::<CFString>()
                    .context("invalid DHCP search domain")?;
                values.push(text.to_string());
            }
            if !values.is_empty() {
                return Ok(values.join("\n"));
            }
        }
        if let Some(domain) = field(&dns, "DomainName").and_then(|v| v.downcast_into::<CFString>())
        {
            return Ok(domain.to_string());
        }
        break;
    }
    Ok(String::new())
}
