pub mod platform;
mod proxy;
use platform::system_proxy;
mod config;
use platform::egress;
mod inbound;
pub use astra_protocols::native;
pub use astra_protocols::outbound;
pub use astra_protocols::tuic;
use platform::mode_config;
use platform::transparent;
use platform::tun;
use proxy::limit;
use proxy::packet;
use proxy::rule_client;
use proxy::runtime;
mod diagnostics;
mod dns;
mod http_forward;
mod subscription;
pub use astra_protocols::xhttp;
use proxy::udp;

use anyhow::{bail, Context, Result};
use astra_ipc::{Inbound, Network, ProcessInfo, RouteAction, RouteRequest, Target, API_VERSION};
use config::{OutboundConfig, ProxyConfig};
use moka::sync::Cache;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::timeout;

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    destination_ip: Option<std::net::IpAddr>,
    target: Target,
    revision: u64,
    network: Network,
    inbound: Inbound,
    process: Option<ProcessInfo>,
}

pub struct App {
    metrics: Arc<proxy::traffic::Metrics>,
    overrides: Arc<std::sync::RwLock<astra_ipc::runtime::Overrides>>,
    egress_signal: Arc<tokio::sync::watch::Sender<bool>>,
    dns: Option<astra_dns::Resolver>,
    config: ProxyConfig,
    source_text: String,
    route_leases: Vec<Arc<tun::OwnedRoute>>,
    outbounds: HashMap<String, OutboundConfig>,
    groups: proxy::groups::Groups,
    rule_client: Option<rule_client::RuleClient>,
    standalone_action: Option<RouteAction>,
    cache: Cache<CacheKey, RouteAction>,
    native: native::Registry,
    native_interfaces: HashMap<String, native::Registry>,
    udp_permits: Arc<limit::Limit>,
    permits: Arc<limit::Limit>,
}

