//! Bounded error chains for logs, without URLs containing subscription secrets
//! or parser source excerpts containing node credentials.
use std::sync::OnceLock;

pub fn endpoint(value: &str) -> String {
    url::Url::parse(value)
        .ok()
        .and_then(|url| {
            Some(format!(
                "{}://{}{}",
                url.scheme(),
                url.host()?,
                url.port()
                    .map(|port| format!(":{port}"))
                    .unwrap_or_default()
            ))
        })
        .unwrap_or_else(|| "[invalid URL]".into())
}

pub fn error(error: &anyhow::Error) -> String {
    let mut details = Vec::new();
    for cause in error.chain().take(12) {
        let message = if let Some(parse) = cause.downcast_ref::<toml::de::Error>() {
            // Display includes the full offending source line; message does not.
            format!(
                "TOML: {}{}",
                parse.message(),
                parse
                    .span()
                    .map(|span| format!(" (bytes {}..{})", span.start, span.end))
                    .unwrap_or_default()
            )
        } else if let Some(parse) = cause.downcast_ref::<serde_yaml::Error>() {
            format!(
                "YAML{}: {}",
                parse
                    .location()
                    .map(|loc| format!(" at line {}, column {}", loc.line(), loc.column()))
                    .unwrap_or_default(),
                parse.to_string().lines().next().unwrap_or_default()
            )
        } else {
            cause.to_string()
        };
        // A parser may append source excerpts. Only its diagnostic line is useful.
        details.push(redact(message.lines().next().unwrap_or_default()));
    }
    details.join(": ").chars().take(4096).collect()
}

fn redact(text: &str) -> String {
    static URLS: OnceLock<regex::Regex> = OnceLock::new();
    static ASSIGNMENTS: OnceLock<regex::Regex> = OnceLock::new();
    static QUOTED: OnceLock<regex::Regex> = OnceLock::new();
    let urls =
        URLS.get_or_init(|| regex::Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s<>"'`]+"#).unwrap());
    let text = urls.replace_all(text, |captures: &regex::Captures<'_>| {
        endpoint(&captures[0])
    });
    let assignments = ASSIGNMENTS.get_or_init(|| regex::Regex::new(
        r#"(?i)\b(password|passwd|token|uuid|authorization|secret|api[_-]?key)\s*[:=]\s*[^\s,;]+"#).unwrap());
    let text = assignments.replace_all(&text, "$1=[redacted]");
    let quoted =
        QUOTED.get_or_init(|| regex::Regex::new(r#""[^"\r\n]*"|'[^'\r\n]*'|`[^`\r\n]*`"#).unwrap());
    quoted
        .replace_all(&text, "[redacted]")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn retains_causes_and_location_but_never_parser_source_or_url_credentials() {
        let parse = toml::from_str::<toml::Table>("password = 'node-secret' trailing").unwrap_err();
        let error = anyhow::Error::new(parse).context("validate merged subscription");
        let report = super::error(&error);
        assert!(
            report.contains("validate merged subscription")
                && report.contains("TOML:")
                && report.contains("bytes")
        );
        assert!(!report.contains("node-secret"));
        let error = anyhow::anyhow!("HTTP 403 https://user:pass@example.com/private-token?token=secret#fragment password=credential uuid='identifier' invalid value \"node-secret\"").context("download provider");
        let report = super::error(&error);
        assert!(
            report.contains("HTTP 403 https://example.com") && report.contains("download provider")
        );
        for secret in [
            "user:pass",
            "private-token",
            "secret#",
            "credential",
            "identifier",
            "node-secret",
        ] {
            assert!(!report.contains(secret), "{report}");
        }
        assert!(super::error(&anyhow::anyhow!("x".repeat(10000))).len() <= 4096);
    }
}
