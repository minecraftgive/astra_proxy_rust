use crate::{i18n::t, protocol::Kind};
use anyhow::{ensure, Context, Result};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

const LIMIT: usize = 480 * 1024;

/// Terminals paste dropped paths, commonly wrapped in quotes. No shell is run.
pub fn path(input: &str, directory: &Path) -> Result<PathBuf> {
    let input = input.trim();
    let input = input.strip_prefix("& ").unwrap_or(input).trim();
    let input = if input.len() >= 2
        && ((input.starts_with('"') && input.ends_with('"'))
            || (input.starts_with('\'') && input.ends_with('\'')))
    {
        &input[1..input.len().saturating_sub(1)]
    } else {
        input
    };
    ensure!(
        !input.is_empty() && !input.contains(['\n', '\r', '\0']),
        t("Choose one TOML or YAML file")
    );
    let path = PathBuf::from(input);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

pub struct Imported {
    pub text: String,
    pub warnings: Vec<String>,
}

pub fn load(path: &Path, kind: Kind, current: &str) -> Result<Imported> {
    ensure!(path.is_file(), t("Choose one TOML or YAML file"));
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    let source = String::from_utf8(bytes).context(t("Configuration must be UTF-8"))?;
    let source = source.trim_start_matches('\u{feff}');
    let base = path.parent().context("Missing import directory")?;
    let yaml = path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("yaml") || s.eq_ignore_ascii_case("yml"));
    let (text, warnings) = if yaml {
        let current: toml::Table = toml::from_str(current)?;
        let socket_key = if kind == Kind::Proxy {
            "rule_socket"
        } else {
            "socket"
        };
        let converted = astra_convert::convert(
            source,
            &astra_convert::Options {
                base_dir: base.to_path_buf(),
                rule_socket: current
                    .get(socket_key)
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned),
                strict_rules: true,
                ..Default::default()
            },
        )?;
        (
            if kind == Kind::Proxy {
                converted.proxy
            } else {
                converted.rules
            },
            converted.warnings,
        )
    } else {
        ensure!(
            path.extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("toml")),
            t("Choose one TOML or YAML file")
        );
        (source.to_owned(), Vec::new())
    };
    let mut doc: toml_edit::DocumentMut = text.parse().context(t("Invalid configuration"))?;
    // Reject the opposite core's document before replacing the draft.
    let proxy = [
        "rule_socket",
        "standalone",
        "outbounds",
        "socks5_listen",
        "http_listen",
        "tun",
    ]
    .iter()
    .any(|key| doc.contains_key(key));
    let rule = [
        "socket",
        "rules",
        "default_action",
        "rule_sets",
        "geoip_database",
        "geosite_database",
    ]
    .iter()
    .any(|key| doc.contains_key(key));
    ensure!(
        if kind == Kind::Proxy {
            proxy && !rule
        } else {
            rule && !proxy
        },
        t("Configuration does not match this core")
    );
    if kind == Kind::Rule {
        for key in ["geoip_database", "geosite_database"] {
            if let Some(item) = doc.get_mut(key) {
                rebase(item, base);
            }
        }
        if let Some(sets) = doc
            .get_mut("rule_sets")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            for (_, set) in sets.iter_mut() {
                if let Some(item) = set.as_table_like_mut().and_then(|s| s.get_mut("path")) {
                    rebase(item, base);
                }
            }
        }
    }
    let text = doc.to_string();
    ensure!(text.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    Ok(Imported { text, warnings })
}