impl App {
    fn inbound_allowed(&self, ip: std::net::IpAddr) -> bool {
        let ip = ip.to_canonical();
        let allow_lan = self.overrides.read().unwrap().allow_lan.unwrap_or(false);
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
    fn authentication(&self, ip: Option<std::net::IpAddr>) -> &[String] {
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
    fn lan_enabled(&self) -> bool {
        let allowed = self.overrides.read().unwrap().allow_lan.unwrap_or(false);
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
    pub(crate) async fn egress_cancelled(&self) {
        let mut changed = self.egress_signal.subscribe();
        let _ = changed.wait_for(|changed| *changed).await;
    }
    pub async fn route(&self, target: &Target, inbound: Inbound) -> Result<RouteAction> {
        self.route_network(target, inbound, Network::Tcp).await
    }

    pub async fn route_network(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
    ) -> Result<RouteAction> {
        self.route_with_process(target, inbound, network, None)
            .await
    }

    pub(crate) async fn route_connection(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
        source: Option<std::net::SocketAddr>,
        destination: Option<std::net::SocketAddr>,
    ) -> Result<RouteAction> {
        // Transparent self-capture is unsafe even in standalone mode.
        let process = if self.standalone_action.is_none() || inbound == Inbound::Tun {
            match (source, destination) {
                (Some(source), Some(destination)) => {
                    platform::process::lookup(network, source, destination).await
                }
                _ => None,
            }
        } else {
            None
        };
        // Explicit proxy listeners report their own local socket here, not
        // the requested endpoint. Only transparent inbounds observe the latter.
        let destination_ip = match inbound {
            Inbound::Tun | Inbound::Tproxy | Inbound::Redirect => {
                destination.map(|address| address.ip())
            }
            Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed => target.host.parse().ok(),
        }
        .filter(|ip| !self.synthetic_ip(*ip));
        self.route_with_context(target, inbound, network, process, destination_ip)
            .await
    }

    async fn route_with_process(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
        process: Option<ProcessInfo>,
    ) -> Result<RouteAction> {
        self.route_with_context(target, inbound, network, process, None)
            .await
    }
    async fn route_with_context(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
        process: Option<ProcessInfo>,
        destination_ip: Option<std::net::IpAddr>,
    ) -> Result<RouteAction> {
        let restored = self.restore_fake_target(target)?;
        let target = &restored;
        if inbound == Inbound::Tun
            && process
                .as_ref()
                .is_some_and(|process| process.pid == std::process::id())
        {
            bail!("TUN recaptured a proxy-core connection; check tun.interface and outbound bypass routes");
        }
        let result = self
            .decide_network(target, inbound, network, process.clone(), destination_ip)
            .await;
        let result = match result {
            Ok(action) => self.resolve_action(&action).await.map(|resolved| {
                if resolved == RouteAction::Block {
                    resolved
                } else {
                    action
                }
            }),
            Err(error) => Err(error),
        };
        match &result {
            Ok(action) => {
                tracing::debug!(target: "astra_route", phase = "route", host = ?target.host, port = target.port, ?destination_ip, ?inbound, ?network, ?process, ?action, "route selected")
            }
            Err(error) => {
                tracing::debug!(target: "astra_route", phase = "route_failed", host = ?target.host, port = target.port, ?inbound, ?network, %error, "routing failed")
            }
        }
        result
    }

    async fn decide_network(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
        process: Option<ProcessInfo>,
        destination_ip: Option<std::net::IpAddr>,
    ) -> Result<RouteAction> {
        match self.overrides.read().unwrap().mode {
            astra_ipc::runtime::Mode::Direct => return Ok(RouteAction::Direct),
            astra_ipc::runtime::Mode::Global => return Ok(RouteAction::Proxy("PROXY".into())),
            astra_ipc::runtime::Mode::Rule => {}
        }
        if let Some(action) = &self.standalone_action {
            return Ok(action.clone());
        }
        let client = self.rule_client.as_ref().context("rule client missing")?;
        let revision = client.revision();
        let key = CacheKey {
            destination_ip,
            revision,
            target: target.clone(),
            network,
            inbound,
            process: process.clone(),
        };
        if revision != 0 && self.config.decision_cache_ttl_secs > 0 {
            if let Some(action) = self.cache.get(&key) {
                return Ok(action);
            }
        }
        let req = RouteRequest {
            destination_ip,
            api_version: API_VERSION,
            id: 0,
            target: target.clone(),
            network,
            inbound,
            process,
        };
        let response = client.decide(req).await?;
        // Do not cache a decision across a concurrent rule generation switch.
        if revision != 0 && response.revision == revision && self.config.decision_cache_ttl_secs > 0
        {
            self.cache.insert(key, response.action.clone());
        }
        let decision = response.action;
        Ok(decision)
    }

    pub(crate) async fn connect_tracked(
        &self,
        target: &Target,
        action: &RouteAction,
        inbound: Inbound,
        source: Option<std::net::SocketAddr>,
    ) -> Result<outbound::Stream> {
        let restored = self.restore_fake_target(target)?;
        let target = &restored;
        let (resolved, lease) = self.resolve_session(action).await?;
        let traffic = proxy::traffic::Traffic::new(
            &self.metrics,
            source,
            target,
            inbound,
            Network::Tcp,
            &resolved,
        );
        let connected = tokio::select! {
            biased;
            _ = lease.cancelled() => Err(anyhow::anyhow!("proxy group switched during connection")),
            _ = proxy::traffic::Traffic::cancelled(&traffic) => Err(anyhow::anyhow!("connection closed by user")),
            result = self.connect_resolved_on(target, &resolved, lease.1.as_deref()) => result,
        };
        match connected {
            Ok(inner) => {
                let inner = lease.wrap(inner);
                if let Some(traffic) = &traffic {
                    traffic.connected();
                } else {
                    return Ok(inner);
                }
                Ok(Box::new(proxy::traffic::Stream::new(inner, traffic)))
            }
            Err(error) => {
                if let Some(traffic) = &traffic {
                    traffic.failed(&error);
                }
                Err(error)
            }
        }
    }

    pub async fn connect(&self, target: &Target, action: &RouteAction) -> Result<outbound::Stream> {
        let restored = self.restore_fake_target(target)?;
        let target = &restored;
        let (resolved, lease) = self.resolve_session(action).await?;
        let inner = tokio::select! {
            biased;
            _ = lease.cancelled() => bail!("proxy group switched during connection"),
            result = self.connect_resolved_on(target, &resolved, lease.1.as_deref()) => result?,
        };
        Ok(lease.wrap(inner))
    }
    pub(crate) async fn resolve_session(
        &self,
        action: &RouteAction,
    ) -> Result<(RouteAction, proxy::group_sessions::Lease)> {
        timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            self.groups.resolve_session(self, action),
        )
        .await
        .context("proxy group selection timed out")?
    }
    pub(crate) async fn resolve_action(&self, action: &RouteAction) -> Result<RouteAction> {
        timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            self.groups.resolve(self, action),
        )
        .await
        .context("proxy group selection timed out; health checks are still running")?
    }
    async fn connect_resolved(
        &self,
        target: &Target,
        action: &RouteAction,
    ) -> Result<outbound::Stream> {
        self.connect_resolved_on(target, action, None).await
    }
    fn native_for(&self, interface: Option<&str>) -> &native::Registry {
        interface.and_then(|name| self.native_interfaces.get(name)).unwrap_or(&self.native)
    }
    async fn connect_resolved_on(
        &self,
        target: &Target,
        action: &RouteAction,
        interface: Option<&str>,
    ) -> Result<outbound::Stream> {
        let interface = interface.or_else(|| self.config.tun.as_ref().and_then(|tun| tun.interface.as_deref()));
        meow_common::outbound_iface::with_outbound_interface(
            interface, self.connect_resolved_inner(target, action, interface),
        ).await?
    }
    async fn connect_resolved_inner(
        &self,
        target: &Target,
        action: &RouteAction,
        interface: Option<&str>,
    ) -> Result<outbound::Stream> {
        if *action == RouteAction::Direct {
            if interface.is_some() || self.config.tun.is_some() || self.dns.is_some() {
                return timeout(
                    Duration::from_secs(self.config.connect_timeout_secs),
                    async {
                        let mut last = None;
                        for address in self.lookup_real(&target.host, target.port).await? {
                            let literal = Target {
                                host: address.ip().to_string(),
                                port: address.port(),
                            };
                            match egress::connect(&literal, interface).await {
                                Ok(stream) => return Ok(stream),
                                Err(error) => last = Some(error),
                            }
                        }
                        Err(last.unwrap_or_else(|| {
                            anyhow::anyhow!("direct DNS lookup returned no addresses")
                        }))
                    },
                )
                .await
                .context("direct egress timeout")?
                .map(|stream| Box::new(stream) as outbound::Stream);
            }
        }
        if let RouteAction::Proxy(tag) = action {
            if let Some(adapter) = self.native_for(interface).get(tag) {
                return timeout(
                    Duration::from_secs(self.config.connect_timeout_secs),
                    native::connect(adapter, target),
                )
                .await
                .context("outbound connection timeout")?;
            }
        }
        let selected = match action {
            RouteAction::Direct => None,
            RouteAction::Block => bail!("blocked"),
            RouteAction::Proxy(tag) => Some(
                self.outbounds
                    .get(tag)
                    .with_context(|| format!("undefined outbound tag {tag}"))?,
            ),
        };
        timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            outbound::open(target, selected),
        )
        .await
        .context("outbound connection timeout")?
    }
}

