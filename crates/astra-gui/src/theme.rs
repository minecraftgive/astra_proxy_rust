use astra_client::{
    motion::MotionMode,
    preferences::{Accent, ColorDepth, Preferences},
};
use gpui::{rgb, App, Hsla, Window};
use gpui_component::{MotionTokens, Theme, ThemeColor};
use std::time::Duration;

/// Rebuild from the system palette so switching color depth never leaves old colors behind.
pub fn apply(preferences: &Preferences, cx: &mut App) {
    Theme::sync_system_appearance(None, cx);
    apply_preferences(preferences, cx);
}

/// Use the window appearance on Linux, where it is more reliable than the app default.
/// Call this at window creation and from a retained appearance observation.
pub fn apply_for_window(preferences: &Preferences, window: &mut Window, cx: &mut App) {
    Theme::sync_system_appearance(Some(window), cx);
    apply_preferences(preferences, cx);
}

pub fn animations_enabled(preferences: &Preferences) -> bool {
    preferences.motion == MotionMode::Normal
}

fn apply_preferences(preferences: &Preferences, cx: &mut App) {
    // GPUI AnimationExt and the component library's transitions and springs all
    // consult this flag; changing token durations alone does not stop them.
    let animate = animations_enabled(preferences);
    cx.set_reduce_motion(!animate);
    Theme::update(cx, |theme| {
        let dark = theme.is_dark();
        let primary: Hsla = rgb(match (preferences.accent, dark) {
            (Accent::Indigo, true) => 0x818cf8,
            (Accent::Teal, true) => 0x2dd4bf,
            (Accent::Green, true) => 0x4ade80,
            (Accent::Violet, true) => 0xa78bfa,
            (Accent::Rose, true) => 0xfb7185,
            (Accent::Amber, true) => 0xfbbf24,
            (Accent::Indigo, false) => 0x4f46e5,
            (Accent::Teal, false) => 0x0f766e,
            (Accent::Green, false) => 0x15803d,
            (Accent::Violet, false) => 0x7c3aed,
            (Accent::Rose, false) => 0xbe123c,
            (Accent::Amber, false) => 0xb45309,
        })
        .into();
        let colors = &mut theme.colors;
        web_palette(colors, dark);
        colors.primary = primary;
        colors.ring = primary;
        colors.accent = primary;
        colors.primary_foreground = foreground_on(primary);
        colors.accent_foreground = colors.primary_foreground;
        colors.primary_hover = lightness(primary, if dark { 0.06 } else { -0.06 });
        colors.primary_active = lightness(primary, if dark { -0.06 } else { 0.06 });
        colors.button_primary = primary;
        colors.button_primary_foreground = colors.primary_foreground;
        colors.button_primary_hover = colors.primary_hover;
        colors.button_primary_active = colors.primary_active;
        colors.sidebar_primary = primary;
        colors.sidebar_primary_foreground = colors.primary_foreground;
        colors.link = primary;
        colors.caret = primary;
        colors.progress_bar = primary;
        colors.slider_bar = primary;
        match preferences.colors {
            ColorDepth::Monochrome => map_colors(colors, |mut color| {
                color.s = 0.;
                color
            }),
            ColorDepth::Ansi16 => {
                // A compact, high-contrast presentation, without imitating ANSI
                // escape colors or losing semantic success/warning/error hues.
                map_colors(colors, |mut color| {
                    color.l = if color.l < 0.5 {
                        color.l * 0.8
                    } else {
                        1. - (1. - color.l) * 0.8
                    };
                    if color.s > 0.1 {
                        color.s = color.s.max(0.8);
                    }
                    color
                });
                colors.background = rgb(if dark { 0x08090b } else { 0xffffff }).into();
                colors.foreground = rgb(if dark { 0xffffff } else { 0x08090b }).into();
                colors.muted_foreground = rgb(if dark { 0xd1d5db } else { 0x374151 }).into();
                colors.border = rgb(if dark { 0x9ca3af } else { 0x6b7280 }).into();
            }
            ColorDepth::Auto | ColorDepth::TrueColor | ColorDepth::Ansi256 => {}
        }
        // Depth conversion can change the best foreground for a filled control.
        colors.primary_foreground = foreground_on(colors.primary);
        colors.button_primary_foreground = foreground_on(colors.button_primary);
        colors.accent_foreground = foreground_on(colors.accent);
        colors.sidebar_primary_foreground = foreground_on(colors.sidebar_primary);
        theme.motion = MotionTokens::default();
        if !animate {
            theme.motion.duration_fast = Duration::ZERO;
            theme.motion.duration_normal = Duration::ZERO;
            theme.motion.duration_slow = Duration::ZERO;
        }
    });
}