fn rebase(item: &mut toml_edit::Item, base: &Path) {
    if let Some(value) = item.as_str().filter(|s| !s.is_empty()) {
        let path = Path::new(value);
        if path.is_relative() {
            *item = toml_edit::value(base.join(path).to_string_lossy().into_owned());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub struct Fixture(pub PathBuf);
    impl Fixture {
        pub fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "astra-import-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        pub fn write(&self, name: &str, text: impl AsRef<[u8]>) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn dropped_paths_handle_spaces_unicode_quotes_and_powershell_prefix_without_shell() {
        let directory = std::env::current_dir().unwrap();
        for text in [
            "配置 file.toml",
            "\"配置 file.toml\"",
            "'配置 file.toml'",
            "& '配置 file.toml'",
        ] {
            assert_eq!(
                path(text, &directory).unwrap(),
                directory.join("配置 file.toml")
            );
        }
        for text in ["", "\"\"", "'a.toml'\n'b.toml'", "a\0.toml"] {
            assert!(path(text, &directory).is_err());
        }
        assert!(path("'", &directory).is_ok()); // A literal filename; never panic on one quote.
    }
    #[test]
    fn native_import_keeps_comments_and_rebases_both_rule_set_forms() {
        let fixture = Fixture::new();
        for sets in [
            "[rule_sets.local]\npath='lists/local.yaml'\n",
            "[rule_sets]\nlocal={path='lists/local.yaml'}\n",
        ] {
            let path = fixture.write("规则 TOML.toml", format!("\u{feff}# keep me\nsocket='rule.sock'\ngeoip_database='Country.mmdb'\ngeosite_database='geosite.dat'\n{sets}"));
            let imported = load(&path, Kind::Rule, "").unwrap();
            assert!(imported.text.contains("# keep me"));
            let doc: toml::Value = toml::from_str(&imported.text).unwrap();
            assert_eq!(
                doc["geoip_database"].as_str().unwrap(),
                fixture.0.join("Country.mmdb").to_str().unwrap()
            );
            assert_eq!(
                doc["rule_sets"]["local"]["path"].as_str().unwrap(),
                fixture.0.join("lists/local.yaml").to_str().unwrap()
            );
            assert_eq!(doc["socket"].as_str(), Some("rule.sock"));
            assert!(imported.warnings.is_empty());
        }
    }
    #[test]
    fn mihomo_import_selects_core_preserves_endpoint_and_stays_offline() {
        let fixture = Fixture::new();
        let path = fixture.write("配置.yml", "socks-port: 1088\nproxies:\n  - {name: edge, type: socks5, server: 127.0.0.1, port: 1080}\nrule-providers:\n  local: {type: file, behavior: domain, path: './local.yaml'}\nrules:\n  - RULE-SET,local,edge\n  - GEOSITE,cn,DIRECT\n  - MATCH,edge\n");
        let proxy = load(&path, Kind::Proxy, "rule_socket='bound.rules'").unwrap();
        let proxy: toml::Value = toml::from_str(&proxy.text).unwrap();
        assert_eq!(proxy["rule_socket"].as_str(), Some("bound.rules"));
        assert_eq!(proxy["outbounds"][0]["tag"].as_str(), Some("edge"));
        assert!(proxy.get("rules").is_none());
        let rules = load(&path, Kind::Rule, "socket='bound.rules'").unwrap();
        assert!(!rules.warnings.is_empty());
        let rules: toml::Value = toml::from_str(&rules.text).unwrap();
        assert_eq!(rules["socket"].as_str(), Some("bound.rules"));
        assert_eq!(rules["rules"][1]["kind"].as_str(), Some("geosite"));
        assert_eq!(
            rules["rule_sets"]["local"]["path"].as_str().unwrap(),
            fixture.0.join("./local.yaml").to_str().unwrap()
        );
        assert!(!fixture.0.join("geosite").exists());
    }
    #[test]
    fn invalid_wrong_core_large_and_unsupported_inputs_are_rejected() {
        let fixture = Fixture::new();
        for (name, text, kind) in [
            ("wrong.toml", "socket='r'", Kind::Proxy),
            ("wrong2.toml", "standalone=true", Kind::Rule),
            ("bad.toml", "socket=[", Kind::Rule),
            ("empty.yaml", "", Kind::Rule),
            ("bad.yml", "rules: ['UNSUPPORTED,test,DIRECT']", Kind::Rule),
            ("data.txt", "socket='r'", Kind::Rule),
        ] {
            assert!(
                load(&fixture.write(name, text), kind, "socket='bound'").is_err(),
                "{name}"
            );
        }
        assert!(load(
            &fixture.write("large.toml", vec![b'a'; LIMIT + 1]),
            Kind::Rule,
            ""
        )
        .is_err());
        assert!(load(&fixture.write("binary.toml", [0xff, 0xfe]), Kind::Rule, "").is_err());
    }
}
