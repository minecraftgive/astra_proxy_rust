//! Shared colors so every screen marks the same things the same way.
use ratatui::style::{Color, Modifier, Style};

pub const BLUE: Color = Color::Rgb(79, 91, 213);
pub const MUTED: Color = Color::Rgb(140, 147, 161);

/// Background of [`selected`] while selections are marked for a frame effect.
/// One step off [`BLUE`], so an unprocessed frame still looks the same.
pub const SELECTED_MARK: Color = Color::Rgb(79, 91, 212);
/// Background of [`selected_keep_colors`] while selections are marked.
pub const SELECTED_KEEP_MARK: Color = Color::Rgb(24, 28, 33);
static MARK_SELECTION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Mark cursor rows with [`SELECTED_MARK`] / [`SELECTED_KEEP_MARK`] so a frame
/// post-processor can find them. The post-processor maps marks back otherwise.
pub fn set_selection_mark(on: bool) {
    MARK_SELECTION.store(on, std::sync::atomic::Ordering::Relaxed);
}
fn marking() -> bool {
    MARK_SELECTION.load(std::sync::atomic::Ordering::Relaxed)
}

/// The row, tab or choice under the cursor; white on blue reads on every background.
pub fn selected() -> Style {
    Style::default()
        .bg(if marking() { SELECTED_MARK } else { BLUE })
        .fg(Color::White)
}
/// Cursor row whose spans carry meaning in their colors (highlighted values).
/// A neutral background keeps blue proxy labels distinct from the selection.
/// Leave the foreground unset: list highlights are applied after span styles.
pub fn selected_keep_colors() -> Style {
    Style::default().bg(if marking() {
        SELECTED_KEEP_MARK
    } else {
        Color::Rgb(24, 28, 34)
    })
}
/// A focused pane's cursor when keyboard focus has moved to a neighbouring pane.
pub fn selected_inactive() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}
/// Text matching the current search or filter.
pub fn matched() -> Style {
    Style::default().bg(Color::Yellow).fg(Color::Black)
}
/// Key hints such as "c connect · Enter actions · Ctrl-K commands": keys in the accent
/// color, their descriptions muted, separators dimmer still.
pub fn key_hints(text: &str) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    // A key is a single symbol, a modifier chord (Ctrl-K), Fn, arrows, a named key,
    // or alternatives of those (Space/c, x/X); "connect/disconnect" is a description.
    fn is_key(token: &str) -> bool {
        const NAMES: &[&str] = &[
            "Enter",
            "Esc",
            "Space",
            "Tab",
            "Backspace",
            "Home",
            "End",
            "PgUp",
            "PgDn",
            "空格",
            "空白鍵",
        ];
        if token.chars().count() == 1 {
            return true;
        }
        if token.contains('/') {
            return token
                .split('/')
                .all(|part| !part.is_empty() && is_key(part));
        }
        ["Ctrl-", "Alt-", "Alt+", "Shift-"]
            .iter()
            .any(|modifier| token.starts_with(modifier))
            || token.chars().all(|c| "↑↓←→".contains(c))
            || (token.starts_with('F') && token[1..].chars().all(|c| c.is_ascii_digit()))
            || NAMES.contains(&token)
    }
    let key = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let description = Style::default().fg(MUTED);
    let mut spans = Vec::new();
    let leading = text.len() - text.trim_start().len();
    if leading > 0 {
        spans.push(Span::raw(text[..leading].to_owned()));
    }
    for (index, segment) in text.trim_start().split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        let tokens: Vec<&str> = segment.split(' ').collect();
        let keys = tokens
            .iter()
            .take_while(|t| !t.is_empty() && is_key(t))
            .count();
        if keys > 0 {
            spans.push(Span::styled(tokens[..keys].join(" "), key));
        }
        if keys < tokens.len() {
            let rest = tokens[keys..].join(" ");
            spans.push(Span::styled(
                if keys > 0 { format!(" {rest}") } else { rest },
                description,
            ));
        }
    }
    spans
}
/// Key hints that fit `width` columns: whole entries only, ending in `more`
/// ("? more") when some had to be left out, so no hint is cut mid-word.
pub fn key_hints_fit(text: &str, width: usize, more: &str) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Line;
    let columns = |s: &str| Line::from(s).width();
    if columns(text) <= width {
        return key_hints(text);
    }
    let leading = &text[..text.len() - text.trim_start().len()];
    let entries: Vec<&str> = text.trim_start().split(" · ").collect();
    let mut kept = leading.to_owned();
    for entry in entries {
        let candidate = if kept.trim().is_empty() {
            format!("{leading}{entry}")
        } else {
            format!("{kept} · {entry}")
        };
        if columns(&candidate) + columns(" · ") + columns(more) > width {
            break;
        }
        kept = candidate;
    }
    let shown = if kept.trim().is_empty() {
        format!("{leading}{more}")
    } else {
        format!("{kept} · {more}")
    };
    if columns(&shown) > width {
        return key_hints(&crate::text::truncate_cells(&shown, width, ""));
    }
    key_hints(&shown)
}
/// Splits `text` into spans, marking every case-insensitive occurrence of `query`.
pub fn mark_matches(text: &str, query: &str, base: Style) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    let lower = text.to_lowercase();
    let query = query.to_lowercase();
    // Lowercasing can change byte offsets outside ASCII; then mark the whole line.
    if query.is_empty() || lower.len() != text.len() {
        return if !query.is_empty() && lower.contains(&query) {
            vec![Span::styled(text.to_owned(), matched())]
        } else {
            vec![Span::styled(text.to_owned(), base)]
        };
    }
    let mut spans = Vec::new();
    let mut at = 0;
    for (start, _) in lower.match_indices(&query) {
        if start < at
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(start + query.len())
        {
            continue;
        }
        if start > at {
            spans.push(Span::styled(text[at..start].to_owned(), base));
        }
        spans.push(Span::styled(
            text[start..start + query.len()].to_owned(),
            matched(),
        ));
        at = start + query.len();
    }
    if at < text.len() || spans.is_empty() {
        spans.push(Span::styled(text[at..].to_owned(), base));
    }
    spans
}

#[cfg(test)]
#[path = "test/theme_test.rs"]
mod tests;