/// The management web UI's neutral and semantic palette (apps/web/src/styles.css),
/// so both graphical frontends read as one product. Accent stays a preference.
fn web_palette(colors: &mut ThemeColor, dark: bool) {
    let pick = |light: u32, night: u32| -> Hsla { rgb(if dark { night } else { light }).into() };
    let bg = pick(0xf4f5f8, 0x13151b);
    let surface = pick(0xffffff, 0x1b1e26);
    let surface_2 = pick(0xeef0f4, 0x232733);
    let border = pick(0xe1e4ea, 0x2c3140);
    let text = pick(0x1a1d23, 0xe7e9ee);
    let muted = pick(0x667085, 0x9aa3b2);
    colors.background = bg;
    colors.foreground = text;
    colors.border = border;
    colors.input = border;
    colors.muted = surface_2;
    colors.muted_foreground = muted;
    colors.secondary = surface_2;
    colors.secondary_hover = border;
    colors.secondary_foreground = text;
    colors.popover = surface;
    colors.popover_foreground = text;
    colors.sidebar = surface;
    colors.sidebar_border = border;
    colors.sidebar_foreground = muted;
    colors.sidebar_accent = surface_2;
    colors.sidebar_accent_foreground = text;
    colors.list = surface;
    colors.list_even = surface;
    colors.list_head = surface_2;
    colors.list_hover = surface_2;
    colors.table = surface;
    colors.table_even = surface;
    colors.table_head = surface_2;
    colors.table_head_foreground = muted;
    colors.table_hover = surface_2;
    colors.table_row_border = border;
    colors.button = surface;
    colors.button_foreground = text;
    colors.button_hover = surface_2;
    colors.button_active = border;
    colors.success = pick(0x1f9d55, 0x3ccb7f);
    colors.warning = pick(0xb7790a, 0xe5b33e);
    colors.danger = pick(0xd64545, 0xf06a6a);
}

/// Surfaces and soft fills derived from the active theme, so color depth
/// preferences (mono, high contrast) reach every custom widget too.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub surface: Hsla,
    pub surface_2: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub ok: Hsla,
    pub ok_soft: Hsla,
    pub warn: Hsla,
    pub warn_soft: Hsla,
    pub err: Hsla,
    pub err_soft: Hsla,
}
pub fn palette(cx: &App) -> Palette {
    let theme = gpui_component::ActiveTheme::theme(cx);
    let soft = if theme.is_dark() { 0.2 } else { 0.12 };
    Palette {
        bg: theme.background,
        surface: theme.sidebar,
        surface_2: theme.secondary,
        border: theme.border,
        text: theme.foreground,
        muted: theme.muted_foreground,
        faint: theme.muted_foreground.opacity(0.7),
        accent: theme.primary,
        accent_soft: theme.primary.opacity(soft),
        ok: theme.success,
        ok_soft: theme.success.opacity(soft),
        warn: theme.warning,
        warn_soft: theme.warning.opacity(soft),
        err: theme.danger,
        err_soft: theme.danger.opacity(soft),
    }
}

/// The theme's monospace family; GPUI does not resolve a generic `monospace` name.
pub fn mono(cx: &App) -> gpui::SharedString {
    gpui_component::ActiveTheme::theme(cx)
        .mono_font_family
        .clone()
}

fn lightness(mut color: Hsla, change: f32) -> Hsla {
    color.l = (color.l + change).clamp(0., 1.);
    color
}
fn luminance(color: Hsla) -> f32 {
    let color = color.to_rgb();
    let linear = |v: f32| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}
fn foreground_on(color: Hsla) -> Hsla {
    rgb(if luminance(color) > 0.179 {
        0x000000
    } else {
        0xffffff
    })
    .into()
}

