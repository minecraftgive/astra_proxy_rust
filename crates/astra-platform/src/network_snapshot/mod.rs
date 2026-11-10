//! Bounded, read-only OS sampling. No active external probes or resolver queries.
use astra_model::network_state::*;
use std::{
    io,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::Mutex};
#[cfg(any(target_os = "linux", all(test, unix)))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod parsers;
#[cfg(unix)]
mod unix;
#[cfg(any(windows, test))]
mod windows;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;
#[cfg(windows)]
use windows as os;

const OUTPUT_LIMIT: usize = 512 * 1024;
const MAX_INTERFACES: usize = 128;
const MAX_ROUTES: usize = 512;
const MAX_RESOLVERS: usize = 256;

/// Unique literal DNS server addresses configured on this host right now
/// (scoped resolvers included, zone suffixes stripped). Read-only.
pub async fn dns_servers() -> Vec<std::net::IpAddr> {
    let (resolvers, _) = os::dns().await;
    server_addresses(&resolvers)
}
pub fn server_addresses(resolvers: &[Resolver]) -> Vec<std::net::IpAddr> {
    let mut found = Vec::new();
    for raw in resolvers.iter().flat_map(|resolver| &resolver.servers) {
        let raw = raw.trim().trim_start_matches('[');
        let raw = raw.split(['%', ']']).next().unwrap_or_default();
        if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
            if !found.contains(&ip) {
                found.push(ip);
            }
        }
    }
    found
}

#[derive(Clone)]
struct DnsSample {
    sampled_at_ms: String,
    resolvers: Vec<Resolver>,
    issues: Vec<Issue>,
    truncated: bool,
}
#[derive(Default)]
pub struct Sampler {
    dns: Mutex<Option<(Instant, DnsSample)>>,
    snapshot: Mutex<Option<(Instant, Snapshot)>>,
}
impl Sampler {
    pub async fn sample(&self, agent_id: &str) -> Snapshot {
        // Coalesce simultaneous frontend refreshes; DNS has its longer cache below.
        let mut cached = self.snapshot.lock().await;
        if let Some((at, snapshot)) = cached.as_ref() {
            if at.elapsed() < Duration::from_secs(2) && snapshot.agent_id == agent_id {
                return snapshot.clone();
            }
        }
        let (mut interfaces, mut routes, mut issues) = os::network().await;
        let dns = {
            let mut cache = self.dns.lock().await;
            if cache
                .as_ref()
                .is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(30))
            {
                let (resolvers, issues) = os::dns().await;
                let mut sample = DnsSample {
                    sampled_at_ms: now(),
                    resolvers,
                    issues,
                    truncated: false,
                };
                sample.truncated = sample.resolvers.len() > MAX_RESOLVERS;
                sample.resolvers.truncate(MAX_RESOLVERS);
                for resolver in &mut sample.resolvers {
                    sample.truncated |= resolver.domains.len() > 64 || resolver.servers.len() > 64;
                    resolver.domains.truncate(64);
                    resolver.servers.truncate(64);
                }
                *cache = Some((Instant::now(), sample));
            }
            cache.as_ref().unwrap().1.clone()
        };
        let mut truncated =
            dns.truncated || interfaces.len() > MAX_INTERFACES || routes.len() > MAX_ROUTES;
        interfaces.truncate(MAX_INTERFACES);
        routes.truncate(MAX_ROUTES);
        for interface in &mut interfaces {
            truncated |= interface.addresses.len() > 64;
            interface.addresses.truncate(64);
        }
        issues.extend(dns.issues);
        let snapshot = Snapshot {
            schema_version: 1,
            agent_id: agent_id.into(),
            sampled_at_ms: now(),
            dns_sampled_at_ms: dns.sampled_at_ms,
            interfaces,
            dns: dns.resolvers,
            routes,
            issues,
            truncated,
        };
        *cached = Some((Instant::now(), snapshot.clone()));
        snapshot
    }
}
fn now() -> String {
    chrono::Utc::now().timestamp_millis().to_string()
}
fn issue(component: &str, error: impl std::fmt::Display) -> Issue {
    Issue {
        component: component.into(),
        message: error.to_string().chars().take(1024).collect(),
    }
}

/// All collectors invoke fixed program names and argument lists, never a shell.
async fn command(program: &str, args: &[&str]) -> io::Result<String> {
    let mut child = crate::platform::background_core(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let read = async {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut stdout = stdout.take(OUTPUT_LIMIT as u64 + 1);
        let mut stderr = stderr.take(4097);
        let (out, err) = tokio::join!(
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut errors)
        );
        out?;
        err?;
        if output.len() > OUTPUT_LIMIT || errors.len() > 4096 {
            return Err(io::Error::other(
                "system query exceeded output limit; data unavailable, not complete",
            ));
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "system query failed: {}",
                String::from_utf8_lossy(&errors).trim()
            )));
        }
        String::from_utf8(output).map_err(io::Error::other)
    };
    let result = tokio::time::timeout(Duration::from_secs(3), read)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "system query timed out",
            ))
        });
    if result.is_err() {
        let _ = child.start_kill();
        child.wait().await?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_sample_is_bounded_and_dns_cache_keeps_its_sample_time() {
        let sampler = Sampler::default();
        let first = sampler.sample("fixture-agent").await;
        let second = sampler.sample("fixture-agent").await;
        assert_eq!(first.agent_id, "fixture-agent");
        assert_eq!(first.dns_sampled_at_ms, second.dns_sampled_at_ms);
        assert!(first.interfaces.len() <= MAX_INTERFACES && first.routes.len() <= MAX_ROUTES);
        assert!(!first.interfaces.is_empty(), "{:?}", first.issues);
    }
}
