use super::chart::{plot, ChartSeries};
use gpui::{px, App, Hsla, IntoElement, Pixels, RenderOnce, Window};

#[derive(IntoElement)]
pub struct Sparkline {
    series: ChartSeries,
    height: Pixels,
}
impl Sparkline {
    pub fn new(samples: &[u64], color: Hsla) -> Self {
        Self {
            series: ChartSeries::new(
                "",
                &samples
                    .iter()
                    .map(|value| Some(*value as f64))
                    .collect::<Vec<_>>(),
                color,
            ),
            height: px(40.),
        }
    }
    pub fn height(mut self, height: Pixels) -> Self {
        self.height = height;
        self
    }
}
impl RenderOnce for Sparkline {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        plot(vec![self.series], self.height, None)
    }
}
