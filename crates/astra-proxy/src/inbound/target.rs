//! Preserve the observed address while attaching names used for rule matching.
use crate::App;
use anyhow::Result;
use astra_model::Target;
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) fn select(
    app: &App,
    original: &Target,
    sniffed: Option<String>,
) -> Result<(Target, Target)> {
    let synthetic = original.host.parse().is_ok_and(|ip| app.synthetic_ip(ip));
    let mapped = app.restore_target(original);
    let route = if synthetic {
        match mapped {
            Ok(target) => target,
            Err(error) => match sniffed {
                Some(host) => Target {
                    host,
                    port: original.port,
                },
                None => return Err(error),
            },
        }
    } else {
        let mut target = mapped?;
        if let Some(host) = sniffed {
            target.host = host;
        }
        target
    };
    let dial = if synthetic {
        route.clone()
    } else {
        original.clone()
    };
    Ok((route, dial))
}

pub(crate) async fn preface<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<(Vec<u8>, Option<String>)> {
    use astra_protocols::sniff::{self, Result as Inspection};
    let mut bytes = Vec::with_capacity(2048);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match sniff::inspect(&bytes) {
            Inspection::Host(host) => return Ok((bytes, Some(host))),
            Inspection::Unknown => return Ok((bytes, None)),
            Inspection::NeedMore => {}
        }
        if bytes.len() >= sniff::LIMIT {
            return Ok((bytes, None));
        }
        let mut buffer = [0; 2048];
        let capacity = buffer.len().min(sniff::LIMIT - bytes.len());
        match tokio::time::timeout_at(deadline, stream.read(&mut buffer[..capacity])).await {
            Ok(Ok(0)) | Err(_) => return Ok((bytes, None)),
            Ok(Ok(size)) => bytes.extend_from_slice(&buffer[..size]),
            Ok(Err(error)) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
#[path = "../test/proxy_target_test.rs"]
mod tests;
