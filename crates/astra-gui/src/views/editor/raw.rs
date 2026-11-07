//! Native code editor with the shared lexer and parser-owned error spans.
use astra_client::{
    editor::{Editor, SourceError},
    highlight::TokenKind,
};
use gpui::{App, AppContext, Context, Entity, FontWeight, HighlightStyle, SharedString, Window};
use gpui_component::{
    input::{
        EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
        RangeDecoration, RangeDecorationCollection, Rope,
    },
    ActiveTheme,
};
use std::{ops::Range, rc::Rc};

#[derive(Default)]
struct SharedHighlighter {
    runs: Vec<(Range<usize>, HighlightStyle)>,
}
impl InputHighlighter for SharedHighlighter {
    fn language(&self) -> SharedString {
        "toml".into()
    }
    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let mut start = 0;
        self.runs = Editor::source_tokens(&text.to_string())
            .into_iter()
            .map(|token| {
                let range = start..start + token.content.len();
                start = range.end;
                let color = match token.style.kind {
                    TokenKind::Plain => cx.theme().foreground,
                    TokenKind::Comment | TokenKind::Punctuation => cx.theme().muted_foreground,
                    TokenKind::Number => cx.theme().warning,
                    TokenKind::Proxy | TokenKind::Address => cx.theme().primary,
                    TokenKind::Mark => cx.theme().info,
                    TokenKind::Direct => cx.theme().success,
                    TokenKind::Block => cx.theme().danger,
                };
                (
                    range,
                    HighlightStyle {
                        color: Some(color),
                        font_weight: token.style.strong.then_some(FontWeight::BOLD),
                        ..Default::default()
                    },
                )
            })
            .collect();
    }
    fn styles(
        &self,
        range: &Range<usize>,
        _: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        self.runs
            .iter()
            .filter_map(|(span, style)| {
                let clipped = span.start.max(range.start)..span.end.min(range.end);
                (!clipped.is_empty()).then_some((clipped, *style))
            })
            .collect()
    }
    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}
pub(super) fn create(
    window: &mut Window,
    cx: &mut App,
) -> (Entity<EditorState>, RangeDecorationCollection) {
    let state = cx.new(|cx| {
        let mut state = EditorState::new(window, cx)
            .language("toml")
            .line_number(true);
        state.set_highlighter_factory(Rc::new(|_| Some(Box::<SharedHighlighter>::default())), cx);
        state.set_highlighter("toml", cx);
        state
    });
    let decorations = state.update(cx, |state, cx| {
        state.create_range_decorations_collection(Vec::new(), cx)
    });
    (state, decorations)
}
pub(super) fn show_error(
    error: Option<&SourceError>,
    decorations: &RangeDecorationCollection,
    cx: &mut App,
) {
    decorations.set(
        error
            .and_then(|error| error.span.clone())
            .map(|span| vec![RangeDecoration::new(span).with_color(cx.theme().danger)])
            .unwrap_or_default(),
        cx,
    );
}
