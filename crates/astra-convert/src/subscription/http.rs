//! Shared HTTP limits and bounded content decoding for both download paths.
use anyhow::{ensure, Context, Result};
use std::io::Read;

pub const MAX_REDIRECTS: usize = 10;
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub const ACCEPT_ENCODING: &str = "gzip, deflate";

/// Limit both transferred and expanded bytes, including concatenated gzip members.
pub fn decode(reader: impl Read, encoding: &str) -> Result<String> {
    let mut data = Vec::new();
    reader
        .take(super::LIMIT + 1)
        .read_to_end(&mut data)
        .with_context(|| {
            format!(
                "read subscription response body (received {} bytes)",
                data.len()
            )
        })?;
    ensure!(
        data.len() as u64 <= super::LIMIT,
        "subscription exceeds 8 MiB"
    );
    match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => super::bounded(data.as_slice()),
        "gzip" | "x-gzip" => super::bounded(flate2::read::MultiGzDecoder::new(data.as_slice()))
            .context("decode gzip subscription response"),
        "deflate" => super::bounded(flate2::read::ZlibDecoder::new(data.as_slice()))
            .context("decode deflate subscription response"),
        _ => anyhow::bail!(
            "unsupported subscription Content-Encoding; expected identity, gzip or deflate"
        ),
    }
}

#[cfg(test)]
#[path = "../test/subscription_http_test.rs"]
mod tests;
