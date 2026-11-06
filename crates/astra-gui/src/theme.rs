use astra_client::preferences::{Accent, ColorDepth, Preferences};
use gpui::{rgb, App};
use gpui_component::{Theme, ThemeMode};
pub fn apply(preferences: &Preferences, cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let color = match preferences.accent {
        Accent::Indigo => 0x818cf8,
        Accent::Teal => 0x2dd4bf,
        Accent::Green => 0x4ade80,
        Accent::Violet => 0xa78bfa,
        Accent::Rose => 0xfb7185,
        Accent::Amber => 0xfbbf24,
    };
    let mut primary: gpui::Hsla = rgb(color).into();
    primary.s *= match preferences.colors {
        ColorDepth::Monochrome => 0.,
        ColorDepth::Ansi16 => 0.45,
        ColorDepth::Ansi256 => 0.75,
        _ => 1.,
    };
    Theme::update(cx, |theme| {
        theme.colors.primary = primary;
        theme.colors.ring = primary;
        theme.colors.button_primary = primary;
        theme.colors.button_primary_foreground = rgb(0x0f172a).into();
        let mut hover = primary;
        hover.l = (hover.l + 0.06).min(1.);
        let mut active = primary;
        active.l = (active.l - 0.06).max(0.);
        theme.colors.button_primary_hover = hover;
        theme.colors.button_primary_active = active;
        if preferences.motion != astra_client::motion::MotionMode::Normal {
            let duration = if preferences.motion == astra_client::motion::MotionMode::Off {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_millis(60)
            };
            theme.motion.duration_fast = duration;
            theme.motion.duration_normal = duration;
            theme.motion.duration_slow = duration;
        }
    });
}
