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
#[path = "../test/subscription_diagnostics_test.rs"]
mod tests;
