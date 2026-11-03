//! Shared configuration declarations and pure validation.
pub mod dns;
pub mod protocol;
pub mod proxy;
pub mod rule;
pub mod ruleset;
pub mod server;
pub mod subscription;
/// Input URLs cannot carry credentials or use non-HTTP transports.
pub fn validate_remote_url(value: &str) -> anyhow::Result<url::Url> {
    let url = url::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid resource URL"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "resource URL requires HTTP(S) without URL credentials"
    );
    Ok(url)
}

pub fn resolve(base: &std::path::Path, path: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    }
}

pub fn validate_redirect(from: &str, to: &str) -> anyhow::Result<()> {
    let target = validate_remote_url(to)?;
    anyhow::ensure!(
        !from.starts_with("https://") || target.scheme() == "https",
        "redirect from HTTPS to plain HTTP refused"
    );
    Ok(())
}
