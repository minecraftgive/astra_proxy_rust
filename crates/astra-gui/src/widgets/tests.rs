use super::{
    chart::normalized_segments, Card, Chart, ChartSeries, DelayChip, Empty, Sparkline, Status,
    StatusDot, ToastHost, ToastKind, TokenText,
};
use astra_client::highlight::{Token, TokenKind, TokenStyle};
use gpui::{
    div, px, AppContext, Context, FontWeight, IntoElement, ParentElement, Render, Styled,
    TestAppContext, Window,
};
use gpui_component::ActiveTheme;
use std::time::Duration;

#[test]
fn plot_preserves_missing_sample_gaps_and_clamps_invalid_values() {
    let segments = normalized_segments(
        &[
            None,
            Some(-1.),
            Some(0.),
            Some(f64::NAN),
            Some(10.),
            Some(f64::INFINITY),
        ],
        10.,
    );
    assert_eq!(segments, vec![vec![[0.2, 1.], [0.4, 1.]], vec![[0.8, 0.]]]);
    assert!(normalized_segments(&[], 0.).is_empty());
    assert_eq!(normalized_segments(&[Some(0.)], 0.), vec![vec![[0., 1.]]]);
    assert_eq!(
        normalized_segments(&[Some(5.), Some(5.)], 5.),
        vec![vec![[0., 0.], [1., 0.]]]
    );
}

#[gpui::test]
fn toast_expires_five_seconds_after_latest_message(cx: &mut TestAppContext) {
    let host = cx.new(|_| ToastHost::default());
    host.update(cx, |host, cx| host.show(ToastKind::Error, "first", cx));
    cx.run_until_parked();
    cx.background_executor.advance_clock(Duration::from_secs(4));
    cx.run_until_parked();
    assert!(host.read_with(cx, |host, _| host.is_visible()));
    host.update(cx, |host, cx| {
        host.show(ToastKind::Error, "replacement", cx)
    });
    cx.run_until_parked();
    cx.background_executor.advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(host.read_with(cx, |host, _| host.is_visible()));
    cx.background_executor.advance_clock(Duration::from_secs(4));
    cx.run_until_parked();
    assert!(!host.read_with(cx, |host, _| host.is_visible()));
}

struct Gallery;
impl Render for Gallery {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let primary = cx.theme().primary;
        div()
            .w(px(600.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                Card::new("Status")
                    .child(StatusDot::new(Status::Running))
                    .child(DelayChip::new(Some(250))),
            )
            .child(Empty::new("Empty", "No content"))
            .child(TokenText::new([Token::styled(
                "出口🚀",
                TokenStyle::default().kind(TokenKind::Proxy).strong(),
            )]))
            .child(Sparkline::new(&[0, 10, 5, 0], primary))
            .child(Chart::new([ChartSeries::new(
                "Traffic",
                &[Some(0.), None, Some(10.)],
                primary,
            )]))
    }
}

#[gpui::test]
fn components_render_and_unicode_token_runs_use_bytes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_window(|_, _| Gallery);
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            let source = "出口🚀";
            let tokens = TokenText::new([
                Token::raw(""),
                Token::styled(
                    source,
                    TokenStyle::default().kind(TokenKind::Proxy).strong(),
                ),
                Token::raw(" direct"),
            ]);
            let (text, runs) = tokens.runs(window, cx);
            assert_eq!(text, format!("{source} direct"));
            assert_eq!(runs[0].len, source.len());
            assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), text.len());
            assert_eq!(runs[0].font.weight, FontWeight::BOLD);
            assert_eq!(runs[0].color, cx.theme().primary);
        })
        .unwrap();
}
