//! Lexical highlighting for single field values: lists, addresses, URLs, rule
//! matchers and actions. It never validates; half-typed input still colors sensibly.
use std::net::IpAddr;
/// Semantic categories; renderers supply their own palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TokenKind {
    #[default]
    Plain,
    Punctuation,
    Number,
    Address,
    Mark,
    Comment,
    Direct,
    Block,
    Proxy,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenStyle {
    pub kind: TokenKind,
    pub strong: bool,
}
impl TokenStyle {
    pub fn kind(mut self, kind: TokenKind) -> Self {
        self.kind = kind;
        self
    }
    pub fn strong(mut self) -> Self {
        self.strong = true;
        self
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub content: String,
    pub style: TokenStyle,
}
impl Token {
    pub fn raw(content: impl Into<String>) -> Self {
        Self::styled(content, TokenStyle::default())
    }
    pub fn styled(content: impl Into<String>, style: TokenStyle) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }
}

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

const PUNCT: TokenKind = TokenKind::Punctuation;
const NUMBER: TokenKind = TokenKind::Number;
const ADDRESS: TokenKind = TokenKind::Address;
const MARK: TokenKind = TokenKind::Mark;

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
pub fn spans(text: &str, flavor: Flavor) -> Vec<Token> {
    let (value, comment) = split_comment(text);
    let mut out = if value.trim_start().starts_with('[') && !looks_like_bracketed_ipv6(value) {
        list(value, flavor)
    } else {
        item(value, flavor)
    };
    if let Some(comment) = comment {
        out.push(styled(comment, TokenKind::Comment));
    }
    out
}

/// Non-string TOML values keep their trailing `# comment`; it is shown dimmed and
/// never highlighted as data. Strings never carry one, so `#` inside them is kept.
pub fn split_comment(text: &str) -> (&str, Option<&str>) {
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

pub fn looks_like_bracketed_ipv6(text: &str) -> bool {
    let text = text.trim();
    text.find(']').is_some_and(|end| {
        text[1..end].parse::<std::net::Ipv6Addr>().is_ok() && !text[..end].contains('"')
    })
}

pub fn styled(text: impl Into<String>, color: TokenKind) -> Token {
    Token::styled(text.into(), TokenStyle::default().kind(color))
}

pub fn list(text: &str, flavor: Flavor) -> Vec<Token> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    let flush = |plain: &mut String, out: &mut Vec<Token>| {
        if !plain.is_empty() {
            out.push(Token::raw(std::mem::take(plain)));
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
pub fn item(text: &str, flavor: Flavor) -> Vec<Token> {
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

pub fn action(text: &str) -> Vec<Token> {
    match text {
        "direct" | "DIRECT" => vec![styled(text, TokenKind::Direct)],
        "block" | "reject" | "REJECT" => vec![styled(text, TokenKind::Block)],
        _ => match text.strip_prefix("proxy:") {
            Some(name) => vec![
                styled("proxy:", PUNCT),
                Token::styled(
                    name.to_owned(),
                    TokenStyle::default().kind(TokenKind::Proxy).strong(),
                ),
            ],
            None => vec![Token::raw(text.to_owned())],
        },
    }
}

pub fn geosite(text: &str) -> Vec<Token> {
    match text.split_once('@') {
        Some((name, attribute)) => vec![
            Token::raw(name.to_owned()),
            styled(format!("@{attribute}"), MARK),
        ],
        None => vec![Token::raw(text.to_owned())],
    }
}

pub fn regex(text: &str) -> Vec<Token> {
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
                    out.push(Token::raw(std::mem::take(&mut plain)));
                }
                out.push(styled(meta, MARK));
            }
            None => plain.push(c),
        }
    }
    if !plain.is_empty() {
        out.push(Token::raw(plain));
    }
    out
}

/// Plain text with the given characters colored.
pub fn marks(text: &str, chars: &[char], color: TokenKind) -> Vec<Token> {
    let mut out = Vec::new();
    let mut plain = String::new();
    for c in text.chars() {
        if chars.contains(&c) {
            if !plain.is_empty() {
                out.push(Token::raw(std::mem::take(&mut plain)));
            }
            out.push(styled(c.to_string(), color));
        } else {
            plain.push(c);
        }
    }
    if !plain.is_empty() {
        out.push(Token::raw(plain));
    }
    out
}

pub fn host(host: &str) -> Token {
    if host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        styled(host, ADDRESS)
    } else {
        Token::raw(host.to_owned())
    }
}

/// host, host:port, IP, [IPv6]:port.
pub fn address(text: &str) -> Option<Vec<Token>> {
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

pub fn auto(text: &str) -> Vec<Token> {
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
                out.push(Token::raw(format!("{user}@")));
            }
            out.extend(address(authority).unwrap_or_else(|| {
                vec![Token::styled(
                    authority.to_owned(),
                    TokenStyle::default().strong(),
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
                return vec![styled(prefix, MARK), Token::raw(rest.to_owned())];
            }
        }
    }
    if text.contains(['\\', '/']) && !text.contains(' ') {
        // File paths: separators recede so the file name stands out.
        return marks(text, &['\\', '/'], PUNCT);
    }
    vec![Token::raw(text.to_owned())]
}

/// Inserts the text cursor `▏` before the `cursor`-th character.
pub fn with_cursor(spans: Vec<Token>, cursor: usize) -> Vec<Token> {
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
                out.push(Token::styled(before, span.style));
            }
            out.push(Token::raw("▏"));
            if !after.is_empty() {
                out.push(Token::styled(after, span.style));
            }
            placed = true;
        } else {
            out.push(span);
        }
        seen += count;
    }
    if !placed {
        out.push(Token::raw("▏"));
    }
    out
}

#[cfg(test)]
#[path = "test/highlight_test.rs"]
mod tests;