impl App {
    fn load(path: &std::path::Path) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let source_text = std::fs::read_to_string(&path)?;
        Self::from_text(&path, source_text)
    }
    fn from_text(path: &std::path::Path, source_text: String) -> Result<Self> {
        let mut config: ProxyConfig = toml::from_str(&source_text)?;
        if let (Some(dns), Some(tun)) = (&config.dns, &mut config.tun) {
            if dns.enable && dns.fake_ip {
                let range: ipnet::Ipv4Net = dns.fake_ip_range.parse()?;
                let cidr = range.trunc().to_string();
                if !tun.routes.contains(&cidr) {
                    tun.routes.push(cidr);
                }
                if let Some(range) = &dns.fake_ip_range6 {
                    let range: ipnet::Ipv6Net = range.parse()?;
                    anyhow::ensure!(
                        tun.ipv6_address.is_some(),
                        "IPv6 Fake IP with TUN requires tun.ipv6_address"
                    );
                    let cidr = range.trunc().to_string();
                    if !tun.routes.contains(&cidr) {
                        tun.routes.push(cidr);
                    }
                }
            }
        }
        if !config.subscription_sources().is_empty() {
            let mut document: toml::Table = toml::from_str(&source_text)?;
            astra_convert::subscription::prepare_groups(&mut document)?;
            config.proxy_groups = document["proxy_groups"].clone().try_into()?;
        }
        let tags = config
            .outbounds
            .iter()
            .map(|o| o.tag().to_owned())
            .collect();
        astra_ipc::groups::ensure_default(&mut config.proxy_groups, &tags)
            .map_err(anyhow::Error::msg)?;
        config.proxy_groups =
            astra_ipc::groups::expand(&config.proxy_groups, &tags).map_err(anyhow::Error::msg)?;
        let outbounds = config.validate()?;
        let native = native::compile(
            &config.outbounds,
            path.parent().context("configuration has no parent")?,
        )?;
        // Multiplexed QUIC/TLS clients must never share a socket across bindings.
        let mut native_interfaces = HashMap::new();
        for interface in config.proxy_groups.values().filter_map(|group| group.interface.as_ref()) {
            if !native_interfaces.contains_key(interface) {
                native_interfaces.insert(interface.clone(), native::compile(
                    &config.outbounds, path.parent().context("configuration has no parent")?,
                )?);
            }
        }
        let mut builder = Cache::builder().max_capacity(65_536);
        if config.decision_cache_ttl_secs > 0 {
            builder = builder.time_to_live(Duration::from_secs(config.decision_cache_ttl_secs));
        }
        let mut dns = config
            .dns
            .clone()
            .filter(|dns| dns.enable)
            .map(astra_dns::Resolver::new)
            .transpose()?;
        if let Some(dns) = &mut dns {
            if let Some(tun) = &config.tun {
                let subnet = ipnet::Ipv4Net::new(tun.address, tun.prefix)?;
                for address in [tun.address, subnet.network(), subnet.broadcast()] {
                    dns.reserve_address(address.into());
                }
                if let Some(address) = tun.ipv6_address {
                    let subnet = ipnet::Ipv6Net::new(address, tun.ipv6_prefix)?;
                    dns.reserve_address(address.into());
                    dns.reserve_address(subnet.network().into());
                }
            }
            for node in &config.outbounds {
                let server = match node {
                    OutboundConfig::Native { config, .. } => {
                        dns.exclude_domain(&config.server);
                        continue;
                    }
                    OutboundConfig::Socks5 { server, .. }
                    | OutboundConfig::Http { server, .. }
                    | OutboundConfig::TrojanTls { server, .. }
                    | OutboundConfig::VlessTls { server, .. } => server,
                };
                if let Ok(uri) = url::Url::parse(&format!("tcp://{server}")) {
                    if let Some(host) = uri.host_str() {
                        dns.exclude_domain(host);
                    }
                }
            }
            for subscription in config.subscription_sources().values() {
                if let Ok(uri) = url::Url::parse(&subscription.url) {
                    if let Some(host) = uri.host_str() {
                        dns.exclude_domain(host);
                    }
                }
            }
        }
        Ok(Self {
            metrics: Arc::default(),
            overrides: Arc::default(),
            egress_signal: Arc::new(tokio::sync::watch::channel(false).0),
            dns,
            groups: proxy::groups::Groups::new(&config)?,
            standalone_action: config.standalone_tag()?.map(RouteAction::Proxy),
            rule_client: if config.standalone || config.dns_only() {
                None
            } else {
                Some(rule_client::RuleClient::new(
                    config.rule_socket.clone(),
                    config.ipc_connections,
                    config.decision_timeout_ms,
                ))
            },
            udp_permits: Arc::new(limit::Limit::new(config.max_udp_flows)),
            permits: Arc::new(limit::Limit::new(config.max_connections)),
            cache: builder.build(),
            route_leases: vec![],
            source_text,
            config,
            outbounds,
            native,
            native_interfaces,
        })
    }
}

