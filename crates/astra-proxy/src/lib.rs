pub mod platform;
pub use platform::entry;
pub use proxy::runtime::run_with_lifeline;
/// Set by the sudo hand-off before the runtime starts; the elevated TUN mode
/// lives only in this process and is never written back to the overrides file.
pub(crate) static ELEVATED_TUN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
mod proxy;
mod rules;
use platform::system_proxy;
mod config;
use platform::egress;
mod inbound;
pub use astra_protocols::native;
pub use astra_protocols::outbound;
pub use astra_protocols::tuic;
use inbound::packet;
use platform::mode_config;
use platform::transparent;
use platform::tun;
use proxy::limit;
use proxy::runtime;
mod diagnostics;
mod dns;
pub mod hosted;
mod nettool;
mod subscription;
pub use astra_protocols::xhttp;
use proxy::udp;

use anyhow::{bail, Context, Result};
use config::{OutboundConfig, ProxyConfig, ProxyConfigExt};
use std::{collections::HashMap, sync::Arc};

/// Established flows retain only this signal, not the configuration generation.
#[derive(Clone)]
pub(crate) struct EgressSignal(Arc<tokio::sync::watch::Sender<bool>>);
impl EgressSignal {
    pub(crate) async fn cancelled(&self) {
        let mut changed = self.0.subscribe();
        let _ = changed.wait_for(|changed| *changed).await;
    }
    pub(crate) fn send_replace(&self, value: bool) {
        self.0.send_replace(value);
    }
}
impl Default for EgressSignal {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }
}

pub struct App {
    // Documents and decision caches belong to the active App, not to an
    // admitted UDP association which may need to route additional targets.
    source_text: String,
    effective_text: Option<String>,
    runtime_chains: std::collections::BTreeMap<String, Vec<String>>,
    cache: Option<rules::DecisionCache>,
    routing: Arc<RoutingContext>,
}

/// Routing resources shared by flows admitted under one configuration.
/// Kept separate so a SOCKS UDP association can preserve its routing semantics
/// without retaining old documents or the generation's decision cache.
#[doc(hidden)]
pub struct RoutingContext {
    hosted: Option<Arc<hosted::Host>>,
    worker_host: Option<hosted::WorkerHost>,
    metrics: Arc<proxy::traffic::Metrics>,
    overrides: Arc<arc_swap::ArcSwap<astra_model::runtime::Overrides>>,
    egress_signal: EgressSignal,
    dns: Option<astra_dns::Resolver>,
    /// Lookup-only resolver used when the DNS service is absent or disabled,
    /// so local hostname resolution stays encrypted unless explicitly bypassed.
    lookup_dns: Option<astra_dns::Resolver>,
    proxy_domains: std::collections::HashSet<String>,
    config: ProxyConfig,
    route_leases: Vec<Arc<tun::OwnedRoute>>,
    outbounds: HashMap<String, OutboundConfig>,
    groups: proxy::groups::Groups,
    rules: rules::Rules,
    chain_base: std::path::PathBuf,
    chain_cache: moka::sync::Cache<
        (Vec<String>, Option<String>, Vec<(String, u64)>),
        Arc<native::NativeOutbound>,
    >,
    native: native::Registry,
    native_interfaces: HashMap<String, native::Registry>,
    udp_permits: Arc<limit::Limit>,
    permits: Arc<limit::Limit>,
}

impl std::ops::Deref for App {
    type Target = RoutingContext;
    fn deref(&self) -> &Self::Target {
        &self.routing
    }
}
impl std::ops::DerefMut for App {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Routing resources are mutable only while preparing a candidate;
        // once published, App itself is behind Arc and mutations use locks.
        Arc::get_mut(&mut self.routing).expect("routing generation already published")
    }
}

