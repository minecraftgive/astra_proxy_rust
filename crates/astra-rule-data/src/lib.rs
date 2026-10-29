//! Shared rule-provider format and bounded resource loading.
pub mod geoip;
pub mod geosite;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleSet {
    #[serde(skip_serializing_if = "is_enabled")]
    pub enabled: bool,
    pub behavior: String,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub interval: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<String>,
}
impl Default for RuleSet {
    fn default() -> Self {
        Self {
            enabled: true,
            behavior: "classical".into(),
            format: "yaml".into(),
            path: None,
            url: None,
            interval: 86400,
            payload: Vec::new(),
        }
    }
}
fn is_enabled(value: &bool) -> bool {
    *value
}
pub fn compile_domain_regex(pattern: &str) -> Result<regex::Regex> {
    ensure!(pattern.len() <= 4096, "domain regex exceeds 4096 bytes");
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .size_limit(2 * 1024 * 1024)
        .build()
        .context("invalid or unsupported domain regex")
}
impl RuleSet {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.behavior.as_str(), "classical" | "domain" | "ipcidr"),
            "unsupported rule-set behavior: {}",
            self.behavior
        );
        ensure!(
            matches!(self.format.as_str(), "yaml" | "text"),
            "unsupported rule-set format: {} (use yaml or text; MRS is not supported)",
            self.format
        );
        ensure!(
            self.interval <= 31_536_000,
            "rule-set interval exceeds one year"
        );
        ensure!(
            self.url.is_none() || self.path.is_some(),
            "HTTP rule-set requires a cache path"
        );
        if let Some(path) = &self.path {
            ensure!(
                !path.trim().is_empty() && !path.contains('\0'),
                "rule-set path must be a nonempty file path"
            );
        }
        ensure!(
            self.payload.is_empty() || (self.path.is_none() && self.url.is_none()),
            "inline payload cannot be combined with path or url"
        );
        if let Some(url) = &self.url {
            validate_url(url)?;
        }
        Ok(())
    }
    pub fn parse(&self, data: &[u8]) -> Result<Vec<MatchSpec>> {
        let text = std::str::from_utf8(data)
            .context("rule-set must be UTF-8")?
            .trim_start_matches('\u{feff}');
        let entries: Vec<String> =
            match self.format.as_str() {
                "yaml" => {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Payload {
                        payload: Vec<String>,
                    }
                    serde_yaml::from_str::<Payload>(text)
                        .context("expected YAML payload list")?
                        .payload
                }
                "text" => {
                    let mut output = Vec::new();
                    for (line, entry) in text.lines().enumerate() {
                        let entry = entry.trim();
                        if entry.is_empty() || entry.starts_with('#') || entry.starts_with("//") {
                            continue;
                        }
                        ensure!(output.len() < 500_000, "rule-set exceeds 500000 entries");
                        let index = output.len() + 1;
                        output.push(self.parse_entry(entry).with_context(|| {
                            format!("rule-set entry {index} (line {})", line + 1)
                        })?);
                    }
                    return Ok(output);
                }
                _ => bail!("unsupported rule-set format"),
            };
        self.parse_entries(&entries)
    }
    pub fn parse_entries(&self, entries: &[String]) -> Result<Vec<MatchSpec>> {
        ensure!(entries.len() <= 500_000, "rule-set exceeds 500000 entries");
        entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                self.parse_entry(entry.trim())
                    .with_context(|| format!("rule-set entry {}", i + 1))
            })
            .collect()
    }
    fn parse_entry(&self, entry: &str) -> Result<MatchSpec> {
        match self.behavior.as_str() {
            "classical" => {
                let matcher = parse_classical(entry)?;
                ensure!(
                    matcher.kind != "rule_set",
                    "RULE-SET cannot be nested in a classical provider"
                );
                Ok(matcher)
            }
            "ipcidr" => {
                entry
                    .parse::<ipnet::IpNet>()
                    .context("invalid rule-set CIDR")?;
                Ok(MatchSpec {
                    kind: "ip_cidr".into(),
                    value: entry.into(),
                })
            }
            "domain" => domain(entry),
            _ => bail!("unsupported rule-set behavior"),
        }
    }
    pub fn load(&self, base: &Path) -> Result<Vec<MatchSpec>> {
        self.validate()?;
        if self.path.is_none() {
            return self.parse_entries(&self.payload);
        }
        let bytes = load_resource(
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

fn domain(value: &str) -> Result<MatchSpec> {
    let value = value.trim_end_matches('.').to_ascii_lowercase();
    ensure!(
        !value.is_empty()
            && value.len() <= 253
            && !value.chars().any(char::is_whitespace)
            && !value.contains([',', '/', '\\']),
        "invalid rule-set domain"
    );
    let (kind, value) = if let Some(suffix) = value.strip_prefix("+.") {
        ("domain_suffix", suffix.to_owned())
    } else if let Some(suffix) = value.strip_prefix('.') {
        ("domain_subdomain", suffix.to_owned())
    } else if value.contains('*') {
        ("domain_pattern", value)
    } else {
        ("domain_exact", value)
    };
    ensure!(
        !value.is_empty()
            && value.split('.').all(|label| !label.is_empty()
                && !label.contains('+')
                && (!label.contains('*') || label == "*")),
        "invalid domain wildcard"
    );
    Ok(MatchSpec {
        kind: kind.into(),
        value,
    })
}

/// Classical providers omit the routing action. All native matchers operate on
/// the supplied target without DNS lookups, so trailing `no-resolve` is redundant
/// but accepted for every supported matcher (including domain rules).
pub fn parse_classical(input: &str) -> Result<MatchSpec> {
    let parts: Vec<_> = input.split(',').map(str::trim).collect();
    ensure!(
        parts.len() == 2 || (parts.len() == 3 && parts[2] == "no-resolve"),
        "unsupported classical rule parameters: {input}"
    );
    let value = parts[1];
    ensure!(!value.is_empty(), "empty rule value");
    let kind = match parts[0] {
        "DOMAIN" => "domain_exact",
        "DOMAIN-SUFFIX" => "domain_suffix",
        "DOMAIN-KEYWORD" => "domain_keyword",
        "DOMAIN-REGEX" => "domain_regex",
        "GEOSITE" => {
            geosite::selector(value)?;
            "geosite"
        }
        "IP-CIDR" | "IP-CIDR6" => {
            value.parse::<ipnet::IpNet>().context("invalid CIDR")?;
            "ip_cidr"
        }
        "DST-PORT" => {
            ensure!(
                value.parse::<u16>().is_ok_and(|p| p > 0),
                "expected single nonzero destination port"
            );
            "port"
        }
        "NETWORK" => {
            ensure!(
                matches!(value.to_ascii_lowercase().as_str(), "tcp" | "udp"),
                "invalid network"
            );
            "network"
        }
        "PROCESS-NAME" => "process_name",
        "PROCESS-PATH" => "process_path",
        "GEOIP" => {
            geoip::category(value)?;
            "geoip"
        }
        "RULE-SET" => "rule_set",
        other => bail!("unsupported rule type {other}"),
    };
    match kind {
        "domain_keyword" => ensure!(
            value.len() <= 253 && !value.chars().any(char::is_whitespace),
            "invalid domain keyword"
        ),
        "domain_regex" => {
            compile_domain_regex(value)?;
        }
        "domain_exact" | "domain_suffix" => ensure!(
            value.len() <= 253
                && !value.chars().any(char::is_whitespace)
                && !value.starts_with('.')
                && !value.trim_end_matches('.').is_empty()
                && !value.trim_end_matches('.').split('.').any(str::is_empty)
                && !value.contains(['*', '+', '/', '\\', '\0']),
            "invalid domain matcher"
        ),
        "process_name" => ensure!(
            value.len() <= 256 && !value.contains(['/', '\\', '\0']),
            "invalid process name"
        ),
        "process_path" => ensure!(
            value.len() <= 4096
                && !value.contains('\0')
                && (value.starts_with('/')
                    || value.starts_with("\\\\")
                    || (value.as_bytes().get(1) == Some(&b':')
                        && matches!(value.as_bytes().get(2), Some(b'/' | b'\\')))),
            "process_path requires an absolute executable path"
        ),
        _ => {}
    }
    Ok(MatchSpec {
        kind: kind.into(),
        value: match kind {
            "network" => value.to_ascii_lowercase(),
            "geoip" => value.to_ascii_uppercase(),
            _ => value.into(),
        },
    })
}

pub fn resolve(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    }
}
fn validate_url(url: &str) -> Result<()> {
    ensure!(
        url.starts_with("https://") || url.starts_with("http://"),
        "resource URL must use HTTP(S)"
    );
    Ok(())
}

/// A failed update retains a validated cache. Invalid new content never replaces it.
pub fn load_resource(
    base: &Path,
    path: &str,
    url: Option<&str>,
    interval: u64,
    limit: u64,
    validate: impl Fn(&[u8]) -> Result<()>,
) -> Result<Vec<u8>> {
    load_resource_with(base, path, url, interval, limit, validate).map(|(bytes, ())| bytes)
}

/// Preserve the validated value alongside its bytes, including on update fallback.
/// Callers can reuse a parsed index instead of decoding the resource a second time.
pub fn load_resource_with<T>(
    base: &Path,
    path: &str,
    url: Option<&str>,
    interval: u64,
    limit: u64,
    decode: impl Fn(&[u8]) -> Result<T>,
) -> Result<(Vec<u8>, T)> {
    let source = resolve(base, path);
    let path = if url.is_some() {
        astra_ipc::storage::runtime_path(&source, "resources")?
    } else {
        source.clone()
    };
    let read = || -> Result<(Vec<u8>, T)> {
        let mut data = Vec::new();
        let input = if path.exists() { &path } else { &source };
        fs::File::open(input)?
            .take(limit + 1)
            .read_to_end(&mut data)?;
        ensure!(data.len() as u64 <= limit, "resource exceeds size limit");
        let parsed = decode(&data)?;
        Ok((data, parsed))
    };
    let Some(url) = url else {
        return read().with_context(|| format!("load resource {}", path.display()));
    };
    validate_url(url)?;
    let cached = read().ok();
    let fresh = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < interval);
    if fresh && cached.is_some() {
        return Ok(cached.unwrap());
    }
    let update = (|| -> Result<(Vec<u8>, T)> {
        let response = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(15))
            .redirects(3)
            .build()
            .get(url)
            .call()
            .context("resource download failed")?;
        let mut data = Vec::new();
        response
            .into_reader()
            .take(limit + 1)
            .read_to_end(&mut data)?;
        ensure!(data.len() as u64 <= limit, "download exceeds size limit");
        let parsed = decode(&data).context("downloaded resource is invalid")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            astra_ipc::platform::set_private(parent, 0o700)?;
        }
        // Unique create-new temporary file; never truncate the active cache.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary = path.with_extension(format!("download-{}-{nonce}", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&data)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok((data, parsed))
    })();
    match (update, cached) {
        (Ok(data), _) => Ok(data),
        (Err(error), Some(data)) => {
            tracing::warn!(%error,resource=%path.display(),"resource update failed; keeping validated cache");
            Ok(data)
        }
        (Err(error), None) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            stream.read(&mut request).unwrap();
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
        let runtime = astra_ipc::storage::runtime_path(&source, "resources").unwrap();
        let _ = fs::remove_file(&runtime);
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/resource", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 1024];
            stream.read(&mut request).unwrap();
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