pub async fn run() -> Result<()> {
    let elevated_config = platform::privilege::bootstrap()?;
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let first = elevated_config.or(first);
    if matches!(
        first.as_deref(),
        Some(
            "--diagnose"
                | "--diagnose-stdin"
                | "--diagnose-request"
                | "--diagnose-stream"
                | "--refresh-subscriptions-stdin"
        )
    ) {
        let path = std::path::absolute(args.next().context("expected config path")?)?;
        let mut request = None;
        let app = if matches!(
            first.as_deref(),
            Some(
                "--diagnose-stdin"
                    | "--diagnose-request"
                    | "--diagnose-stream"
                    | "--refresh-subscriptions-stdin"
            )
        ) {
            use std::io::Read;
            let mut input = String::new();
            std::io::stdin()
                .take(2 * 1024 * 1024 + 1)
                .read_to_string(&mut input)?;
            anyhow::ensure!(
                input.len() <= 2 * 1024 * 1024,
                "Diagnostic request exceeds limit"
            );
            let text = if matches!(
                first.as_deref(),
                Some("--diagnose-request" | "--diagnose-stream")
            ) {
                let payload: astra_ipc::diagnostics::Request = serde_json::from_str(&input)?;
                let text = payload.config.clone();
                request = Some(payload);
                text
            } else {
                input
            };
            anyhow::ensure!(text.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
            App::from_text(&path, text)?
        } else {
            App::load(&path)?
        };
        if first.as_deref() == Some("--refresh-subscriptions-stdin") {
            println!("{}", subscription::refresh_once(&path, app).await?);
            return Ok(());
        }
        let app = subscription::cached_app(&path, app);
        if first.as_deref() == Some("--diagnose-stream") {
            use std::io::Write;
            let tags = request
                .and_then(|r| r.targets)
                .unwrap_or_else(|| diagnostics::targets(&app));
            diagnostics::selected_stream(&app, &tags, |event| {
                let mut stdout = std::io::stdout().lock();
                serde_json::to_writer(&mut stdout, &event)?;
                writeln!(stdout)?;
                stdout.flush()?;
                Ok(())
            })
            .await?;
            return Ok(());
        }
        let output = if let Some(request) = request {
            if request.list_targets {
                serde_json::to_string(&diagnostics::targets(&app))?
            } else if let Some(targets) = request.targets {
                serde_json::to_string(&diagnostics::selected(&app, &targets).await?)?
            } else {
                serde_json::to_string(&diagnostics::run(&app).await)?
            }
        } else {
            serde_json::to_string(&diagnostics::run(&app).await)?
        };
        println!("{output}");
        return Ok(());
    }
    astra_ipc::logging::init()?;
    if first.as_deref() == Some("--runtime") {
        use astra_ipc::runtime::{Command, Mode};
        let socket = args
            .next()
            .context("--runtime requires a core management socket")?;
        let command = match args.next().as_deref().unwrap_or("snapshot") {
            "snapshot" => Command::Snapshot,
            "mode" => Command::SetMode(match args.next().as_deref() {
                Some("rule") => Mode::Rule,
                Some("global") => Mode::Global,
                Some("direct") => Mode::Direct,
                _ => bail!("mode requires rule, global or direct"),
            }),
            "select" => Command::Select {
                group: args.next().context("select requires group")?,
                member: args.next().context("select requires member")?,
            },
            "test" => Command::TestGroup(args.next().context("test requires group")?),
            "close-all" => Command::Disconnect(None),
            "close" => Command::Disconnect(Some(
                args.next()
                    .context("close requires connection ID")?
                    .parse()?,
            )),
            "system-proxy" => Command::SystemProxy(match args.next().as_deref() {
                Some("on") => true,
                Some("off") => false,
                _ => bail!("system-proxy requires on or off"),
            }),
            "tun" => Command::Tun(match args.next().as_deref() {
                Some("on") => true,
                Some("off") => false,
                _ => bail!("tun requires on or off"),
            }),
            "allow-lan" => Command::AllowLan(match args.next().as_deref() {
                Some("on") => true,
                Some("off") => false,
                _ => bail!("allow-lan requires on or off"),
            }),
            "reset" => Command::Reset,
            _ => bail!("unknown runtime command"),
        };
        anyhow::ensure!(args.next().is_none(), "unexpected runtime argument");
        println!(
            "{}",
            serde_json::to_string_pretty(
                &astra_ipc::control::runtime(std::path::Path::new(&socket), command).await?
            )?
        );
        return Ok(());
    }
    if first.as_deref() == Some("--configure-egress") {
        let path = std::fs::canonicalize(args.next().context("expected config path")?)?;
        let mut app = App::load(&path)?;
        let tun = app
            .config
            .tun
            .as_mut()
            .context("configuration has no TUN")?;
        egress::prepare_interface(tun)?;
        egress::cache_interface(&path, tun)?;
        println!("TUN physical egress: {}", tun.interface.as_deref().unwrap());
        return Ok(());
    }
    if first.as_deref() == Some("--stop") {
        let socket = args.next().context("expected management socket")?;
        astra_ipc::control::request(
            std::path::Path::new(&socket),
            astra_ipc::control::Command::Stop,
        )
        .await?;
        return Ok(());
    }
    if first.as_deref() == Some("--restore-system-proxy") {
        system_proxy::recover(std::path::Path::new(
            &args.next().context("expected recovery file path")?,
        ))?;
        return Ok(());
    }
    if first.as_deref() == Some("--update-subscriptions") {
        let socket = args.next().context("expected management socket")?;
        astra_ipc::control::request(
            std::path::Path::new(&socket),
            astra_ipc::control::Command::UpdateSubscriptions,
        )
        .await?;
        println!("Subscription update scheduled");
        return Ok(());
    }
    if matches!(first.as_deref(), Some("--reload" | "--switch")) {
        let socket = args.next().context("expected management socket")?;
        let path = if first.as_deref() == Some("--switch") {
            Some(std::fs::canonicalize(
                args.next().context("expected config path")?,
            )?)
        } else {
            None
        };
        astra_ipc::control::request(
            std::path::Path::new(&socket),
            astra_ipc::control::Command::Reload { path },
        )
        .await?;
        return Ok(());
    }
    if first.as_deref() == Some("--check-stdin") {
        use std::io::Read;
        let path = args
            .next()
            .context("--check-stdin requires the configuration path")?;
        let mut text = String::new();
        std::io::stdin()
            .take(480 * 1024 + 1)
            .read_to_string(&mut text)?;
        anyhow::ensure!(text.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
        App::from_text(std::path::Path::new(&path), text)?;
        return Ok(());
    }
    let check = first.as_deref() == Some("--check");
    let mode = first.as_deref() == Some("--print-routing-mode");
    let path = if check || mode { args.next() } else { first }
        .unwrap_or_else(|| "configs/proxy.toml".to_owned());
    let app = App::load(std::path::Path::new(&path))?;
    if mode {
        println!(
            "{}",
            if app.config.standalone {
                "standalone"
            } else {
                "rules"
            }
        );
        return Ok(());
    }
    if check {
        return Ok(());
    }
    if platform::privilege::delegate(
        std::path::Path::new(&path),
        astra_ipc::runtime::load_overrides(std::path::Path::new(&path))?.tun == Some(true),
    )
    .await?
    {
        return Ok(());
    }
    runtime::run(std::fs::canonicalize(path)?, app).await
}

#[cfg(test)]
mod recapture_tests {
    use super::*;

    #[tokio::test]
    async fn tun_direct_keeps_loopback_and_rejects_unscoped_link_local() {
        let app = App::from_text(
            std::path::Path::new("tun-direct.toml"),
            "rule_socket='unused.sock'\nsocks5_listen='127.0.0.1:1080'\n[tun]\nname='test-only'\n"
                .into(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        };
        let _client = app
            .connect_resolved(&target, &RouteAction::Direct)
            .await
            .unwrap();
        let _server = listener.accept().await.unwrap();
        let link_local = Target {
            host: "fe80::123".into(),
            port: 53,
        };
        let result = app
            .connect_resolved(&link_local, &RouteAction::Direct)
            .await;
        match result {
            Err(error) => assert!(error.to_string().contains("tun.interface"), "{error:#}"),
            Ok(_) => panic!("unscoped link-local must be rejected before dialing"),
        }
    }

    #[tokio::test]
    async fn tun_rejects_recaptured_proxy_connections_even_in_standalone_mode() {
        let app = App::from_text(
            std::path::Path::new("recapture.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='127.0.0.1:1081'\n".into(),
        ).unwrap();
        let target = Target {
            host: "fe80::123".into(),
            port: 53,
        };
        let own = ProcessInfo {
            pid: std::process::id(),
            name: "astra-proxy.exe".into(),
            path: "astra-proxy.exe".into(),
        };
        for network in [Network::Tcp, Network::Udp] {
            let result = app
                .route_with_process(&target, Inbound::Tun, network, Some(own.clone()))
                .await;
            assert!(
                result.is_err(),
                "recaptured {network:?} must not open another outbound"
            );
            assert!(result.unwrap_err().to_string().contains("recaptured"));
            let mut other = own.clone();
            other.pid = own.pid.wrapping_add(1);
            assert!(app
                .route_with_process(&target, Inbound::Tun, network, Some(other))
                .await
                .is_ok());
            // Explicit SOCKS use is not transparent recapture.
            assert!(app
                .route_with_process(&target, Inbound::Socks5, network, Some(own.clone()))
                .await
                .is_ok());
        }
        #[cfg(windows)]
        {
            // Exercise OS ownership lookup too: standalone TUN must not skip it.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let destination = listener.local_addr().unwrap();
            let client = tokio::net::TcpStream::connect(destination).await.unwrap();
            let (_server, _) = listener.accept().await.unwrap();
            let result = app
                .route_connection(
                    &target,
                    Inbound::Tun,
                    Network::Tcp,
                    Some(client.local_addr().unwrap()),
                    Some(destination),
                )
                .await;
            assert!(result.unwrap_err().to_string().contains("recaptured"));
        }
    }
}
