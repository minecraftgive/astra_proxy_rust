//! A bounded banner animation; its visual choices come from shared preferences.
use super::ViewEvent;
use crate::model::AppModel;
use astra_client::{
    motion::MotionMode,
    preferences::{ColorDepth, Preferences},
};
use gpui::{
    div, linear_color_stop, linear_gradient, prelude::*, px, App, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, Render, Subscription, Task, Window,
};
use gpui_component::ActiveTheme;
use std::time::{Duration, Instant};

pub struct MarqueeView {
    model: Entity<AppModel>,
    focus: FocusHandle,
    started: Instant,
    _timer: Task<()>,
    _observations: Vec<Subscription>,
}
impl EventEmitter<ViewEvent> for MarqueeView {}
impl Focusable for MarqueeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl MarqueeView {
    pub fn new(model: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&model, |_, _, cx| cx.notify());
        let timer = cx.spawn(async move |view, cx| loop {
            let frame = view.read_with(cx, |view, cx| {
                let p = &view.model.read(cx).client.preferences;
                if p.marquee.effect().any() && p.motion == MotionMode::Normal {
                    Duration::from_secs_f64(1. / f64::from(p.marquee.effect().fps.max(1)))
                } else {
                    Duration::from_millis(500)
                }
            });
            let Ok(frame) = frame else {
                break;
            };
            cx.background_executor().timer(frame).await;
            if view
                .update(cx, |view, cx| {
                    let p = &view.model.read(cx).client.preferences;
                    if p.marquee.effect().any() && p.motion == MotionMode::Normal {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        });
        Self {
            model,
            focus: cx.focus_handle(),
            started: Instant::now(),
            _timer: timer,
            _observations: vec![observation],
        }
    }
    pub fn visible(preferences: &Preferences) -> bool {
        preferences.marquee.effect().any()
    }
}
fn color(preferences: &Preferences, hue: f32) -> Hsla {
    let depth = if preferences.marquee.true_color {
        ColorDepth::TrueColor
    } else {
        preferences.colors
    };
    let hue = hue.rem_euclid(360.) / 360.;
    match depth {
        ColorDepth::Monochrome => Hsla {
            h: 0.,
            s: 0.,
            l: 0.65,
            a: 1.,
        },
        ColorDepth::Ansi16 => Hsla {
            h: (hue * 6.).floor() / 6.,
            s: 0.9,
            l: 0.55,
            a: 1.,
        },
        ColorDepth::Ansi256 => Hsla {
            h: (hue * 36.).floor() / 36.,
            s: 0.8,
            l: 0.6,
            a: 1.,
        },
        _ => Hsla {
            h: hue,
            s: 0.8,
            l: 0.6,
            a: 1.,
        },
    }
}
impl Render for MarqueeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let preferences = &model.client.preferences;
        if !Self::visible(preferences) {
            return div().into_any_element();
        }
        let effect = preferences.marquee.effect();
        let seconds = if preferences.motion == MotionMode::Normal {
            self.started.elapsed().as_secs_f32()
        } else {
            0.
        };
        let hue = seconds * effect.speed;
        let first = color(preferences, hue);
        let last = color(preferences, hue + 360. * 48. / effect.width.max(1.));
        let mut banner = div()
            .id("marquee-banner")
            .track_focus(&self.focus)
            .h(px(38.))
            .relative()
            .overflow_hidden()
            .flex()
            .items_center()
            .px_4()
            .gap_2();
        if effect.background {
            banner = banner.bg(linear_gradient(
                90.,
                linear_color_stop(first.opacity(0.2), 0.),
                linear_color_stop(last.opacity(0.2), 1.),
            ));
        }
        if effect.border {
            banner = banner.border_y_2().border_color(first);
        }
        if effect.indicator && model.online {
            banner = banner.child(div().text_color(first).child("●"));
        }
        let message = if effect.status && !model.client.message.is_empty() {
            astra_client::i18n::t(&model.client.message).to_owned()
        } else {
            "Astra".into()
        };
        let text: String = message.chars().take(96).collect();
        let foreground = cx.theme().foreground;
        let mut letters = div().flex().items_center().gap(px(1.));
        for (index, symbol) in text.chars().enumerate() {
            let tint = if effect.text || effect.status {
                color(
                    preferences,
                    hue + index as f32 * 360. / effect.width.max(1.),
                )
            } else {
                foreground
            };
            letters = letters.child(div().text_color(tint).child(symbol.to_string()));
        }
        let movement = if preferences.motion == MotionMode::Normal {
            (seconds * effect.speed / 6.).rem_euclid(80.)
        } else {
            0.
        };
        banner
            .child(div().relative().left(px(movement)).child(letters))
            .into_any_element()
    }
}
