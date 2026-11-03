use astra_model::{Inbound, Network, RouteRequest, Target};
use astra_rule::engine::{RuleConfig, RuleEngine, RuleSpec};
use astra_rule_data::geoip::{Cidr, Database, GeoIp, GeoIpList};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use prost::Message;

fn matching(c: &mut Criterion) {
    for kind in ["domain_keyword", "domain_pattern"] {
        let mut config: RuleConfig =
            toml::from_str("socket='unused'\ndirect_local=false\ndefault_action='direct'").unwrap();
        config.rules = (0..10_000)
            .map(|i| RuleSpec {
                kind: kind.into(),
                value: if kind == "domain_keyword" {
                    format!("needle{i:05}")
                } else {
                    format!("*.item{i:05}.test")
                },
                action: "block".into(),
            })
            .collect();
        let engine = RuleEngine::compile_at(&config, std::path::Path::new(".")).unwrap();
        let req = RouteRequest {
            api_version: astra_ipc::API_VERSION,
            id: 1,
            target: Target {
                host: "www.needle09999.item09999.test".into(),
                port: 443,
            },
            destination_ip: None,
            network: Network::Tcp,
            inbound: Inbound::Socks5,
            process: None,
        };
        c.bench_function(kind, |b| {
            b.iter(|| black_box(engine.decision(black_box(&req))))
        });
    }
    let list = GeoIpList {
        entry: vec![GeoIp {
            country_code: "test".into(),
            inverse_match: false,
            cidr: (0..50_000u32)
                .map(|i| Cidr {
                    ip: (0x0a000000u32 + i * 32).to_be_bytes().to_vec(),
                    prefix: 28 + i % 5,
                })
                .collect(),
        }],
    };
    let db = Database::decode(&list.encode_to_vec()).unwrap();
    let group = db.group("test").unwrap();
    let hit = "10.24.105.224".parse().unwrap();
    let miss = "203.0.113.7".parse().unwrap();
    c.bench_function("geoip_50000_hit", |b| {
        b.iter(|| black_box(group.contains(black_box(hit))))
    });
    c.bench_function("geoip_50000_miss", |b| {
        b.iter(|| black_box(group.contains(black_box(miss))))
    });
}
criterion_group! { name = benches; config = Criterion::default().sample_size(20).measurement_time(std::time::Duration::from_secs(2)).warm_up_time(std::time::Duration::from_secs(1)); targets = matching }
criterion_main!(benches);
