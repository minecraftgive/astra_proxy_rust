use crate::App;
use anyhow::Result;

pub(crate) fn configure_tun(
    dns: &mut astra_dns::Resolver,
    tun: &mut crate::mode_config::Tun,
) -> Result<()> {
    if dns.config.hijack
        && (dns.config.system_takeover || dns.config.route_system_dns)
        && tun.ipv6_address.is_some()
        && !tun.routes.iter().any(|route| route.contains(':'))
    {
        tun.routes
            .extend(["::/1".to_owned(), "8000::/1".to_owned()]);
    }
    for range in dns.config.active_fake_ranges() {
        anyhow::ensure!(
            matches!(range, ipnet::IpNet::V4(_)) || tun.ipv6_address.is_some(),
            "IPv6 Fake IP with TUN requires tun.ipv6_address"
        );
        if !tun.routes.iter().any(|cidr| {
            cidr.parse::<ipnet::IpNet>()
                .is_ok_and(|net| net.trunc() == range)
        }) {
            tun.routes.push(range.to_string());
        }
    }
    let subnet = ipnet::Ipv4Net::new(tun.address, tun.prefix)?;
    for address in [tun.address, subnet.network(), subnet.broadcast()] {
        dns.reserve_address(address.into());
    }
    if let Some(address) = crate::platform::system_dns::dns_address(tun) {
        dns.reserve_address(address.into());
    }
    if let Some(address) = tun.ipv6_address {
        let subnet = ipnet::Ipv6Net::new(address, tun.ipv6_prefix)?;
        dns.reserve_address(address.into());
        dns.reserve_address(subnet.network().into());
    }
    Ok(())
}

impl App {
    pub(crate) fn apply_runtime_modes(
        &mut self,
        state: &astra_model::runtime::Overrides,
    ) -> Result<()> {
        let routing = &mut **self;
        crate::mode_config::runtime_modes(&mut routing.config, state);
        if let (Some(dns), Some(tun)) = (&mut routing.dns, &mut routing.config.tun) {
            configure_tun(dns, tun)?;
        }
        Ok(())
    }
    pub(crate) fn inbound_allowed(&self, ip: std::net::IpAddr) -> bool {
        let ip = ip.to_canonical();
        let allow_lan = self.overrides.load().allow_lan.unwrap_or(false);
        (allow_lan || ip.is_loopback())
            && (self.config.lan_allowed_ips.is_empty()
                || self
                    .config
                    .lan_allowed_ips
                    .iter()
                    .any(|cidr| cidr.contains(&ip)))
            && !self
                .config
                .lan_disallowed_ips
                .iter()
                .any(|cidr| cidr.contains(&ip))
    }
    pub(crate) fn authentication(&self, ip: Option<std::net::IpAddr>) -> &[String] {
        if ip.is_some_and(|ip| {
            self.config
                .skip_auth_prefixes
                .iter()
                .any(|cidr| cidr.contains(&ip.to_canonical()))
        }) {
            &[]
        } else {
            &self.config.authentication
        }
    }
    pub(crate) fn lan_enabled(&self) -> bool {
        let allowed = self.overrides.load().allow_lan.unwrap_or(false);
        allowed
            && self
                .config
                .socks5_listen
                .iter()
                .chain(&self.config.http_listen)
                .chain(&self.config.mixed_listen)
                .any(|address| {
                    address
                        .parse::<std::net::SocketAddr>()
                        .is_ok_and(|a| !a.ip().is_loopback())
                })
    }
}
