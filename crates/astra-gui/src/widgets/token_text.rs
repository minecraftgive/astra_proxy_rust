use astra_client::highlight::{Token, TokenKind};
use gpui::{App, FontWeight, IntoElement, RenderOnce, StyledText, TextRun, Window};
use gpui_component::ActiveTheme;

#[derive(IntoElement)]
pub struct TokenText {
    tokens: Vec<Token>,
}
impl TokenText {
    pub fn new(tokens: impl IntoIterator<Item = Token>) -> Self {
        Self {
            tokens: tokens.into_iter().collect(),
        }
    }
    pub(super) fn runs(&self, window: &Window, cx: &App) -> (String, Vec<TextRun>) {
        let mut text = String::new();
        let mut runs = Vec::with_capacity(self.tokens.len());
        for token in &self.tokens {
            if token.content.is_empty() {
                continue;
            }
            let mut run = window.text_style().to_run(token.content.len());
            run.color = match token.style.kind {
                TokenKind::Plain => cx.theme().foreground,
                TokenKind::Punctuation | TokenKind::Comment => cx.theme().muted_foreground,
                TokenKind::Number => cx.theme().warning,
                TokenKind::Address | TokenKind::Proxy => cx.theme().primary,
                TokenKind::Mark => cx.theme().info,
                TokenKind::Direct => cx.theme().success,
                TokenKind::Block => cx.theme().danger,
            };
            if token.style.strong {
                run.font.weight = FontWeight::BOLD;
            }
            text.push_str(&token.content);
            runs.push(run);
        }
        (text, runs)
    }
}
impl RenderOnce for TokenText {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (text, runs) = self.runs(window, cx);
        StyledText::new(text).with_runs(runs)
    }
}