impl App {
    fn routing_view(routing: Arc<RoutingContext>) -> Self {
        Self {
            source_text: String::new(),
            effective_text: None,
            runtime_chains: Default::default(),
            cache: None,
            routing,
        }
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
        let mut dns = config
            .dns
            .clone()
            .filter(|dns| dns.enable)
            .map(astra_dns::Resolver::new)
            .transpose()?;
        if let (Some(resolver), Some(tun)) = (&mut dns, &mut config.tun) {
            inbound::configure_tun(resolver, tun)?;
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
        astra_model::groups::ensure_default(&mut config.proxy_groups, &tags)
            .map_err(anyhow::Error::msg)?;
        config.proxy_groups =
            astra_model::groups::expand(&config.proxy_groups, &tags).map_err(anyhow::Error::msg)?;
        let outbounds = config.validate()?;
        let native = native::compile(
            &config.outbounds,
            path.parent().context("configuration has no parent")?,
        )?;
        // Multiplexed QUIC/TLS clients must never share a socket across bindings.
        let mut native_interfaces = HashMap::new();
        for interface in config
            .proxy_groups
            .values()
            .filter_map(|group| group.interface.as_ref())
        {
            if !native_interfaces.contains_key(interface) {
                native_interfaces.insert(
                    interface.clone(),
                    native::compile(
                        &config.outbounds,
                        path.parent().context("configuration has no parent")?,
                    )?,
                );
            }
        }
        let proxy_domains: std::collections::HashSet<String> = config
            .outbounds
            .iter()
            .filter_map(|node| {
                let server = match node {
                    OutboundConfig::Native { config, .. } => {
                        return Some(config.server.trim_end_matches('.').to_ascii_lowercase())
                    }
                    OutboundConfig::Socks5 { server, .. }
                    | OutboundConfig::Http { server, .. }
                    | OutboundConfig::TrojanTls { server, .. }
                    | OutboundConfig::VlessTls { server, .. } => server,
                };
                url::Url::parse(&format!("tcp://{server}"))
                    .ok()?
                    .host_str()
                    .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
            })
            .collect();
        if let Some(dns) = &mut dns {
            for domain in &proxy_domains {
                dns.exclude_domain(domain);
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
            runtime_chains: Default::default(),
            // Hosted workers consume the validated expansion; ordinary cores
            // only need the editable source text.
            effective_text: std::env::var_os("ASTRA_HOSTED_EXPERIMENTAL")
                .map(|_| source_text.clone()),
            source_text,
            cache: rules::decision_cache(config.decision_cache_ttl_secs),
            routing: Arc::new(RoutingContext {
                hosted: None,
                worker_host: None,
                metrics: Arc::default(),
                overrides: Arc::default(),
                egress_signal: EgressSignal::default(),
                lookup_dns: crate::dns::lookup_resolver(dns.is_some(), config.dns.as_ref())?,
                proxy_domains,
                dns,
                groups: proxy::groups::Groups::new(&config)?,
                rules: rules::Rules::new(&config)?,
                udp_permits: Arc::new(limit::Limit::new(config.max_udp_flows)),
                permits: Arc::new(limit::Limit::new(config.max_connections)),
                route_leases: vec![],
                config,
                outbounds,
                chain_base: path
                    .parent()
                    .context("configuration has no parent")?
                    .to_owned(),
                chain_cache: moka::sync::Cache::new(128),
                native,
                native_interfaces,
            }),
        })
    }
}

pub async fn run() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--host-worker") {
        let _log = astra_platform::logging::init()?;
        return hosted::worker().await;
    }
    let elevated_config = platform::privilege::bootstrap()?;
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let first = elevated_config.or(first);
    if first.as_deref() == Some("--nettool") {
        return nettool::cli(args).await;
    }
    if first.as_deref() == Some("--network-diagnostics-stream") {
        use std::io::{Read, Write};
        // Keep the command shape compatible with isolated diagnostic launchers.
        let _path = args.next().context("expected config path")?;
        let mut input = String::new();
        std::io::stdin()
            .take(512 * 1024 + 1)
            .read_to_string(&mut input)?;
        anyhow::ensure!(
            input.len() <= 512 * 1024,
            "Network diagnostic request exceeds limit"
        );
        let request: astra_model::nettool::DetailedRequest = serde_json::from_str(&input)?;
        anyhow::ensure!(
            request.config.is_empty(),
            "Detailed diagnostics do not initialize a profile"
        );
        nettool::run_detailed_context(&request, |event| {
            let mut output = std::io::stdout().lock();
            serde_json::to_writer(&mut output, &event)?;
            writeln!(output)?;
            output.flush()?;
            Ok(())
        })
        .await?;
        return Ok(());
    }
    if first.as_deref() == Some("--nettool-stream") {
        use std::io::{Read, Write};
        let path = std::path::absolute(args.next().context("expected config path")?)?;
        let mut input = String::new();
        std::io::stdin()
            .take(512 * 1024 + 1)
            .read_to_string(&mut input)?;
        anyhow::ensure!(
            input.len() <= 512 * 1024,
            "Network tool request exceeds limit"
        );
        let request: astra_model::nettool::Request = serde_json::from_str(&input)?;
        // Host-path probes must not initialize unrelated profile state, even
        // when an older caller still supplies a complete configuration.
        let app = if !matches!(request.egress, astra_model::nettool::Egress::Outbound(_))
            || request.config.is_empty()
        {
            None
        } else {
            anyhow::ensure!(
                request.config.len() <= 480 * 1024,
                "Configuration exceeds 480 KiB"
            );
            Some(subscription::cached_app(
                &path,
                App::from_text(&path, request.config.clone())?,
            ))
        };
        nettool::run(app.as_ref(), &request, false, |event| {
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer(&mut stdout, &event)?;
            writeln!(stdout)?;
            stdout.flush()?;
            Ok(())
        })
        .await?;
        return Ok(());
    }
    if first.as_deref() == Some("--probe-serve") {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let path = std::path::absolute(args.next().context("expected config path")?)?;
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        (&mut stdin)
            .take(2 * 1024 * 1024 + 1)
            .read_line(&mut line)
            .await?;
        anyhow::ensure!(line.ends_with('\n'), "Probe configuration exceeds limit");
        let payload: astra_model::diagnostics::ProbeServe = serde_json::from_str(&line)?;
        anyhow::ensure!(
            payload.config.len() <= 480 * 1024,
            "Configuration exceeds 480 KiB"
        );
        let app = subscription::cached_app(&path, App::from_text(&path, payload.config)?);
        diagnostics::serve(&app, stdin, tokio::io::stdout()).await?;
        return Ok(());
    }
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
                let payload: astra_model::diagnostics::Request = serde_json::from_str(&input)?;
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
            // stdout is the management reply; diagnostics belong on stderr.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .try_init()
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
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
    let _log = astra_platform::logging::init()?;
    if first.as_deref() == Some("--runtime") {
        use astra_model::runtime::Command;
        use astra_model::runtime::Mode;
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
            "chain" => Command::SetChain {
                group: args
                    .next()
                    .context("chain requires a group and ordered hops")?,
                members: Some(args.by_ref().collect()),
            },
            "chain-clear" => Command::SetChain {
                group: args.next().context("chain-clear requires a group")?,
                members: None,
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
            astra_model::control::Command::Stop,
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
            astra_model::control::Command::UpdateSubscriptions,
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
            astra_model::control::Command::Reload { path },
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
        astra_platform::runtime::load_overrides(std::path::Path::new(&path))?.tun == Some(true),
    )
    .await?
    {
        return Ok(());
    }
    runtime::run(std::fs::canonicalize(path)?, app).await
}

#[cfg(test)]
#[path = "test/lib_recapture_test.rs"]
mod recapture_tests;

#[cfg(test)]
#[path = "test/chain_test.rs"]
mod chain_tests;