// Explicitly visit every field in the pinned ThemeColor. This preserves alpha
// and avoids serializing/quantizing the palette when a preference changes.
fn map_colors(colors: &mut ThemeColor, map: impl Fn(Hsla) -> Hsla) {
    macro_rules! visit { ($($field:ident),+ $(,)?) => { $(colors.$field = map(colors.$field);)+ }; }
    visit! {
    accent,
        accent_foreground,
        accordion,
        background,
        border,
        button,
        button_active,
        button_foreground,
        button_hover,
        button_danger,
        button_danger_active,
        button_danger_foreground,
        button_danger_hover,
        button_info,
        button_info_active,
        button_info_foreground,
        button_info_hover,
        button_primary,
        button_primary_active,
        button_primary_foreground,
        button_primary_hover,
        button_secondary,
        button_secondary_active,
        button_secondary_foreground,
        button_secondary_hover,
        button_success,
        button_success_active,
        button_success_foreground,
        button_success_hover,
        button_warning,
        button_warning_active,
        button_warning_foreground,
        button_warning_hover,
        group_box,
        group_box_foreground,
        caret,
        chart_1,
        chart_2,
        chart_3,
        chart_4,
        chart_5,
        chart_bullish,
        chart_bearish,
        chart_grid,
        danger,
        danger_active,
        danger_foreground,
        danger_hover,
        description_list_label,
        description_list_label_foreground,
        drag_border,
        drop_target,
        foreground,
        info,
        info_active,
        info_foreground,
        info_hover,
        input,
        link,
        link_active,
        link_hover,
        list,
        list_active,
        list_active_border,
        list_even,
        list_head,
        list_hover,
        muted,
        muted_foreground,
        popover,
        popover_foreground,
        primary,
        primary_active,
        primary_foreground,
        primary_hover,
        progress_bar,
        ring,
        scrollbar,
        scrollbar_thumb,
        scrollbar_thumb_hover,
        secondary,
        secondary_active,
        secondary_foreground,
        secondary_hover,
        selection,
        sidebar,
        sidebar_accent,
        sidebar_accent_foreground,
        sidebar_border,
        sidebar_foreground,
        sidebar_primary,
        sidebar_primary_foreground,
        skeleton,
        slider_bar,
        slider_thumb,
        success,
        success_foreground,
        success_hover,
        success_active,
        switch,
        switch_thumb,
        tab,
        tab_active,
        tab_active_foreground,
        tab_bar,
        tab_bar_segmented,
        tab_foreground,
        table,
        table_active,
        table_active_border,
        table_even,
        table_head,
        table_head_foreground,
        table_foot,
        table_foot_foreground,
        table_hover,
        table_row_border,
        title_bar,
        title_bar_border,
        status_bar,
        status_bar_border,
        warning,
        warning_active,
        warning_hover,
        warning_foreground,
        overlay,
        window_border,
        red,
        red_light,
        green,
        green_light,
        blue,
        blue_light,
        yellow,
        yellow_light,
        magenta,
        magenta_light,
        cyan,
        cyan_light,
        }
}

#[cfg(test)]
mod tests {
    use super::{apply, apply_preferences, luminance};
    use astra_client::{
        motion::MotionMode,
        preferences::{Accent, ColorDepth, Preferences},
    };
    use gpui::{Hsla, TestAppContext};
    use gpui_component::{MotionTokens, Theme, ThemeMode};

    #[gpui::test]
    fn palettes_remain_readable_and_mono_covers_every_semantic_color(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                for accent in Accent::ALL {
                    for colors in [
                        ColorDepth::TrueColor,
                        ColorDepth::Ansi256,
                        ColorDepth::Ansi16,
                        ColorDepth::Monochrome,
                    ] {
                        Theme::change(mode, None, cx);
                        apply_preferences(
                            &Preferences {
                                accent,
                                colors,
                                ..Default::default()
                            },
                            cx,
                        );
                        let theme = Theme::global(cx);
                        let contrast = |a, b| {
                            let a = luminance(a);
                            let b = luminance(b);
                            (a.max(b) + 0.05) / (a.min(b) + 0.05)
                        };
                        assert!(contrast(theme.primary, theme.primary_foreground) >= 4.5);
                        assert_eq!(theme.primary, theme.ring);
                        assert_eq!(theme.primary, theme.accent);
                        if colors == ColorDepth::Ansi16 {
                            assert!(contrast(theme.background, theme.foreground) >= 7.);
                        }
                        if colors == ColorDepth::Monochrome {
                            let palette = serde_json::to_value(theme.colors).unwrap();
                            for (name, value) in palette.as_object().unwrap() {
                                let color: Hsla = serde_json::from_value(value.clone()).unwrap();
                                assert!(color.s < 0.001, "{name} retains saturation");
                            }
                        }
                    }
                }
            }
        });
    }

    #[gpui::test]
    fn motion_and_full_palette_restore_after_preference_changes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for motion in [MotionMode::Reduced, MotionMode::Off] {
                apply(
                    &Preferences {
                        motion,
                        colors: ColorDepth::Monochrome,
                        ..Default::default()
                    },
                    cx,
                );
                assert!(cx.reduce_motion());
                assert!(Theme::global(cx).motion.duration_normal.is_zero());
            }
            apply(&Preferences::default(), cx);
            assert!(!cx.reduce_motion());
            assert_eq!(
                Theme::global(cx).motion.duration_normal,
                MotionTokens::default().duration_normal
            );
            assert!(Theme::global(cx).primary.s > 0.);
            let true_colors = Theme::global(cx).colors;
            apply(
                &Preferences {
                    colors: ColorDepth::Ansi256,
                    ..Default::default()
                },
                cx,
            );
            assert_eq!(true_colors, Theme::global(cx).colors);
            let expected = ThemeMode::from(cx.window_appearance());
            assert_eq!(Theme::global(cx).mode, expected);
        });
    }
}
