//! Terminal wrapping and token presentation for shared program information.
use super::*;
use astra_client::highlight::TokenKind;
/// Wrap at grapheme boundaries using terminal cell widths. Keeping the physical
/// lines lets the same viewport scroll information and clickable preferences.
pub(super) fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut output = Vec::new();
    for line in lines {
        let mut spans = Vec::new();
        let mut used = 0;
        for grapheme in line.styled_graphemes(Style::default()) {
            let cells = astra_tui_kit::text::width(grapheme.symbol);
            if used > 0 && used + cells > width {
                output.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            spans.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            used += cells;
        }
        output.push(Line::from(spans));
    }
    output
}

impl Ui {
    pub(super) fn settings_program_info(&self, width: u16) -> Vec<Line<'static>> {
        let rows = self.client.settings_program_info(
            &astra_platform::build_info!(),
            Some(self.animations.detected_colors()),
        );
        let lines = rows
            .into_iter()
            .map(|tokens| {
                Line::from(
                    tokens
                        .into_iter()
                        .map(|token| {
                            let color = match token.style.kind {
                                TokenKind::Comment => Some(MUTED),
                                TokenKind::Direct => Some(Color::Green),
                                TokenKind::Number => Some(Color::Yellow),
                                _ => None,
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
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        wrap(lines, width)
    }
}
