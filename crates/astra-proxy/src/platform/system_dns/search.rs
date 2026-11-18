//! Search domains are saved with the resolver takeover so both roll back together.
use super::{args, Change, NETWORKSETUP, RESOLVECTL};
use anyhow::{ensure, Context, Result};
use astra_model::config::dns::valid_search_domain;
use std::process::Command;

pub(super) const MAC_DYNAMIC: &str = "mac-service-search-domains";
pub(super) const WIN_SERVERS_SET: &str = "windows-set-ipv4-dns";
pub(super) const WIN_SERVERS_READ: &str = "windows-read-ipv4-dns";
pub(super) const WIN_BY_GUID: &str = "windows-read-dns-by-guid";
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
    let mut restore = base;
    restore.extend(super::resolved_values(&raw)?);
    Ok(Change { apply, restore })
}

pub(super) fn linux_domain(value: &str) -> bool {
    value == "~." || valid_search_domain(value.strip_prefix('~').unwrap_or(value))
}

pub(super) fn windows(guid: &str, suffix: &str, old: &str) -> Result<Option<Change>> {
    ensure!(
        old.is_empty() || valid_search_domain(old),
        "invalid previous Windows DNS suffix"
    );
    if same_domain(old, suffix) {
        return Ok(None);
    }
    Ok(Some(Change {
        apply: args(&[WIN_SET, guid, suffix]),
        restore: args(&[WIN_SET, guid, old]),
    }))
}

// Use the interface GUID for replay. An empty previous manual suffix means
// reset to the system/DHCP default. Values travel in the environment, never
// as interpolated PowerShell source.
pub(super) fn windows_command(argv: &[String]) -> Result<Command> {
    let mut command = Command::new("powershell.exe");
    let (op, rest) = argv
        .split_first()
        .context("missing Windows DNS operation")?;
    let (identity, values) = rest.split_first().context("missing adapter identity")?;
    command.env("ASTRA_DNS_LINK", identity);
    let by_name = op == WIN_SERVERS_READ;
    let lookup = if by_name {
        "Get-NetAdapter -IncludeHidden | Where-Object Name -eq $env:ASTRA_DNS_LINK"
    } else {
        uuid::Uuid::parse_str(identity)?;
        "Get-NetAdapter -IncludeHidden | Where-Object { ([guid]$_.InterfaceGuid) -eq ([guid]$env:ASTRA_DNS_LINK) }"
    };
    let action = match op.as_str() {
        WIN_SERVERS_READ | WIN_BY_GUID => {
            "ConvertTo-Json -Compress -InputObject @($id, $servers, [string]$settings.Domain)"
        }
        WIN_SERVERS_SET => {
            command.env("ASTRA_DNS_SERVERS", values.join(","));
            r#"$client = Get-DnsClientServerAddress -InterfaceIndex $a.ifIndex -AddressFamily IPv4
if ($env:ASTRA_DNS_SERVERS) {
    $client | Set-DnsClientServerAddress -ServerAddresses ($env:ASTRA_DNS_SERVERS -split ',')
} else {
    $client | Set-DnsClientServerAddress -ResetServerAddresses
}"#
        }
        WIN_SET if values.len() == 1 => {
            command.env("ASTRA_DNS_DOMAIN", &values[0]);
            r#"if ($env:ASTRA_DNS_DOMAIN) {
    Set-DnsClient -InterfaceIndex $a.ifIndex -ConnectionSpecificSuffix $env:ASTRA_DNS_DOMAIN
} else {
    Set-DnsClient -InterfaceIndex $a.ifIndex -ResetConnectionSpecificSuffix
}"#
        }
        _ => anyhow::bail!("unexpected Windows DNS operation"),
    };
    let script = format!(
        r#"$ErrorActionPreference='Stop'
$a = @({lookup})
if ($a.Count -eq 0) {{ Write-Output 'null'; exit 0 }}
if ($a.Count -ne 1) {{ throw 'Cannot identify DNS adapter' }}
$id = ([guid]$a[0].InterfaceGuid).ToString('B')
$settings = Get-ItemProperty -LiteralPath ('HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\' + $id)
$servers = @(([string]$settings.NameServer -split '[,;\s]+') | Where-Object {{ $_ }})
{action}"#
    );
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    Ok(command)
}

#[cfg(target_os = "macos")]
pub(crate) fn mac_service_id(service: &str) -> Result<String> {
    use system_configuration::{
        core_foundation::{array::CFArray, base::TCFType, string::CFString},
        network_configuration::{SCNetworkService, SCNetworkSet},
        preferences::SCPreferences,
        sys::network_configuration::{
            SCNetworkServiceGetName, SCNetworkSetCopyCurrent, SCNetworkSetCopyServices,
        },
    };
    // The dynamic store omits inactive services such as an unplugged USB
    // adapter. networksetup addresses services in the current preferences set.
    let prefs = SCPreferences::default(&CFString::new("Astra DNS takeover"));
    let raw_set = unsafe { SCNetworkSetCopyCurrent(prefs.as_concrete_TypeRef()) };
    ensure!(!raw_set.is_null(), "no current macOS network location");
    let set = unsafe { SCNetworkSet::wrap_under_create_rule(raw_set) };
    let raw = unsafe { SCNetworkSetCopyServices(set.as_concrete_TypeRef()) };
    ensure!(!raw.is_null(), "current network location has no services");
    let services = unsafe { CFArray::<SCNetworkService>::wrap_under_create_rule(raw) };
    let mut id = None;
    for candidate in &services {
        let name = unsafe { SCNetworkServiceGetName(candidate.as_concrete_TypeRef()) };
        if !name.is_null() && unsafe { CFString::wrap_under_get_rule(name) }.to_string() == service
        {
            ensure!(id.is_none(), "ambiguous macOS network service name");
            id = Some(
                candidate
                    .id()
                    .context("network service has no ID")?
                    .to_string(),
            );
        }
    }
    id.with_context(|| format!("network service {service} is missing or renamed"))
}

// networksetup only reports manual settings. Read the service's live DHCP
// search list before overriding it, then restore automatic mode on exit.
#[cfg(target_os = "macos")]
pub(super) fn mac_service(service: &str) -> Result<(String, String)> {
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
    let id = mac_service_id(service)?;
    let store = SCDynamicStoreBuilder::new("Astra DNS search domains").build();
    let state = format!("State:/Network/Service/{id}/DNS");
    let Some(dns) = store
        .get(state.as_str())
        .and_then(|v| v.downcast_into::<CFDictionary>())
    else {
        return Ok((id, String::new()));
    };
    if let Some(domains) = field(&dns, "SearchDomains").and_then(|v| v.downcast_into::<CFArray>()) {
        let mut values = Vec::new();
        for value in &domains {
            let text = unsafe { CFType::wrap_under_get_rule(*value) }
                .downcast_into::<CFString>()
                .context("invalid DHCP search domain")?;
            values.push(text.to_string());
        }
        if !values.is_empty() {
            return Ok((id, values.join("\n")));
        }
    }
    if let Some(domain) = field(&dns, "DomainName").and_then(|v| v.downcast_into::<CFString>()) {
        return Ok((id, domain.to_string()));
    }
    Ok((id, String::new()))
}
