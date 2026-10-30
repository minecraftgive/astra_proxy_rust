//! Lexical highlighting for single field values: lists, addresses, URLs, rule
//! matchers and actions. It never validates; half-typed input still colors sensibly.
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use std::net::IpAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// Detect each value: URL, IP/CIDR, host:port, domain pattern, number.
    Auto,
    Number,
    /// Rule action: direct, block or proxy:NAME.
    Action,
    /// GEOSITE category with optional @attribute / @!attribute.
    Geosite,
    /// Domain pattern with `*` label wildcards.
    Pattern,
    Regex,
}

const PUNCT: Color = Color::Gray;
const NUMBER: Color = Color::Yellow;
const ADDRESS: Color = Color::Cyan;
const MARK: Color = Color::Magenta;

/// Picks the flavor for a field from its key and, for rule values, the rule kind.
pub fn flavor(key: &str, number: bool, rule_kind: Option<&str>) -> Flavor {
    if number {
        return Flavor::Number;
    }
    match key {
        "action" | "default_action" => Flavor::Action,
        "value" => match rule_kind {
            Some("domain_regex") => Flavor::Regex,
            Some("geosite") => Flavor::Geosite,
            Some("domain_pattern") => Flavor::Pattern,
            Some("port") => Flavor::Number,
            _ => Flavor::Auto,
        },
        _ if key.ends_with("filter") => Flavor::Regex,
        _ => Flavor::Auto,
    }
}

/// Colors `text`. A TOML string list keeps its brackets, quotes and commas
/// subdued and highlights each item with `flavor`.
pub fn spans(text: &str, flavor: Flavor) -> Vec<Span<'static>> {
    let (value, comment) = split_comment(text);
    let mut out = if value.trim_start().starts_with('[') && !looks_like_bracketed_ipv6(value) {
        list(value, flavor)
    } else {
        item(value, flavor)
    };
    if let Some(comment) = comment {
        out.push(styled(comment, Color::DarkGray));
    }
    out
}

/// Non-string TOML values keep their trailing `# comment`; it is shown dimmed and
/// never highlighted as data. Strings never carry one, so `#` inside them is kept.
fn split_comment(text: &str) -> (&str, Option<&str>) {
    let mut quote = None;
    for (index, c) in text.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '#') if index > 0 && text[..index].ends_with(char::is_whitespace) => {
                let value = text[..index].trim();
                let structured = value.starts_with('[')
                    || value.parse::<f64>().is_ok()
                    || value == "true"
                    || value == "false";
                return if structured {
                    (&text[..index], Some(&text[index..]))
                } else {
                    (text, None)
                };
            }
            _ => {}
        }
    }
    (text, None)
}

fn looks_like_bracketed_ipv6(text: &str) -> bool {
    let text = text.trim();
    text.find(']').is_some_and(|end| {
        text[1..end].parse::<std::net::Ipv6Addr>().is_ok() && !text[..end].contains('"')
    })
}

fn styled(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}

fn list(text: &str, flavor: Flavor) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    let flush = |plain: &mut String, out: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            out.push(Span::raw(std::mem::take(plain)));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '[' | ']' | ',' => {
                flush(&mut plain, &mut out);
                out.push(styled(c.to_string(), PUNCT));
            }
            '"' | '\'' => {
                flush(&mut plain, &mut out);
                out.push(styled(c.to_string(), PUNCT));
                let mut value = String::new();
                let mut closed = false;
                while let Some(next) = chars.next() {
                    if next == '\\' && c == '"' {
                        value.push(next);
                        if let Some(escaped) = chars.next() {
                            value.push(escaped);
                        }
                    } else if next == c {
                        closed = true;
                        break;
                    } else {
                        value.push(next);
                    }
                }
                out.extend(item(&value, flavor));
                if closed {
                    out.push(styled(c.to_string(), PUNCT));
                }
            }
            _ => plain.push(c),
        }
    }
    flush(&mut plain, &mut out);
    out
}

/// One value without list syntax.
pub fn item(text: &str, flavor: Flavor) -> Vec<Span<'static>> {
    if text.is_empty() {
        return vec![];
    }
    match flavor {
        Flavor::Number => vec![styled(text, NUMBER)],
        Flavor::Action => action(text),
        Flavor::Geosite => geosite(text),
        Flavor::Pattern => marks(text, &['*'], MARK),
        Flavor::Regex => regex(text),
        Flavor::Auto => auto(text),
    }
}

fn action(text: &str) -> Vec<Span<'static>> {
    match text {
        "direct" | "DIRECT" => vec![styled(text, Color::Green)],
        "block" | "reject" | "REJECT" => vec![styled(text, Color::Red)],
        _ => match text.strip_prefix("proxy:") {
            Some(name) => vec![
                styled("proxy:", PUNCT),
                Span::styled(
                    name.to_owned(),
                    Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::BOLD),
                ),
            ],
            None => vec![Span::raw(text.to_owned())],
        },
    }
}

