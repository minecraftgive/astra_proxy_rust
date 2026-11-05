use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub manager_socket: PathBuf,
    pub state_dir: PathBuf,
    /// Optional dedicated credential file; otherwise state_dir/administrator.phc.
    pub credentials_file: Option<PathBuf>,
    #[serde(default = "listen")]
    pub listen: SocketAddr,
    pub public_origin: String,
    pub tls: Option<Tls>,
    #[serde(default)]
    pub dev_http: bool,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}
fn listen() -> SocketAddr {
    "127.0.0.1:8787".parse().unwrap()
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read web configuration")?;
        let mut config: Self = toml::from_str(&text).context("parse web configuration")?;
        let base = path.parent().unwrap_or(Path::new("."));
        for file in [&mut config.manager_socket, &mut config.state_dir] {
            if file.is_relative() {
                *file = base.join(&*file);
            }
        }
        if let Some(file) = &mut config.credentials_file {
            if file.is_relative() {
                *file = base.join(&*file);
            }
        }
        if let Some(tls) = &mut config.tls {
            for file in [&mut tls.certificate, &mut tls.private_key] {
                if file.is_relative() {
                    *file = base.join(&*file);
                }
            }
        }
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.public_origin.ends_with('/'),
            "public_origin must be an origin without a trailing slash"
        );
        let origin: axum::http::Uri = self
            .public_origin
            .parse()
            .context("invalid public_origin")?;
        ensure!(
            origin.authority().is_some() && origin.path() == "/" && origin.query().is_none(),
            "public_origin must contain only scheme, hostname and optional port"
        );
        ensure!(
            !origin.authority().unwrap().as_str().contains('@'),
            "public_origin cannot contain credentials"
        );
        if self.tls.is_some() {
            ensure!(
                origin.scheme_str() == Some("https"),
                "TLS requires an https public_origin"
            );
        } else {
            ensure!(self.dev_http && self.listen.ip().is_loopback(), "HTTP is only allowed with dev_http=true on loopback; configure TLS for LAN/public access");
            ensure!(
                origin.scheme_str() == Some("http"),
                "development HTTP requires an http public_origin"
            );
        }
        Ok(())
    }
    pub fn password_file(&self) -> PathBuf {
        self.credentials_file
            .clone()
            .unwrap_or_else(|| self.state_dir.join("administrator.phc"))
    }
}

#[cfg(test)]
#[path = "test/config_test.rs"]
mod tests;
