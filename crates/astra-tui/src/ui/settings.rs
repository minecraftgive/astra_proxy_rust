//! Settings page: personal preferences shared by every window of this user.
//!
//! Preferences live in their own file next to the UI session, not in the
//! per-window session, so a change made in one window is what the next window
//! opens with. Every change applies immediately and is saved at once.
use super::dashboard::{Target, BLUE, MUTED};
use super::*;
#[cfg(windows)]
use crate::startup as startup_registration;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::startup_unix as startup_registration;
use ratatui::{layout::Rect, style::Modifier};

mod program_info;
mod terminals;
pub(super) use terminals::TerminalState;

/// Information and preferences share one viewport. Explicit scrolling lets a
/// short terminal inspect the header without changing a preference.
#[derive(Default)]
pub(super) struct View {
    offset: std::cell::Cell<usize>,
    height: std::cell::Cell<usize>,
    total: std::cell::Cell<usize>,
    follow_selection: std::cell::Cell<bool>,
    selected_visible: std::cell::Cell<bool>,
}

pub(super) const FILE: &str = "ui-preferences.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LanguageChoice {
    #[default]
    System,
    English,
    Simplified,
    Traditional,
}
impl LanguageChoice {
    fn language(self) -> Option<crate::i18n::Language> {
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
pub(super) enum Accent {
    #[default]
    Indigo,
    Teal,
    Green,
    Violet,
    Rose,
    Amber,
}
impl Accent {
    const ALL: [Self; 6] = [
        Self::Indigo,
        Self::Teal,
        Self::Green,
        Self::Violet,
        Self::Rose,
        Self::Amber,
    ];
    /// Every accent keeps white text readable on top of it.
    pub(super) fn color(self) -> Color {
        match self {
            Self::Indigo => BLUE,
            Self::Teal => Color::Rgb(15, 118, 110),
            Self::Green => Color::Rgb(21, 128, 61),
            Self::Violet => Color::Rgb(124, 58, 237),
            Self::Rose => Color::Rgb(190, 18, 60),
            Self::Amber => Color::Rgb(180, 83, 9),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ColorDepth {
    #[default]
    Auto,
    TrueColor,
    Ansi256,
    Ansi16,
    Monochrome,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StartPage {
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
    const ALL: [Self; 7] = [
        Self::Overview,
        Self::Proxies,
        Self::Connections,
        Self::Rules,
        Self::Logs,
        Self::Profiles,
        Self::Settings,
    ];
    pub(super) fn page(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Preferences {
    pub(super) language: LanguageChoice,
    pub(super) accent: Accent,
    pub(super) colors: ColorDepth,
    pub(super) motion: animation::MotionMode,
    pub(super) vim_keys: bool,
    pub(super) mouse: bool,
    pub(super) start_page: StartPage,
    pub(super) terminal: String,
    /// The generated OS startup file is authoritative, not these preferences.
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    #[serde(skip)]
    pub(super) startup_enabled: bool,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(super) silent_startup: bool,
    #[cfg(windows)]
    #[serde(skip)]
    pub(super) windows_service: bool,
    pub(super) marquee: MarqueePreferences,
}

/// Hidden until Ctrl+Home is held for three seconds on the accent color row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct MarqueePreferences {
    pub(super) unlocked: bool,
    pub(super) border: bool,
    pub(super) text: bool,
    pub(super) background: bool,
    pub(super) status: bool,
    pub(super) indicator: bool,
    /// Hue degrees per second.
    #[serde(deserialize_with = "speed_or_preset")]
    pub(super) speed: u16,
    /// Cells one whole rainbow spans.
    #[serde(deserialize_with = "width_or_preset")]
    pub(super) width: u16,
    /// Frames per second while only the marquee is moving.
    pub(super) fps: u16,
    pub(super) true_color: bool,
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
    pub(super) fn effect(self) -> animation::Marquee {
        animation::Marquee {
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
pub(super) struct Range {
    min: u16,
    max: u16,
    step: u16,
    /// Step with Shift held.
    big: u16,
    default: u16,
    unit: &'static str,
}
impl Range {
    fn clamp(self, value: u16) -> u16 {
        value.clamp(self.min, self.max)
    }
    fn nudge(self, value: u16, up: bool, big: bool) -> u16 {
        let step = if big { self.big } else { self.step };
        self.clamp(if up {
            value.saturating_add(step)
        } else {
            value.saturating_sub(step)
        })
    }
}
const SPEED: Range = Range {
    min: 0,
    max: 1800,
    step: 10,
    big: 100,
    default: 160,
    unit: "\u{b0}/s",
};
const WIDTH: Range = Range {
    min: 2,
    max: 400,
    step: 1,
    big: 10,
    default: 30,
    unit: "cells",
};
const FPS: Range = Range {
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
/// How long Ctrl+Home must be held, and the longest gap between key repeats
/// that still counts as holding (the first repeat waits for the OS delay).
const HOLD: Duration = Duration::from_secs(3);
const HOLD_GAP: Duration = Duration::from_millis(800);

impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: LanguageChoice::System,
            accent: Accent::Indigo,
            colors: ColorDepth::Auto,
            motion: animation::MotionMode::Normal,
            vim_keys: false,
            mouse: true,
            start_page: StartPage::Overview,
            terminal: "auto".into(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Language,
    Accent,
    Colors,
    Motion,
    VimKeys,
    Mouse,
    StartPage,
    Terminal,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    StartupRegistration,
    #[cfg(any(windows, target_os = "macos"))]
    SilentStartup,
    #[cfg(windows)]
    WindowsService,
    MarqueeBorder,
    MarqueeText,
    MarqueeBackground,
    MarqueeStatus,
    MarqueeIndicator,
    MarqueeSpeed,
    MarqueeWidth,
    MarqueeFps,
    MarqueeTrueColor,
}
const ALL_FIELDS: &[Field] = &[
    Field::Language,
    Field::Accent,
    Field::Colors,
    Field::Motion,
    Field::VimKeys,
    Field::Mouse,
    Field::StartPage,
    Field::Terminal,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    Field::StartupRegistration,
    #[cfg(any(windows, target_os = "macos"))]
    Field::SilentStartup,
    #[cfg(windows)]
    Field::WindowsService,
    Field::MarqueeBorder,
    Field::MarqueeText,
    Field::MarqueeBackground,
    Field::MarqueeStatus,
    Field::MarqueeIndicator,
    Field::MarqueeSpeed,
    Field::MarqueeWidth,
    Field::MarqueeFps,
    Field::MarqueeTrueColor,
];
/// Row of the first marquee field.
const MARQUEE_ROW: usize = 8
    + 3 * cfg!(windows) as usize
    + 2 * cfg!(target_os = "macos") as usize
    + cfg!(target_os = "linux") as usize;
/// Rows shown now; the marquee section stays hidden until it is unlocked.
fn fields(p: &Preferences) -> &'static [Field] {
    if p.marquee.unlocked {
        ALL_FIELDS
    } else {
        &ALL_FIELDS[..MARQUEE_ROW]
    }
}
const SECTIONS: [(&str, std::ops::Range<usize>); 4] = [
    ("Appearance", 0..4),
    ("Interaction", 4..7),
    ("Startup", 7..MARQUEE_ROW),
    ("Marquee", MARQUEE_ROW..ALL_FIELDS.len()),
];

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Language => "Language",
            Self::Accent => "Accent color",
            Self::Colors => "Colors",
            Self::Motion => "Motion",
            Self::VimKeys => "Vim navigation",
            Self::Mouse => "Mouse",
            Self::StartPage => "Start page",
            Self::Terminal => "Terminal",
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupRegistration => {
                if cfg!(target_os = "macos") {
                    "Start TUI at macOS login"
                } else if cfg!(target_os = "linux") {
                    "systemd user service file"
                } else {
                    "Start TUI with Windows"
                }
            }
            #[cfg(any(windows, target_os = "macos"))]
            Self::SilentStartup => "Silent startup",
            #[cfg(windows)]
            Self::WindowsService => "Windows privilege service",
            Self::MarqueeBorder => "Focused border",
            Self::MarqueeText => "Focused item text",
            Self::MarqueeBackground => "Focused item background",
            Self::MarqueeStatus => "Status text",
            Self::MarqueeIndicator => "Status indicator",
            Self::MarqueeSpeed => "Marquee speed",
            Self::MarqueeWidth => "Gradient width",
            Self::MarqueeFps => "Frame rate",
            Self::MarqueeTrueColor => "Always true color",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::Language => "Interface language. System follows ASTRA_LANG, LANG and the OS locale; --lang on the command line wins for that run.",
            Self::Accent => "Color of the active page tab, key badges and selections.",
            Self::Colors => "Auto uses what the terminal reports. Pick fewer colors if highlights look wrong over SSH or in a basic console.",
            Self::Motion => "Normal animates overlays opening and closing. Reduced and Off show every change at once.",
            Self::VimKeys => "h j k l g G move in lists and logs, in addition to the arrow keys.",
            Self::Mouse => "Off returns the mouse to the terminal so you can select and copy text.",
            Self::StartPage => "Page shown when a window opens without --view.",
            Self::Terminal => "Choose the terminal for the next TUI launch, new windows and non-silent startup. The current window stays here. Silent startup does not use a terminal. Press r to scan again.",
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupRegistration => {
                if cfg!(target_os = "macos") {
                    "Open this TUI in the selected terminal at the next macOS login. Silent startup runs it in the background instead. Each instance config controls startup with auto_start. No administrator permission is required."
                } else if cfg!(target_os = "linux") {
                    "Create or remove a user systemd service file for silent startup. Enable it manually with systemctl. Each instance config controls startup with auto_start. Removing the file does not stop running instances."
                } else {
                    "Open this TUI when the current user signs in to Windows, using the current directories and manager target. Set auto_start = true in each instance config to start it too."
                }
            }
            #[cfg(any(windows, target_os = "macos"))]
            Self::SilentStartup => "At sign-in, start in the background without a terminal window. Each instance config controls startup with auto_start. Open the TUI manually to manage them; failures are recorded in startup.log.",
            #[cfg(windows)]
            Self::WindowsService => "Register a local Windows service with one UAC approval so TUN can start silently later. Requires an administrator account. Removing or updating the protected service requires approval again.",
            Self::MarqueeBorder => "A rainbow runs around the border of the focused panel.",
            Self::MarqueeText => "A rainbow runs across the text of the selected row.",
            Self::MarqueeBackground => "A rainbow runs across the background of the selected row.",
            Self::MarqueeStatus => "A rainbow runs across the status text at the bottom of the window.",
            Self::MarqueeIndicator => "A rainbow runs across the connection dot and state word, only while connected.",
            Self::MarqueeSpeed => "Hue degrees per second, 0–1800; at 360 one whole rainbow passes each cell per second. ←→ ±10, Shift ±100. Reduced or Off motion holds the colors still.",
            Self::MarqueeWidth => "Cells one whole rainbow spans, 2–400; wider is a gentler gradient. ←→ ±1, Shift ±10.",
            Self::MarqueeFps => "Redraws per second while the marquee moves, 1–120. Higher is smoother but sends more to the terminal; over SSH keep it low. ←→ ±1, Shift ±10.",
            Self::MarqueeTrueColor => "Marquee colors ignore the Colors setting and are always sent as 24-bit color. Needs a terminal with true color.",
        }
    }
    /// Choice names; language names are shown in their own language.
    fn choices(self) -> &'static [&'static str] {
        match self {
            Self::Terminal => &["Auto"], // Dynamic choices are supplied by Ui.
            Self::Language => &["System", "English", "简体中文", "繁體中文"],
            Self::Accent => &["Indigo", "Teal", "Green", "Violet", "Rose", "Amber"],
            Self::Colors => &[
                "Auto",
                "True color",
                "256 colors",
                "16 colors",
                "Monochrome",
            ],
            Self::Motion => &["Normal", "Reduced", "Off"],
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupRegistration => {
                if cfg!(target_os = "linux") {
                    &["Created", "Not created"]
                } else {
                    &["On", "Off"]
                }
            }
            #[cfg(any(windows, target_os = "macos"))]
            Self::SilentStartup => &["On", "Off"],
            #[cfg(windows)]
            Self::WindowsService => &["On", "Off"],
            Self::VimKeys
            | Self::Mouse
            | Self::MarqueeBorder
            | Self::MarqueeText
            | Self::MarqueeBackground
            | Self::MarqueeStatus
            | Self::MarqueeIndicator
            | Self::MarqueeTrueColor => &["On", "Off"],
            Self::MarqueeSpeed | Self::MarqueeWidth | Self::MarqueeFps => &[],
            Self::StartPage => &shell::PAGES,
        }
    }
    fn get(self, p: &Preferences) -> usize {
        use animation::MotionMode;
        match self {
            Self::Language => p.language as usize,
            Self::Accent => p.accent as usize,
            Self::Colors => p.colors as usize,
            Self::Motion => match p.motion {
                MotionMode::Normal => 0,
                MotionMode::Reduced => 1,
                MotionMode::Off => 2,
            },
            Self::VimKeys => usize::from(!p.vim_keys),
            Self::Mouse => usize::from(!p.mouse),
            Self::StartPage => p.start_page.page(),
            Self::Terminal => 0, // The cached terminal list resolves the saved ID.
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupRegistration => usize::from(!p.startup_enabled),
            #[cfg(any(windows, target_os = "macos"))]
            Self::SilentStartup => usize::from(!p.silent_startup),
            #[cfg(windows)]
            Self::WindowsService => usize::from(!p.windows_service),
            Self::MarqueeBorder => usize::from(!p.marquee.border),
            Self::MarqueeText => usize::from(!p.marquee.text),
            Self::MarqueeBackground => usize::from(!p.marquee.background),
            Self::MarqueeStatus => usize::from(!p.marquee.status),
            Self::MarqueeIndicator => usize::from(!p.marquee.indicator),
            Self::MarqueeSpeed => usize::from(p.marquee.speed),
            Self::MarqueeWidth => usize::from(p.marquee.width),
            Self::MarqueeFps => usize::from(p.marquee.fps),
            Self::MarqueeTrueColor => usize::from(!p.marquee.true_color),
        }
    }
    fn set(self, p: &mut Preferences, index: usize) {
        use animation::MotionMode;
        let index = match self.range() {
            Some(range) => usize::from(range.clamp(index.min(usize::from(u16::MAX)) as u16)),
            None => index.min(self.choices().len() - 1),
        };
        match self {
            Self::Language => {
                p.language = [
                    LanguageChoice::System,
                    LanguageChoice::English,
                    LanguageChoice::Simplified,
                    LanguageChoice::Traditional,
                ][index]
            }
            Self::Accent => p.accent = Accent::ALL[index],
            Self::Colors => {
                p.colors = [
                    ColorDepth::Auto,
                    ColorDepth::TrueColor,
                    ColorDepth::Ansi256,
                    ColorDepth::Ansi16,
                    ColorDepth::Monochrome,
                ][index]
            }
            Self::Motion => {
                p.motion = [MotionMode::Normal, MotionMode::Reduced, MotionMode::Off][index]
            }
            Self::VimKeys => p.vim_keys = index == 0,
            Self::Mouse => p.mouse = index == 0,
            Self::StartPage => p.start_page = StartPage::ALL[index],
            Self::Terminal => p.terminal = "auto".into(),
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupRegistration => p.startup_enabled = index == 0,
            #[cfg(any(windows, target_os = "macos"))]
            Self::SilentStartup => p.silent_startup = index == 0,
            #[cfg(windows)]
            Self::WindowsService => p.windows_service = index == 0,
            Self::MarqueeBorder => p.marquee.border = index == 0,
            Self::MarqueeText => p.marquee.text = index == 0,
            Self::MarqueeBackground => p.marquee.background = index == 0,
            Self::MarqueeStatus => p.marquee.status = index == 0,
            Self::MarqueeIndicator => p.marquee.indicator = index == 0,
            Self::MarqueeSpeed => p.marquee.speed = index as u16,
            Self::MarqueeWidth => p.marquee.width = index as u16,
            Self::MarqueeFps => p.marquee.fps = index as u16,
            Self::MarqueeTrueColor => p.marquee.true_color = index == 0,
        }
    }
    /// Numeric preferences have a range instead of choices.
    fn range(self) -> Option<Range> {
        match self {
            Self::MarqueeSpeed => Some(SPEED),
            Self::MarqueeWidth => Some(WIDTH),
            Self::MarqueeFps => Some(FPS),
            _ => None,
        }
    }
    /// The current value as shown in the row and in notices.
    fn value_label(self, p: &Preferences) -> String {
        match self.range() {
            Some(range) => format!("{} {}", self.get(p), t(range.unit)),
            None => self.choice_label(self.get(p)).to_owned(),
        }
    }
    fn choice_label(self, index: usize) -> &'static str {
        let name = self.choices()[index];
        if self == Self::Language && index > 0 {
            name
        } else {
            t(name)
        }
    }
}

/// Read saved preferences. A missing file starts from what the session already
/// remembered (motion, Vim aliases); a damaged file falls back to defaults.
pub(super) fn load(path: &std::path::Path, session: &Session) -> (Preferences, Option<String>) {
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

impl Ui {
    /// Load preferences at startup and apply all of them.
    pub(super) fn load_preferences(&mut self, path: PathBuf) {
        let (preferences, error) = load(&path, &self.session);
        self.preferences_path = Some(path);
        self.preferences = preferences;
        self.apply_preferences(None);
        if let Some(error) = error {
            self.message = error;
        }
    }
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(super) fn load_startup_context(&mut self, context: &crate::windows::Context) {
        self.startup_context = Some(context.clone());
        match startup_registration::enabled(context) {
            Ok(enabled) => self.preferences.startup_enabled = enabled,
            Err(error) => {
                self.message = tr!("Cannot read startup registration: {}", format!("{error:#}"));
            }
        }
        #[cfg(windows)]
        match astra_platform::windows_service::installed() {
            Ok(installed) => self.preferences.windows_service = installed,
            Err(error) => self.message = tr!("Cannot read Windows service: {}", error.to_string()),
        }
    }

    #[cfg(windows)]
    fn change_windows_service(&mut self, enabled: bool) {
        if self.service_change.is_some() {
            self.message = t("Windows service registration is in progress").into();
            return;
        }
        if self.preferences.windows_service == enabled {
            return;
        }
        let Some(context) = &self.startup_context else {
            self.message = tr!(
                "Cannot change Windows service: {}",
                "Startup context is unavailable"
            );
            return;
        };
        if crate::protocol::target().is_some() {
            self.message = t("Open the local TUI to register the Windows privilege service").into();
            return;
        }
        let bin = context.bin.clone();
        self.service_change = Some(tokio::task::spawn_blocking(move || {
            astra_platform::windows_service::set_installed(&bin, enabled)
                .and_then(|_| astra_platform::windows_service::installed())
                .map_err(|error| error.to_string())
        }));
        self.message = t("Approve the Windows UAC prompt to change the privilege service").into();
    }

    #[cfg(windows)]
    pub(super) async fn poll_service_change(&mut self) -> bool {
        if !self
            .service_change
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            return false;
        }
        let result = self
            .service_change
            .take()
            .unwrap()
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        match result {
            Ok(installed) => {
                self.preferences.windows_service = installed;
                self.message = format!(
                    "{}: {}",
                    t(Field::WindowsService.label()),
                    Field::WindowsService.value_label(&self.preferences)
                );
            }
            Err(error) => {
                if let Ok(installed) = astra_platform::windows_service::installed() {
                    self.preferences.windows_service = installed;
                }
                self.message = tr!("Cannot change Windows service: {}", error);
            }
        }
        true
    }
    /// Save first so a failed write never presents an unsaved choice as enabled.
    pub(super) fn change_preferences(&mut self, change: impl FnOnce(&mut Preferences)) -> bool {
        let previous = self.preferences.clone();
        change(&mut self.preferences);
        if self.preferences == previous {
            return true;
        }
        if let Err(error) = self.save_preferences() {
            self.preferences = previous;
            self.message = tr!("Cannot save preferences: {}", format!("{error:#}"));
            return false;
        }
        self.apply_preferences(Some(&previous));
        true
    }
    fn save_preferences(&self) -> Result<()> {
        let Some(path) = &self.preferences_path else {
            return Ok(());
        };
        let data = serde_json::to_vec_pretty(&self.preferences)?;
        astra_platform::storage::atomic_write_default(path, &data)?;
        Ok(())
    }
    /// `previous` is `None` at startup, when everything is applied.
    fn apply_preferences(&mut self, previous: Option<&Preferences>) {
        let p = self.preferences.clone();
        let changed = |field: Field| previous.is_none_or(|old| field.get(old) != field.get(&p));
        if changed(Field::Language) && !(previous.is_none() && crate::i18n::command_line_language())
        {
            crate::i18n::set_language(p.language.language());
        }
        if changed(Field::Accent) || changed(Field::Colors) {
            self.animations.set_palette(p.accent.color(), p.colors);
        }
        if changed(Field::Motion) {
            self.session.motion = p.motion;
            self.animations.set_mode(p.motion);
        }
        if changed(Field::VimKeys) {
            self.session.vim_keys = p.vim_keys;
            for viewer in [&mut self.viewer, &mut self.tui_viewer]
                .into_iter()
                .flatten()
            {
                viewer.set_vim_navigation(p.vim_keys);
            }
        }
        if previous.is_none_or(|old| old.marquee != p.marquee) {
            self.animations.set_marquee(p.marquee.effect());
        }
        // Mouse capture belongs to the terminal session, which the event loop owns.
        if previous.is_some_and(|old| old.mouse != p.mouse) || (previous.is_none() && !p.mouse) {
            self.mouse_request = Some(p.mouse);
        }
    }

    fn settings_field(&self) -> Field {
        let fields = fields(&self.preferences);
        fields[self.settings_cursor.min(fields.len() - 1)]
    }

    /// Holding Ctrl+Home for three seconds on the accent color row shows the
    /// marquee section, or hides it again and turns it off. Any other key, a
    /// release or a pause longer than the key-repeat delay starts over.
    pub(super) fn marquee_hold_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        let chord = key.code == KeyCode::Home
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        let here = self.page == shell::SETTINGS
            && self.settings_field() == Field::Accent
            && self.focus.region == focus::Region::Main
            && self.palette.is_none()
            && self.prompt.is_none()
            && self.menu.is_none()
            && self.help_scroll.is_none();
        if !(chord && here) {
            self.marquee_hold = None;
            return None;
        }
        self.marquee_hold_at(Instant::now());
        Some(Input::Idle)
    }
    fn marquee_hold_at(&mut self, now: Instant) {
        let (start, done) = match self.marquee_hold {
            Some((start, last, done)) if now.saturating_duration_since(last) <= HOLD_GAP => {
                (start, done)
            }
            _ => (now, false),
        };
        let held = now.saturating_duration_since(start);
        let finished = !done && held >= HOLD;
        self.marquee_hold = Some((start, now, done || finished));
        if finished {
            let unlock = !self.preferences.marquee.unlocked;
            if !self.change_preferences(|p| {
                p.marquee = if unlock {
                    MarqueePreferences {
                        unlocked: true,
                        border: true,
                        ..p.marquee
                    }
                } else {
                    MarqueePreferences::default()
                }
            }) {
                return;
            }
            if unlock {
                self.settings_cursor = MARQUEE_ROW;
                self.settings_view.follow_selection.set(true);
            }
            self.message = t(if unlock {
                "Marquee unlocked"
            } else {
                "Marquee hidden"
            })
            .into();
        } else if !done && held >= Duration::from_millis(500) {
            let steps = ((held.as_millis() * 10 / HOLD.as_millis()) as usize).min(10);
            self.message = tr!(
                "Keep holding\u{2026} {}",
                format!(
                    "{}{}",
                    "\u{25B0}".repeat(steps),
                    "\u{25B1}".repeat(10 - steps)
                )
            );
        }
    }
    /// Releasing Home or Ctrl ends a hold even inside the repeat gap
    /// (terminals that report releases).
    pub(super) fn key_release(&mut self, key: event::KeyEvent) {
        use event::ModifierKeyCode::{LeftControl, RightControl};
        if matches!(
            key.code,
            KeyCode::Home | KeyCode::Modifier(LeftControl | RightControl)
        ) {
            self.marquee_hold = None;
        }
    }
    fn choose_setting(&mut self, field: Field, index: usize) {
        if field == Field::Terminal {
            self.choose_terminal(index);
            return;
        }
        #[cfg(windows)]
        if field == Field::WindowsService {
            self.change_windows_service(index == 0);
            return;
        }
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        if field == Field::StartupRegistration {
            let result = self
                .startup_context
                .as_ref()
                .context("Startup context is unavailable")
                .and_then(|context| {
                    startup_registration::set_enabled(
                        context,
                        index == 0,
                        self.preferences.silent_startup,
                    )
                });
            match result {
                Ok(()) => self.preferences.startup_enabled = index == 0,
                Err(error) => {
                    if let Some(context) = &self.startup_context {
                        if let Ok(enabled) = startup_registration::enabled(context) {
                            self.preferences.startup_enabled = enabled;
                        }
                    }
                    self.message = tr!(
                        "Cannot change startup registration: {}",
                        format!("{error:#}")
                    );
                    return;
                }
            }
            self.message = format!(
                "{}: {}",
                t(field.label()),
                field.value_label(&self.preferences)
            );
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if let Some(context) = &self.startup_context {
                if let Ok(path) = startup_registration::managed_path(context) {
                    self.message.push_str(&format!(" · {}", path.display()));
                }
            }
            return;
        }
        #[cfg(any(windows, target_os = "macos"))]
        if field == Field::SilentStartup && self.preferences.startup_enabled {
            let previous = self.preferences.silent_startup;
            let result = self
                .startup_context
                .as_ref()
                .context("Startup context is unavailable")
                .and_then(|context| startup_registration::set_enabled(context, true, index == 0));
            if let Err(error) = result {
                self.message = tr!(
                    "Cannot change startup registration: {}",
                    format!("{error:#}")
                );
                return;
            }
            if !self.change_preferences(|p| p.silent_startup = index == 0) {
                if let Some(context) = &self.startup_context {
                    if let Err(error) = startup_registration::set_enabled(context, true, previous) {
                        self.message.push_str(&format!(
                            "\n{}",
                            tr!(
                                "Cannot change startup registration: {}",
                                format!("{error:#}")
                            )
                        ));
                    }
                }
                return;
            }
        }
        if !self.change_preferences(|p| field.set(p, index)) {
            return;
        }
        self.message = format!(
            "{}: {}",
            t(field.label()),
            field.value_label(&self.preferences)
        );
    }
    pub(super) fn click_setting(&mut self, row: usize, choice: Option<usize>) -> Input {
        self.focus = Default::default();
        self.settings_view.follow_selection.set(true);
        self.settings_cursor = row.min(fields(&self.preferences).len() - 1);
        let field = self.settings_field();
        let current = self.setting_index(field);
        if field == Field::Terminal && choice.is_none() {
            self.open_terminal_picker();
            return Input::Idle;
        }
        let index = match field.range() {
            // The arrows either side of the value: 0 lowers it, anything else raises it.
            Some(range) => usize::from(range.nudge(current as u16, choice != Some(0), false)),
            None => choice.unwrap_or((current + 1) % self.setting_choice_count(field)),
        };
        self.choose_setting(field, index);
        Input::Idle
    }

    /// Settings keys; `None` lets page numbers, the palette and quit through.
    pub(super) fn settings_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if self.terminals.picker.is_some() {
            return self.terminal_picker_key(key);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        // The information header can fill a short viewport. A change key first
        // reveals the selected preference instead of changing a hidden value.
        if self.settings_view.total.get() > 0
            && !self.settings_view.selected_visible.get()
            && matches!(
                key.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Char('h' | 'l' | ' ')
                    | KeyCode::Enter
                    | KeyCode::Delete
                    | KeyCode::Backspace
            )
        {
            self.settings_view.follow_selection.set(true);
            return Some(Input::Idle);
        }
        let field = self.settings_field();
        let current = self.setting_index(field);
        if let Some(range) = field.range() {
            let big = key.modifiers.contains(KeyModifiers::SHIFT);
            let up = match key.code {
                KeyCode::Left | KeyCode::Char('h') => Some(false),
                KeyCode::Right | KeyCode::Char('l') => Some(true),
                _ => None,
            };
            if let Some(up) = up {
                let value = range.nudge(current as u16, up, big);
                if usize::from(value) != current {
                    self.choose_setting(field, usize::from(value));
                }
                self.settings_view.follow_selection.set(true);
                return Some(Input::Idle);
            }
            if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                self.settings_view.follow_selection.set(true);
                return Some(Input::Idle);
            }
        }
        let count = self.setting_choice_count(field);
        match key.code {
            KeyCode::Char('r') => {
                self.refresh_program_info();
                self.request_terminal_scan();
                return Some(Input::Idle);
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let step = self.settings_view.height.get().saturating_sub(1).max(1);
                self.move_settings_view(key.code == KeyCode::PageDown, step);
                return Some(Input::Idle);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_cursor = self.settings_cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_cursor =
                    (self.settings_cursor + 1).min(fields(&self.preferences).len() - 1)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.settings_cursor = 0;
                self.settings_view.offset.set(0);
                self.settings_view.follow_selection.set(false);
                return Some(Input::Idle);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.settings_cursor = fields(&self.preferences).len() - 1
            }
            KeyCode::Left | KeyCode::Char('h') if current > 0 => {
                self.choose_setting(field, current - 1)
            }
            KeyCode::Right | KeyCode::Char('l') if current + 1 < count => {
                self.choose_setting(field, current + 1)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {}
            KeyCode::Enter | KeyCode::Char(' ') => {
                if field == Field::Terminal {
                    self.open_terminal_picker();
                } else {
                    self.choose_setting(field, (current + 1) % count)
                }
            }
            KeyCode::Delete | KeyCode::Backspace => {
                let default = field.get(&Preferences::default());
                self.choose_setting(field, default);
            }
            _ => return None,
        }
        self.settings_view.follow_selection.set(true);
        Some(Input::Idle)
    }

    pub(super) fn scroll_settings(&mut self, down: bool) {
        if self.terminals.picker.is_some() {
            self.terminal_picker_key(event::KeyEvent::new(
                if down { KeyCode::Down } else { KeyCode::Up },
                KeyModifiers::NONE,
            ));
            return;
        }
        self.move_settings_view(down, 3);
    }

    fn move_settings_view(&mut self, down: bool, step: usize) {
        let view = &self.settings_view;
        let offset = if down {
            view.offset.get().saturating_add(step)
        } else {
            view.offset.get().saturating_sub(step)
        };
        view.offset
            .set(offset.min(view.total.get().saturating_sub(view.height.get())));
        view.follow_selection.set(false);
    }

    pub(super) fn render_settings(&self, f: &mut Frame<'_>, area: Rect) {
        let block = self.focused_card("Settings", focus::Region::Main);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width < 4 || inner.height == 0 {
            self.settings_view.selected_visible.set(false);
            return;
        }
        let fields = fields(&self.preferences);
        let selected = self.settings_cursor.min(fields.len() - 1);
        let label_width = fields
            .iter()
            .map(|field| Line::from(t(field.label())).width())
            .max()
            .unwrap_or(0)
            + 2;
        // (line, field row) — section titles and spacing have no row.
        let mut lines: Vec<(Line<'static>, Option<usize>)> = self
            .settings_program_info(inner.width.saturating_sub(1))
            .into_iter()
            .map(|line| (line, None))
            .collect();
        lines.push((Line::raw(""), None));
        for (index, (title, range)) in SECTIONS.iter().enumerate() {
            if range.start >= fields.len() {
                break;
            }
            if index > 0 {
                lines.push((Line::raw(""), None));
            }
            lines.push((
                Line::styled(
                    format!(" {}", t(title)),
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                None,
            ));
            for row in range.clone() {
                lines.push((Line::raw(""), Some(row)));
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if row == selected && fields[row] == Field::StartupRegistration {
                    if let Some(context) = &self.startup_context {
                        // Activation instructions may be longer than the
                        // fixed footer. Keep every line in the scrollable
                        // viewport so narrow terminals can inspect them all.
                        let mut details = Vec::new();
                        if let Ok(path) = startup_registration::managed_path(context) {
                            let path: String = path
                                .display()
                                .to_string()
                                .chars()
                                .filter(|c| !c.is_control())
                                .collect();
                            details.push(Line::raw(path));
                        }
                        if self.preferences.startup_enabled {
                            if let Ok(instructions) = startup_registration::instructions(context) {
                                details.extend(
                                    instructions.lines().map(|line| Line::raw(line.to_owned())),
                                );
                            }
                        }
                        lines.extend(
                            program_info::wrap(details, inner.width.saturating_sub(1))
                                .into_iter()
                                .map(|line| (line, None)),
                        );
                    }
                }
            }
        }
        let description_lines = vec![
            Line::styled(
                t(fields[selected].description()).to_owned(),
                Style::default().fg(MUTED),
            ),
            Line::styled(
                self.preferences_path
                    .as_ref()
                    .map(|path| {
                        let path = path.display().to_string();
                        // Windows verbatim paths are correct but noisy to read.
                        let path = path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned();
                        tr!("Saved automatically to {}", path)
                    })
                    .unwrap_or_default(),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let description_lines = {
            let mut description_lines = description_lines;
            if fields[selected] == Field::StartupRegistration {
                // This field is backed by the generated OS file rather than
                // ui-preferences.json, whose path is shown for other fields.
                description_lines.truncate(1);
            }
            description_lines
        };
        let description = Paragraph::new(description_lines).wrap(Wrap { trim: true });
        let description_height = (description.line_count(inner.width.saturating_sub(2).max(1))
            as u16)
            .min(inner.height / 3);
        let list_height = inner.height.saturating_sub(description_height + 1).max(1) as usize;
        // Follow keyboard selection only after a preference action. On entry,
        // show the program information at the top even in a short terminal.
        let selected_line = lines
            .iter()
            .position(|(_, row)| *row == Some(selected))
            .unwrap_or(0);
        let view = &self.settings_view;
        view.height.set(list_height);
        view.total.set(lines.len());
        let mut offset = view.offset.get();
        if view.follow_selection.get() {
            if selected_line < offset {
                offset = selected_line;
            } else if selected_line >= offset.saturating_add(list_height) {
                offset = selected_line.saturating_sub(list_height.saturating_sub(1));
            }
        }
        offset = offset.min(lines.len().saturating_sub(list_height));
        view.offset.set(offset);
        view.selected_visible
            .set(selected_line >= offset && selected_line < offset + list_height);
        let focused = self.focus.region == focus::Region::Main
            && self.palette.is_none()
            && self.prompt.is_none();
        for (line_index, (line, row)) in lines.iter().enumerate().skip(offset).take(list_height) {
            let y = inner.y + (line_index - offset) as u16;
            let rect = Rect::new(inner.x, y, inner.width.saturating_sub(1), 1);
            let Some(row) = *row else {
                f.render_widget(Paragraph::new(line.clone()), rect);
                continue;
            };
            self.render_setting_row(f, rect, row, row == selected && focused, label_width);
        }
        if lines.len() > list_height {
            use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
            let mut state =
                ScrollbarState::new(lines.len().saturating_sub(list_height) + 1).position(offset);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                Rect::new(inner.x, inner.y, inner.width, list_height as u16),
                &mut state,
            );
        }
        if description_height > 0 {
            f.render_widget(
                description,
                Rect::new(
                    inner.x + 1,
                    inner.bottom() - description_height,
                    inner.width.saturating_sub(2),
                    description_height,
                ),
            );
        }
        self.render_terminal_picker(f, area);
    }

    fn render_setting_row(
        &self,
        f: &mut Frame<'_>,
        rect: Rect,
        row: usize,
        selected: bool,
        label_width: usize,
    ) {
        let field = ALL_FIELDS[row];
        let current = self.setting_index(field);
        let marker = if selected { " › " } else { "   " };
        let label = astra_tui_kit::text::pad_cells(t(field.label()), label_width);
        let marquee = self.animations.marquee();
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled(
                label,
                if selected && (marquee.text || marquee.background) {
                    // Give the item marquee something to run across here too.
                    crate::theme::selected().add_modifier(Modifier::BOLD)
                } else if selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ];
        let prefix: usize = spans.iter().map(Span::width).sum();
        if let Some(range) = field.range() {
            let arrow = |enabled: bool| {
                Style::default().fg(if enabled {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })
            };
            let value = format!(" {} ", field.value_label(&self.preferences));
            let value_width = Line::from(value.as_str()).width() as u16;
            let x = rect.x + prefix as u16;
            self.hit(
                Rect::new(x, rect.y, 2, 1).intersection(rect),
                Target::Setting(row, Some(0)),
            );
            self.hit(
                Rect::new(x + 2 + value_width, rect.y, 2, 1).intersection(rect),
                Target::Setting(row, Some(1)),
            );
            spans.push(Span::styled("‹ ", arrow(current > usize::from(range.min))));
            spans.push(Span::styled(
                value,
                Style::default()
                    .bg(BLUE)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(" ›", arrow(current < usize::from(range.max))));
            // Where the value sits in its range.
            let filled = (current.saturating_sub(usize::from(range.min)) * 10
                / usize::from(range.max - range.min).max(1))
            .min(10);
            spans.push(Span::styled(
                format!("  {}{}", "█".repeat(filled), "░".repeat(10 - filled)),
                Style::default().fg(Color::DarkGray),
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), rect);
            return;
        }
        if field == Field::Terminal {
            self.render_terminal_choice(f, rect, row, spans);
            return;
        }
        let swatch = |index: usize| (field == Field::Accent).then(|| Accent::ALL[index].color());
        let chip = |index: usize| -> Vec<Span<'static>> {
            let mut name = field.choice_label(index).to_owned();
            if field == Field::Colors && index == 0 {
                // Say what Auto resolved to, so a wrong detection is visible.
                name.push_str(&match self.animations.detected_colors() {
                    None => format!(" · {}", t("none")),
                    Some(u16::MAX) => " · 24-bit".into(),
                    Some(count) => format!(" · {count}"),
                });
            }
            let text = format!(" {name} ");
            let style = if index == current {
                Style::default()
                    .bg(BLUE)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default()
            } else {
                Style::default().fg(MUTED)
            };
            match swatch(index) {
                Some(color) => vec![
                    Span::styled("■", Style::default().fg(color)),
                    Span::styled(text, style),
                ],
                None => vec![Span::styled(text, style)],
            }
        };
        let chips: Vec<Vec<Span<'static>>> = (0..field.choices().len()).map(chip).collect();
        let full: usize = chips
            .iter()
            .map(|c| c.iter().map(Span::width).sum::<usize>() + 1)
            .sum();
        let mut x = rect.x + prefix as u16;
        if prefix + full <= rect.width as usize {
            for (index, chip) in chips.into_iter().enumerate() {
                let width = chip.iter().map(Span::width).sum::<usize>() as u16;
                self.hit(
                    Rect::new(x, rect.y, width, 1).intersection(rect),
                    Target::Setting(row, Some(index)),
                );
                x += width + 1;
                spans.extend(chip);
                spans.push(Span::raw(" "));
            }
        } else {
            // Too narrow for every choice: show the current one and its position.
            spans.push(Span::styled("‹", Style::default().fg(MUTED)));
            spans.extend(std::iter::once(current).flat_map(chip).collect::<Vec<_>>());
            spans.push(Span::styled("›", Style::default().fg(MUTED)));
            spans.push(Span::styled(
                format!(" {}/{}", current + 1, field.choices().len()),
                Style::default().fg(Color::DarkGray),
            ));
            self.hit(rect, Target::Setting(row, None));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), rect);
    }
}

#[cfg(test)]
#[path = "../test/ui_settings_test.rs"]
mod tests;