fn geosite(text: &str) -> Vec<Span<'static>> {
    match text.split_once('@') {
        Some((name, attribute)) => vec![
            Span::raw(name.to_owned()),
            styled(format!("@{attribute}"), MARK),
        ],
        None => vec![Span::raw(text.to_owned())],
    }
}

fn regex(text: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        let meta = match c {
            '\\' => Some(format!(
                "\\{}",
                chars.next().map(String::from).unwrap_or_default()
            )),
            '^' | '$' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' => {
                Some(c.to_string())
            }
            _ => None,
        };
        match meta {
            Some(meta) => {
                if !plain.is_empty() {
                    out.push(Span::raw(std::mem::take(&mut plain)));
                }
                out.push(styled(meta, MARK));
            }
            None => plain.push(c),
        }
    }
    if !plain.is_empty() {
        out.push(Span::raw(plain));
    }
    out
}

/// Plain text with the given characters colored.
fn marks(text: &str, chars: &[char], color: Color) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut plain = String::new();
    for c in text.chars() {
        if chars.contains(&c) {
            if !plain.is_empty() {
                out.push(Span::raw(std::mem::take(&mut plain)));
            }
            out.push(styled(c.to_string(), color));
        } else {
            plain.push(c);
        }
    }
    if !plain.is_empty() {
        out.push(Span::raw(plain));
    }
    out
}

fn host(host: &str) -> Span<'static> {
    if host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        styled(host, ADDRESS)
    } else {
        Span::raw(host.to_owned())
    }
}

/// host, host:port, IP, [IPv6]:port.
fn address(text: &str) -> Option<Vec<Span<'static>>> {
    if text.parse::<IpAddr>().is_ok() {
        return Some(vec![styled(text, ADDRESS)]);
    }
    let (left, port) = text.rsplit_once(':')?;
    let bracketed = left.starts_with('[') && left.ends_with(']');
    if port.is_empty()
        || !port.bytes().all(|b| b.is_ascii_digit())
        || left.is_empty()
        || (left.contains(':') && !bracketed)
        || left.contains(['/', ' '])
    {
        return None;
    }
    Some(vec![host(left), styled(":", PUNCT), styled(port, NUMBER)])
}

fn auto(text: &str) -> Vec<Span<'static>> {
    if let Some((scheme, rest)) = text.split_once("://") {
        if !scheme.is_empty()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+')
        {
            let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let (authority, tail) = rest.split_at(split);
            let mut out = vec![styled(format!("{scheme}://"), MARK)];
            // Credentials before @ stay plain; the host is the part people check.
            let (credentials, authority) = match authority.rsplit_once('@') {
                Some((user, host)) => (Some(user), host),
                None => (None, authority),
            };
            if let Some(user) = credentials {
                out.push(Span::raw(format!("{user}@")));
            }
            out.extend(address(authority).unwrap_or_else(|| {
                vec![Span::styled(
                    authority.to_owned(),
                    Style::default().add_modifier(Modifier::BOLD),
                )]
            }));
            if !tail.is_empty() {
                out.push(styled(tail, PUNCT));
            }
            return out;
        }
    }
    if let Some((ip, prefix)) = text.split_once('/') {
        if ip.parse::<IpAddr>().is_ok() && prefix.bytes().all(|b| b.is_ascii_digit()) {
            return vec![
                styled(ip, ADDRESS),
                styled("/", PUNCT),
                styled(prefix, NUMBER),
            ];
        }
    }
    if let Some(spans) = address(text) {
        return spans;
    }
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return vec![styled(text, NUMBER)];
    }
    for prefix in ["+.", "*.", "."] {
        if let Some(rest) = text.strip_prefix(prefix) {
            if !rest.is_empty() && !rest.contains(['/', ' ', '\\']) {
                return vec![styled(prefix, MARK), Span::raw(rest.to_owned())];
            }
        }
    }
    if text.contains(['\\', '/']) && !text.contains(' ') {
        // File paths: separators recede so the file name stands out.
        return marks(text, &['\\', '/'], PUNCT);
    }
    vec![Span::raw(text.to_owned())]
}

