//! Lexical highlighting for single field values: lists, addresses, URLs, rule
//! matchers and actions. It never validates; half-typed input still colors sensibly.
#[cfg(test)]
use astra_client::highlight::flavor;
pub use astra_client::highlight::Flavor;
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
const PUNCT: Color = Color::Gray;
const NUMBER: Color = Color::Yellow;
const ADDRESS: Color = Color::Cyan;
const MARK: Color = Color::Magenta;
fn render(tokens: Vec<astra_client::highlight::Token>) -> Vec<Span<'static>> {
    use astra_client::highlight::TokenKind;
    tokens
        .into_iter()
        .map(|token| {
            let color = match token.style.kind {
                TokenKind::Plain => None,
                TokenKind::Punctuation => Some(PUNCT),
                TokenKind::Number => Some(NUMBER),
                TokenKind::Address => Some(ADDRESS),
                TokenKind::Mark => Some(MARK),
                TokenKind::Comment => Some(Color::DarkGray),
                TokenKind::Direct => Some(Color::Green),
                TokenKind::Block => Some(Color::Red),
                TokenKind::Proxy => Some(Color::LightBlue),
            };
            let mut style = Style::default();
            if let Some(color) = color {
                style = style.fg(color);
            }
            if token.style.strong {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(token.content, style)
        })
        .collect()
}
pub fn spans(text: &str, flavor: Flavor) -> Vec<Span<'static>> {
    render(astra_client::highlight::spans(text, flavor))
}
#[cfg(test)]
pub fn item(text: &str, flavor: Flavor) -> Vec<Span<'static>> {
    render(astra_client::highlight::item(text, flavor))
}
#[cfg(test)]
fn action(text: &str) -> Vec<Span<'static>> {
    render(astra_client::highlight::action(text))
}
#[cfg(test)]
fn geosite(text: &str) -> Vec<Span<'static>> {
    render(astra_client::highlight::geosite(text))
}
#[cfg(test)]
fn regex(text: &str) -> Vec<Span<'static>> {
    render(astra_client::highlight::regex(text))
}
#[cfg(test)]
fn marks(text: &str, chars: &[char], _: Color) -> Vec<Span<'static>> {
    render(astra_client::highlight::marks(
        text,
        chars,
        astra_client::highlight::TokenKind::Mark,
    ))
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
#[path = "test/highlight_test.rs"]
mod tests;
