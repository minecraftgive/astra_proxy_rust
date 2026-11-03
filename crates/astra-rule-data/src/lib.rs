//! Shared rule-provider format and bounded resource loading.
pub mod geoip;
pub mod geosite;
use anyhow::Result;
pub use astra_model::config::ruleset::{compile_domain_regex, parse_classical, MatchSpec, RuleSet};
use std::path::Path;
#[cfg(test)]
use std::{
    fs,
    io::{Read, Write},
    time::Duration,
};
pub trait RuleSetExt {
    fn load(&self, base: &Path) -> Result<Vec<MatchSpec>>;
    fn load_with(&self, base: &Path, loader: &ResourceLoader) -> Result<Vec<MatchSpec>>;
}
impl RuleSetExt for RuleSet {
    fn load(&self, base: &Path) -> Result<Vec<MatchSpec>> {
        self.load_with(base, &ResourceLoader::default())
    }
    fn load_with(&self, base: &Path, loader: &ResourceLoader) -> Result<Vec<MatchSpec>> {
        self.validate()?;
        if self.path.is_none() {
            return self.parse_entries(&self.payload);
        }
        let bytes = loader.load_resource(
            base,
            self.path.as_deref().unwrap(),
            self.url.as_deref(),
            self.interval,
            16 * 1024 * 1024,
            |data| self.parse(data).map(|_| ()),
        )?;
        self.parse(&bytes)
    }
}
pub use astra_resource::{
    load_resource, load_resource_with, resolve, validate_redirect, validate_remote_url, LoadPolicy,
    ResourceLoader,
};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immutable_generation_never_downloads_or_rewrites_shared_cache() {
        let dir = std::env::temp_dir().join(format!("astra-read-only-rule-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("resource");
        fs::write(&source, b"42").unwrap();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let url = format!("http://{}/resource", server.local_addr().unwrap());
        let decode = |bytes: &[u8]| -> Result<u32> { Ok(std::str::from_utf8(bytes)?.parse()?) };
        let loader = ResourceLoader::new(LoadPolicy::ReadOnly);
        let (_, number) = loader
            .load_resource_with(&dir, "resource", Some(&url), 0, 100, decode)
            .unwrap();
        assert_eq!(number, 42);
        assert_eq!(fs::read(&source).unwrap(), b"42");
        assert!(loader
            .load_resource_with(&dir, "missing", Some(&url), 0, 100, decode)
            .is_err());
        fs::write(&source, b"invalid").unwrap();
        assert!(loader
            .load_resource_with(&dir, "resource", Some(&url), 0, 100, decode)
            .is_err());
        assert_eq!(
            server.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(fs::read(&source).unwrap(), b"invalid");
        fs::remove_file(source).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn parsed_resource_reuses_validation_and_keeps_cached_value_on_invalid_update() {
        use std::{cell::Cell, net::TcpListener};
        let dir =
            std::env::temp_dir().join(format!("astra-parsed-resource-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cache"), b"42").unwrap();
        let calls = Cell::new(0);
        let decode = |bytes: &[u8]| -> Result<u32> {
            calls.set(calls.get() + 1);
            Ok(std::str::from_utf8(bytes)?.parse()?)
        };
        let (bytes, value) = load_resource_with(&dir, "cache", None, 0, 100, decode).unwrap();
        assert_eq!((bytes.as_slice(), value), (b"42".as_slice(), 42));
        assert_eq!(calls.get(), 1);

        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/resource", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 1024];
            let received = stream.read(&mut request).unwrap();
            assert!(received > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                .unwrap();
        });
        let (bytes, value) = load_resource_with(&dir, "cache", Some(&url), 0, 100, decode).unwrap();
        worker.join().unwrap();
        assert_eq!((bytes.as_slice(), value), (b"42".as_slice(), 42));
        assert_eq!(calls.get(), 3); // one cached decode and one rejected candidate
        assert_eq!(fs::read(dir.join("cache")).unwrap(), b"42");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remote_download_uses_runtime_cache_without_overwriting_local_resource() {
        use std::net::TcpListener;
        let dir =
            std::env::temp_dir().join(format!("astra-isolated-resource-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("resource");
        fs::write(&source, b"original local resource").unwrap();
        let runtime = astra_platform::storage::runtime_path(&source, "resources").unwrap();
        let _ = fs::remove_file(&runtime);
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/resource", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 1024];
            let received = stream.read(&mut request).unwrap();
            assert!(received > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n42")
                .unwrap();
        });
        let decode = |bytes: &[u8]| -> Result<u32> { Ok(std::str::from_utf8(bytes)?.parse()?) };
        let (_, number) =
            load_resource_with(&dir, "resource", Some(&url), 3600, 100, decode).unwrap();
        worker.join().unwrap();
        assert_eq!(number, 42);
        assert_eq!(fs::read(&source).unwrap(), b"original local resource");
        assert_eq!(fs::read(&runtime).unwrap(), b"42");
        assert_eq!(
            load_resource_with(&dir, "resource", Some(&url), 3600, 100, decode)
                .unwrap()
                .1,
            42
        );
        fs::remove_file(runtime).unwrap();
        fs::remove_file(source).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn provider_errors_keep_entry_and_original_text_line_context() {
        let source = RuleSet {
            format: "text".into(),
            ..Default::default()
        };
        let error = source
            .parse(b"# comment\n\nDOMAIN,valid.test\n// comment\nDOMAIN-REGEX,[\n")
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("line 5") && error.contains("entry 2") && error.contains("regex"),
            "{error}"
        );
        let error = RuleSet::default()
            .parse(b"payload: ['DOMAIN,valid.test', 'IP-CIDR,not-an-ip']")
            .unwrap_err();
        assert!(format!("{error:#}").contains("entry 2"));
        assert!(RuleSet {
            behavior: "domain".into(),
            ..Default::default()
        }
        .parse_entries(&["bad\tdomain.test".into()])
        .is_err());
    }
    #[test]
    fn no_resolve_preserves_matchers_in_classical_providers() {
        let entries = [
            "DOMAIN,csgo.wmsj.cn",
            "DOMAIN-SUFFIX,steamcontent.com",
            "DOMAIN-KEYWORD,steam",
            "DOMAIN-REGEX,^steam",
            "GEOSITE,cn",
            "IP-CIDR,10.0.0.0/8",
            "IP-CIDR6,fc00::/7",
            "GEOIP,LAN",
            "DST-PORT,443",
            "NETWORK,tcp",
            "PROCESS-NAME,browser.exe",
        ];
        for entry in entries {
            let plain = parse_classical(entry).unwrap();
            let flagged = parse_classical(&format!("{entry},no-resolve")).unwrap();
            assert_eq!(plain.kind, flagged.kind);
            assert_eq!(plain.value, flagged.value);
        }
        let provider = RuleSet::default();
        assert_eq!(
            provider
                .parse(b"payload: ['DOMAIN,a.test,no-resolve','DOMAIN-SUFFIX,b.test,no-resolve']")
                .unwrap()
                .len(),
            2
        );
        for entry in [
            "DOMAIN,a.test,resolve",
            "DOMAIN,a.test,no-resolve,extra",
            "UNKNOWN,a.test,no-resolve",
        ] {
            assert!(parse_classical(entry).is_err(), "{entry}");
        }
    }
    #[test]
    fn provider_formats_and_wildcards_preserve_their_meanings() {
        let source = RuleSet {
            behavior: "domain".into(),
            ..Default::default()
        };
        let parsed = source
            .parse(b"payload: ['+.example.com', '.sub.test', '*.*.test', 'exact.test']")
            .unwrap();
        assert_eq!(
            parsed.iter().map(|m| m.kind.as_str()).collect::<Vec<_>>(),
            [
                "domain_suffix",
                "domain_subdomain",
                "domain_pattern",
                "domain_exact"
            ]
        );
        let source = RuleSet {
            behavior: "ipcidr".into(),
            format: "text".into(),
            ..Default::default()
        };
        assert_eq!(
            source
                .parse(b"# ignore\n10.0.0.0/8\n::1/128\n")
                .unwrap()
                .len(),
            2
        );
        let source = RuleSet {
            format: "text".into(),
            ..Default::default()
        };
        assert_eq!(
            source
                .parse(b"DOMAIN-KEYWORD,ads\nGEOIP,CN,no-resolve\nPROCESS-NAME,browser.exe\n")
                .unwrap()
                .len(),
            3
        );
    }
    #[test]
    fn invalid_provider_content_is_rejected_instead_of_dropped() {
        let source = RuleSet::default();
        for data in [
            b"payload: ['SRC-PORT,80']".as_slice(),
            b"payload: ['DOMAIN,bad domain']",
            b"payload: ['PROCESS-PATH,relative.exe']",
        ] {
            assert!(source.parse(data).is_err());
        }
        assert!(RuleSet {
            format: "mrs".into(),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(RuleSet {
            behavior: "domain".into(),
            ..Default::default()
        }
        .parse(b"payload: ['bad..domain']")
        .is_err());
        assert!(source.parse(b"payload: ['GEOIP,bad category']").is_err());
    }
    #[test]
    fn missing_or_oversized_local_resources_fail() {
        let dir = std::env::temp_dir().join(format!("astra-source-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_resource(&dir, "missing", None, 1, 4, |_| Ok(())).is_err());
        std::fs::write(dir.join("large"), b"12345").unwrap();
        assert!(load_resource(&dir, "large", None, 1, 4, |_| Ok(())).is_err());
        std::fs::remove_file(dir.join("large")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