/// Inserts the text cursor `▏` before the `cursor`-th character.
pub fn with_cursor(spans: Vec<Span<'static>>, cursor: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + 2);
    let mut seen = 0;
    let mut placed = false;
    for span in spans {
        let count = span.content.chars().count();
        if !placed && cursor <= seen + count {
            let at = cursor - seen;
            let (before, after): (String, String) = (
                span.content.chars().take(at).collect(),
                span.content.chars().skip(at).collect(),
            );
            if !before.is_empty() {
                out.push(Span::styled(before, span.style));
            }
            out.push(Span::raw("▏"));
            if !after.is_empty() {
                out.push(Span::styled(after, span.style));
            }
            placed = true;
        } else {
            out.push(span);
        }
        seen += count;
    }
    if !placed {
        out.push(Span::raw("▏"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn colored(spans: &[Span]) -> Vec<(String, Option<Color>)> {
        spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    }
    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }
    #[test]
    fn lists_keep_their_text_and_color_each_item() {
        let source = r#"["10.0.0.0/8", "+.example.com", "fc00::/7"]"#;
        let list = spans(source, Flavor::Auto);
        assert_eq!(text(&list), source);
        let colors = colored(&list);
        assert!(colors.contains(&("10.0.0.0".into(), Some(ADDRESS))));
        assert!(colors.contains(&("8".into(), Some(NUMBER))));
        assert!(colors.contains(&("+.".into(), Some(MARK))));
        assert!(colors.contains(&("fc00::".into(), Some(ADDRESS))));
        assert!(colors.contains(&("[".into(), Some(PUNCT))));
        // Unfinished input while typing still renders every character.
        let partial = r#"["a.example", "b.exa"#;
        assert_eq!(text(&spans(partial, Flavor::Auto)), partial);
        // Trailing TOML comments are dimmed and never parsed as data.
        let commented = r#"[]    # e.g. ["127.0.0.0/8"]"#;
        let colors = colored(&spans(commented, Flavor::Auto));
        assert_eq!(colors.last().unwrap().1, Some(Color::DarkGray));
        assert!(!colors.iter().any(|(_, c)| *c == Some(ADDRESS)));
        let number = colored(&spans("8      # 1..128", Flavor::Number));
        assert_eq!(number[0].1, Some(NUMBER));
        assert_eq!(number[1], ("# 1..128".into(), Some(Color::DarkGray)));
        // A # inside plain strings (URLs, regex) stays part of the value.
        assert_eq!(
            colored(&spans("https://a.example/#x", Flavor::Auto))[0].1,
            Some(MARK)
        );
        assert!(!colored(&spans("a #b", Flavor::Regex))
            .iter()
            .any(|(_, c)| *c == Some(Color::DarkGray)));
    }
    #[test]
    fn addresses_urls_actions_and_matchers() {
        assert_eq!(
            colored(&item("127.0.0.1:7890", Flavor::Auto)),
            [
                ("127.0.0.1".into(), Some(ADDRESS)),
                (":".into(), Some(PUNCT)),
                ("7890".into(), Some(NUMBER))
            ]
        );
        assert_eq!(text(&spans("[::1]:443", Flavor::Auto)), "[::1]:443");
        assert_eq!(
            colored(&spans("[::1]:443", Flavor::Auto))[0].1,
            Some(ADDRESS)
        );
        let url = item(
            "https://user@sub.example.net:8443/api?token=1",
            Flavor::Auto,
        );
        assert_eq!(text(&url), "https://user@sub.example.net:8443/api?token=1");
        assert_eq!(colored(&url)[0], ("https://".into(), Some(MARK)));
        assert!(colored(&url).contains(&("8443".into(), Some(NUMBER))));
        assert_eq!(colored(&action("direct"))[0].1, Some(Color::Green));
        assert_eq!(colored(&action("block"))[0].1, Some(Color::Red));
        assert_eq!(text(&action("proxy:PROXY")), "proxy:PROXY");
        assert_eq!(
            colored(&geosite("cn@!ads"))[1],
            ("@!ads".into(), Some(MARK))
        );
        assert_eq!(
            colored(&regex(r"^ad\.example$"))
                .iter()
                .filter(|(_, color)| *color == Some(MARK))
                .count(),
            3
        );
        assert_eq!(text(&marks("*.cdn.*", &['*'], MARK)), "*.cdn.*");
        let path = item(r"C:\Program Files\Browser\browser.exe", Flavor::Auto);
        assert_eq!(text(&path), r"C:\Program Files\Browser\browser.exe");
        assert_eq!(flavor("value", false, Some("domain_regex")), Flavor::Regex);
        assert_eq!(flavor("default_action", false, None), Flavor::Action);
        assert_eq!(flavor("port", true, None), Flavor::Number);
    }
    #[test]
    fn cursor_lands_between_characters_of_any_span() {
        let spans = item("127.0.0.1:80", Flavor::Auto);
        for cursor in 0..=12 {
            let with = with_cursor(spans.clone(), cursor);
            let text = text(&with);
            assert_eq!(text.chars().position(|c| c == '▏'), Some(cursor));
            assert_eq!(text.replace('▏', ""), "127.0.0.1:80");
        }
        assert_eq!(text(&with_cursor(vec![], 0)), "▏");
    }
}
