use astra_client::i18n::t;
use gpui::{
    canvas, div, point, px, App, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, RenderOnce,
    SharedString, Styled, Window,
};
use gpui_component::ActiveTheme;
use std::sync::Arc;

#[derive(Clone)]
pub struct ChartSeries {
    pub label: SharedString,
    pub values: Arc<[Option<f64>]>,
    pub color: Hsla,
}
impl ChartSeries {
    /// Missing or non-finite samples create a gap, rather than a false zero.
    pub fn new(label: impl Into<SharedString>, values: &[Option<f64>], color: Hsla) -> Self {
        Self {
            label: label.into(),
            values: values.into(),
            color,
        }
    }
}

#[derive(IntoElement)]
pub struct Chart {
    series: Vec<ChartSeries>,
    height: Pixels,
}
impl Chart {
    pub fn new(series: impl IntoIterator<Item = ChartSeries>) -> Self {
        Self {
            series: series.into_iter().collect(),
            height: px(160.),
        }
    }
    pub fn height(mut self, height: Pixels) -> Self {
        self.height = height;
        self
    }
}
impl RenderOnce for Chart {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let mut legend = div().flex().flex_wrap().gap_4().text_xs();
        for series in &self.series {
            legend = legend.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(7.)).rounded_full().bg(series.color))
                    .child(series.label.clone()),
            );
        }
        let has_samples = self.series.iter().any(|series| {
            series
                .values
                .iter()
                .flatten()
                .any(|value| value.is_finite())
        });
        let mut chart = div().flex().flex_col().min_w_0().gap_2().child(legend);
        if has_samples {
            chart = chart.child(plot(self.series, self.height, Some(cx.theme().chart_grid)));
        } else {
            chart = chart.child(
                div()
                    .h(self.height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("No samples yet")),
            );
        }
        chart
    }
}

pub(super) fn plot(
    series: Vec<ChartSeries>,
    height: Pixels,
    grid: Option<Hsla>,
) -> impl IntoElement {
    let maximum = series
        .iter()
        .flat_map(|series| series.values.iter())
        .flatten()
        .filter(|value| value.is_finite())
        .copied()
        .fold(0_f64, f64::max)
        .max(1.);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            // Keep the stroke within the canvas even when a sample hits an edge.
            let origin = bounds.origin + point(px(1.), px(1.));
            let width = (bounds.size.width - px(2.)).max(px(0.));
            let height = (bounds.size.height - px(2.)).max(px(0.));
            if let Some(color) = grid {
                let mut path = PathBuilder::stroke(px(1.));
                for fraction in [0., 0.5, 1.] {
                    path.move_to(origin + point(px(0.), height * fraction));
                    path.line_to(origin + point(width, height * fraction));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
            for series in &series {
                for segment in normalized_segments(&series.values, maximum) {
                    let mut path = PathBuilder::stroke(px(1.5));
                    for (index, [x, y]) in segment.iter().enumerate() {
                        let position = origin + point(width * *x, height * *y);
                        if index == 0 {
                            path.move_to(position);
                        } else {
                            path.line_to(position);
                        }
                        if segment.len() == 1 {
                            path.line_to(position + point(px(1.), px(0.)));
                        }
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, series.color);
                    }
                }
            }
        },
    )
    .w_full()
    .h(height)
}

/// Geometry only. Histories and rate calculation belong to the shared client.
pub(super) fn normalized_segments(values: &[Option<f64>], maximum: f64) -> Vec<Vec<[f32; 2]>> {
    let denominator = values.len().saturating_sub(1).max(1) as f64;
    let maximum = if maximum.is_finite() {
        maximum.max(1.)
    } else {
        1.
    };
    let mut segments = vec![];
    let mut current = vec![];
    for (index, value) in values.iter().enumerate() {
        if let Some(value) = value.filter(|value| value.is_finite()) {
            current.push([
                (index as f64 / denominator) as f32,
                (1. - (value / maximum).clamp(0., 1.)) as f32,
            ]);
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}
