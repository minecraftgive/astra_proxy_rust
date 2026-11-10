//! Synthetic, in-process workloads. Same counted system allocator on both sides.
mod network;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
struct Allocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn added(n: usize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(live, Ordering::Relaxed);
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            added(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(layout);
        if !p.is_null() {
            added(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        System.dealloc(p, layout);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let q = System.realloc(p, layout, size);
        if !q.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            added(size);
        }
        q
    }
}
#[global_allocator]
static ALLOC: Allocator = Allocator;
fn report(scenario: &str, started: Instant, mut samples: Vec<f64>) {
    let elapsed = started.elapsed().as_secs_f64();
    let live = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    samples.sort_by(f64::total_cmp);
    let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100).saturating_sub(1)];
    println!(
        "{}",
        serde_json::json!({"scenario":scenario,"seconds":elapsed,"operations":samples.len(),
        "operations_per_second":samples.len() as f64/elapsed,"p95_us":percentile(95),"p99_us":percentile(99),
        "live_allocated_bytes":live,"peak_allocated_bytes":peak})
    );
    std::thread::sleep(std::time::Duration::from_millis(250));
}
fn rules() {
    use astra_model::{Inbound, Network, RouteRequest, Target};
    use astra_rule::engine::{RuleConfig, RuleEngine, RuleSpec};
    let mut config: RuleConfig =
        toml::from_str("socket='unused'\ndefault_action='direct'\ndirect_local=false").unwrap();
    config.rules = (0..2000)
        .map(|i| RuleSpec {
            kind: "domain_regex".into(),
            value: format!("^host{i}\\.example\\.test$"),
            action: "block".into(),
        })
        .collect();
    let engine = RuleEngine::compile_at(&config, std::path::Path::new(".")).unwrap();
    drop(config);
    let started = Instant::now();
    let mut samples = Vec::new();
    for i in 0..10000 {
        let request = RouteRequest {
            api_version: 1,
            id: 1,
            target: Target {
                host: format!("host{}.example.test", i % 2500),
                port: 443,
            },
            destination_ip: None,
            network: Network::Tcp,
            inbound: Inbound::Socks5,
            process: None,
        };
        let now = Instant::now();
        black_box(engine.decision(&request));
        samples.push(now.elapsed().as_secs_f64() * 1e6);
    }
    report("regex_2000", started, samples);
    black_box(&engine);
}
fn logs() {
    use astra_client::logs::{Settings, Viewer};
    let mut viewer = Viewer::new(
        "benchmark".into(),
        Settings {
            memory_lines: 10000,
            ..Default::default()
        },
    );
    for i in 0..8000 {
        viewer.push(format!("INFO {i:05} {}", "abcdefgh ".repeat(25)));
    }
    viewer.toggle_pause();
    #[cfg(not(feature = "legacy"))]
    let mut cache = astra_client::logs::PresentationCache::default();
    let started = Instant::now();
    let mut samples = Vec::new();
    for _ in 0..500 {
        let now = Instant::now();
        #[cfg(feature = "legacy")]
        {
            let rows =
                std::sync::Arc::new(viewer.visible().into_iter().cloned().collect::<Vec<_>>());
            black_box(rows.iter().map(|r| r.text.chars().count()).max());
            black_box(rows);
        }
        #[cfg(not(feature = "legacy"))]
        {
            cache.update(&viewer);
            black_box(cache.rows.clone());
            black_box(cache.width_chars);
        }
        samples.push(now.elapsed().as_secs_f64() * 1e6);
    }
    report("logs_8000_paused_redraw", started, samples);
    black_box(&viewer);
}
struct Upstream;
impl astra_dns::Transport for Upstream {
    fn exchange<'a>(
        &'a self,
        _: &'a astra_dns::Upstream,
        query: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<u8>>> + Send + 'a>>
    {
        Box::pin(async move {
            use hickory_proto::{
                op::Message,
                rr::{rdata::A, RData, Record},
            };
            let q = Message::from_vec(&query)?;
            let mut answer =
                astra_dns::wire::response(&q, hickory_proto::op::ResponseCode::NoError);
            for i in 1..=16 {
                answer.answers.push(Record::from_rdata(
                    q.queries[0].name().clone(),
                    60,
                    RData::A(A(std::net::Ipv4Addr::new(203, 0, 113, i))),
                ));
            }
            Ok(answer.to_vec()?)
        })
    }
}
async fn dns(fake: bool) {
    use hickory_proto::rr::RecordType;
    let resolver = astra_dns::Resolver::new(astra_dns::Config {
        fake_ip: fake,
        cache_capacity: 8192,
        mapping_capacity: 8192,
        ..Default::default()
    })
    .unwrap();
    let queries: Vec<_> = (0..8192)
        .map(|i| {
            astra_dns::wire::query(&format!("host{i}.example.test"), RecordType::A)
                .unwrap()
                .to_vec()
                .unwrap()
        })
        .collect();
    for query in &queries {
        resolver.answer(query, &Upstream).await.unwrap();
    }
    let started = Instant::now();
    let mut samples = Vec::new();
    for i in 0..16384 {
        let now = Instant::now();
        black_box(
            resolver
                .answer(&queries[i % queries.len()], &Upstream)
                .await
                .unwrap(),
        );
        samples.push(now.elapsed().as_secs_f64() * 1e6);
    }
    report(
        if fake {
            "fake_ip_8192"
        } else {
            "dns_cache_8192"
        },
        started,
        samples,
    );
    black_box(&resolver);
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("rules") => rules(),
        Some("logs") => logs(),
        Some("dns") => dns(false).await,
        Some("fake") => dns(true).await,
        Some("network") => network::run().await.unwrap(),
        _ => {
            panic!("rules|logs|dns|fake|network <proxy-port> <concurrency> <connections> <seconds>")
        }
    }
}
