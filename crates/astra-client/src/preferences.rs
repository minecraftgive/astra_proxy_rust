use crate::{state::Session, tr};
use serde::{Deserialize, Serialize};
use std::io;
pub const FILE: &str = "ui-preferences.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageChoice {
    #[default]
    System,
    English,
    Simplified,
    Traditional,
}
impl LanguageChoice {
    pub fn language(self) -> Option<crate::i18n::Language> {
        use crate::i18n::Language;
        match self {
            Self::System => None,
            Self::English => Some(Language::English),
            Self::Simplified => Some(Language::Simplified),
            Self::Traditional => Some(Language::Traditional),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accent {
    #[default]
    Indigo,
    Teal,
    Green,
    Violet,
    Rose,
    Amber,
}
impl Accent {
    pub const ALL: [Self; 6] = [
        Self::Indigo,
        Self::Teal,
        Self::Green,
        Self::Violet,
        Self::Rose,
        Self::Amber,
    ];
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorDepth {
    #[default]
    Auto,
    TrueColor,
    Ansi256,
    Ansi16,
    Monochrome,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartPage {
    #[default]
    Overview,
    Proxies,
    Connections,
    Rules,
    Logs,
    Profiles,
    Settings,
}
impl StartPage {
    pub const ALL: [Self; 7] = [
        Self::Overview,
        Self::Proxies,
        Self::Connections,
        Self::Rules,
        Self::Logs,
        Self::Profiles,
        Self::Settings,
    ];
    pub fn page(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// Which frontend the operating system starts at sign-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupFrontend {
    #[default]
    Tui,
    Gui,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub language: LanguageChoice,
    pub accent: Accent,
    pub colors: ColorDepth,
    pub motion: crate::motion::MotionMode,
    pub vim_keys: bool,
    pub mouse: bool,
    pub start_page: StartPage,
    pub terminal: String,
    pub startup_frontend: StartupFrontend,
    pub installation_notices: std::collections::BTreeSet<String>,
    /// The generated OS startup file is authoritative, not these preferences.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    #[serde(skip)]
    pub startup_enabled: bool,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub silent_startup: bool,
    #[cfg(windows)]
    #[serde(skip)]
    pub windows_service: bool,
    pub marquee: MarqueePreferences,
}

/// Hidden until Ctrl+Home is held for three seconds on the accent color row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarqueePreferences {
    pub unlocked: bool,
    pub border: bool,
    pub text: bool,
    pub background: bool,
    pub status: bool,
    pub indicator: bool,
    /// Hue degrees per second.
    #[serde(deserialize_with = "speed_or_preset")]
    pub speed: u16,
    /// Cells one whole rainbow spans.
    #[serde(deserialize_with = "width_or_preset")]
    pub width: u16,
    /// Frames per second while only the marquee is moving.
    pub fps: u16,
    pub true_color: bool,
}
impl Default for MarqueePreferences {
    fn default() -> Self {
        Self {
            unlocked: false,
            border: false,
            text: false,
            background: false,
            status: false,
            indicator: false,
            speed: SPEED.default,
            width: WIDTH.default,
            fps: FPS.default,
            true_color: false,
        }
    }
}
impl MarqueePreferences {
    /// What actually runs: nothing while the section is hidden.
    pub fn effect(self) -> crate::motion::Marquee {
        crate::motion::Marquee {
            border: self.unlocked && self.border,
            text: self.unlocked && self.text,
            background: self.unlocked && self.background,
            status: self.unlocked && self.status,
            indicator: self.unlocked && self.indicator,
            true_color: self.true_color,
            speed: f32::from(SPEED.clamp(self.speed)),
            width: f32::from(WIDTH.clamp(self.width)),
            fps: FPS.clamp(self.fps),
        }
    }
}
/// A preference adjusted in steps rather than picked from named choices.
#[derive(Clone, Copy, Debug)]
pub struct Range {
    pub min: u16,
    pub max: u16,
    pub step: u16,
    /// Step with Shift held.
    pub big: u16,
    pub default: u16,
    pub unit: &'static str,
}
impl Range {
    pub fn clamp(self, value: u16) -> u16 {
        value.clamp(self.min, self.max)
    }
    pub fn nudge(self, value: u16, up: bool, big: bool) -> u16 {
        let step = if big { self.big } else { self.step };
        self.clamp(if up {
            value.saturating_add(step)
        } else {
            value.saturating_sub(step)
        })
    }
}
pub const SPEED: Range = Range {
    min: 0,
    max: 1800,
    step: 10,
    big: 100,
    default: 160,
    unit: "\u{b0}/s",
};
pub const WIDTH: Range = Range {
    min: 2,
    max: 400,
    step: 1,
    big: 10,
    default: 30,
    unit: "cells",
};
pub const FPS: Range = Range {
    min: 1,
    max: 120,
    step: 1,
    big: 10,
    default: 20,
    unit: "fps",
};
/// A number, or a preset name saved by an earlier build.
#[derive(Deserialize)]
#[serde(untagged)]
enum NumberOrPreset {
    Number(u64),
    Preset(String),
}
fn number_or_preset<'de, D: serde::Deserializer<'de>>(
    d: D,
    range: Range,
    preset: fn(&str) -> Option<u16>,
) -> Result<u16, D::Error> {
    Ok(match NumberOrPreset::deserialize(d)? {
        NumberOrPreset::Number(n) => range.clamp(n.min(u64::from(u16::MAX)) as u16),
        NumberOrPreset::Preset(name) => preset(&name).unwrap_or(range.default),
    })
}
fn speed_or_preset<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    number_or_preset(d, SPEED, |name| match name {
        "slow" => Some(60),
        "fast" => Some(320),
        "very_fast" => Some(640),
        _ => None,
    })
}
fn width_or_preset<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    number_or_preset(d, WIDTH, |name| match name {
        "narrow" => Some(12),
        "wide" => Some(60),
        "extra_wide" => Some(120),
        _ => None,
    })
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: LanguageChoice::System,
            accent: Accent::Indigo,
            colors: ColorDepth::Auto,
            motion: crate::motion::MotionMode::Normal,
            vim_keys: false,
            mouse: true,
            start_page: StartPage::Overview,
            terminal: "auto".into(),
            startup_frontend: StartupFrontend::Tui,
            installation_notices: Default::default(),
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            startup_enabled: false,
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            silent_startup: false,
            #[cfg(windows)]
            windows_service: false,
            marquee: MarqueePreferences::default(),
        }
    }
}

pub fn load(path: &std::path::Path, session: &Session) -> (Preferences, Option<String>) {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(preferences) => (preferences, None),
            Err(error) => (
                Preferences::default(),
                Some(tr!(
                    "Cannot read preferences, using defaults: {}",
                    error.to_string()
                )),
            ),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => (
            Preferences {
                motion: session.motion,
                vim_keys: session.vim_keys,
                ..Default::default()
            },
            None,
        ),
        Err(error) => (
            Preferences::default(),
            Some(tr!(
                "Cannot read preferences, using defaults: {}",
                error.to_string()
            )),
        ),
    }
}
