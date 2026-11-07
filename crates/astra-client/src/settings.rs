//! Shared preference metadata, persistence, startup and terminal selection.
pub use crate::preferences::*;
pub mod program_info;
#[cfg(windows)]
use crate::startup as startup_registration;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::startup_unix as startup_registration;
use crate::{i18n::t, state::Client, tr};
use anyhow::{Context, Result};
use futures_util::FutureExt;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// Frontends consume these effects with their own rendering and input systems.
pub struct PreferenceEffects {
    pub palette: Option<(Accent, ColorDepth)>,
    pub motion: Option<crate::motion::MotionMode>,
    pub marquee: Option<crate::motion::Marquee>,
    pub mouse: Option<bool>,
}
pub fn effects(previous: Option<&Preferences>, p: &Preferences) -> PreferenceEffects {
    PreferenceEffects {
        palette: previous
            .is_none_or(|old| old.accent != p.accent || old.colors != p.colors)
            .then_some((p.accent, p.colors)),
        motion: previous
            .is_none_or(|old| old.motion != p.motion)
            .then_some(p.motion),
        marquee: previous
            .is_none_or(|old| old.marquee != p.marquee)
            .then_some(p.marquee.effect()),
        mouse: (previous.is_some_and(|old| old.mouse != p.mouse)
            || (previous.is_none() && !p.mouse))
            .then_some(p.mouse),
    }
}
/// How long Ctrl+Home must be held, and the longest gap between key repeats
/// that still counts as holding (the first repeat waits for the OS delay).
pub const HOLD: Duration = Duration::from_secs(3);
pub const HOLD_GAP: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Installation,
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
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    StartupFrontend,
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
pub const ALL_FIELDS: &[Field] = &[
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
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    Field::StartupFrontend,
    #[cfg(any(windows, target_os = "macos"))]
    Field::SilentStartup,
    #[cfg(windows)]
    Field::WindowsService,
    Field::Installation,
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
pub const MARQUEE_ROW: usize = 9
    + 4 * cfg!(windows) as usize
    + 3 * cfg!(target_os = "macos") as usize
    + 2 * cfg!(target_os = "linux") as usize;
/// Rows shown now; the marquee section stays hidden until it is unlocked.
pub fn fields(p: &Preferences) -> &'static [Field] {
    if p.marquee.unlocked {
        ALL_FIELDS
    } else {
        &ALL_FIELDS[..MARQUEE_ROW]
    }
}
pub const SECTIONS: [(&str, std::ops::Range<usize>); 5] = [
    ("Appearance", 0..4),
    ("Interaction", 4..7),
    ("Startup", 7..MARQUEE_ROW - 1),
    ("Installation", MARQUEE_ROW - 1..MARQUEE_ROW),
    ("Marquee", MARQUEE_ROW..ALL_FIELDS.len()),
];

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Self::Installation => "Install current package",
            Self::Language => "Language",
            Self::Accent => "Accent color",
            Self::Colors => "Colors",
            Self::Motion => "Motion",
            Self::VimKeys => "Vim navigation",
            Self::Mouse => "Mouse",
            Self::StartPage => "Start page",
            Self::Terminal => "Terminal",
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupFrontend => "Startup interface",
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupRegistration => {
                if cfg!(target_os = "macos") {
                    "Start Astra at macOS login"
                } else if cfg!(target_os = "linux") {
                    "systemd user service file"
                } else {
                    "Start Astra with Windows"
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
    pub fn description(self) -> &'static str {
        match self {
            Self::Installation => "Install a complete package already present on the selected management target. Review its location and installation scope before continuing.",
            Self::Language => "Interface language. System follows ASTRA_LANG, LANG and the OS locale; --lang on the command line wins for that run.",
            Self::Accent => "Color of the active page tab, key badges and selections.",
            Self::Colors => "Auto uses what the terminal reports. Pick fewer colors if highlights look wrong over SSH or in a basic console.",
            Self::Motion => "Normal animates overlays opening and closing. Reduced and Off show every change at once.",
            Self::VimKeys => "h j k l g G move in lists and logs, in addition to the arrow keys.",
            Self::Mouse => "Off returns the mouse to the terminal so you can select and copy text.",
            Self::StartPage => "Page shown when a window opens without --view.",
            Self::Terminal => "Choose the terminal for the next TUI launch, new windows and non-silent startup. The current window stays here. Silent startup does not use a terminal. Press r to scan again.",
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupFrontend => "Choose the graphical interface or terminal interface for future sign-in startup.",
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupRegistration => {
                if cfg!(target_os = "macos") {
                    "Open the selected interface at the next macOS login. Silent startup runs in the background. Instance auto_start settings control which instances start. No administrator permission is required."
                } else if cfg!(target_os = "linux") {
                    "Create or remove a user systemd service file for silent startup. Enable it manually with systemctl. Each instance config controls startup with auto_start. Removing the file does not stop running instances."
                } else {
                    "Open the selected interface when the current user signs in to Windows, using the current directories and manager target. Instance auto_start settings control which instances start."
                }
            }
            #[cfg(any(windows, target_os = "macos"))]
            Self::SilentStartup => "At sign-in, start in the background without an interface window. Instance auto_start settings control which instances start. Open Astra to manage them; failures are recorded in startup.log.",
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
    pub fn desktop_description(self) -> &'static str {
        match self {
            Self::Colors => "Full color, high contrast or grayscale appearance for the desktop interface.",
            Self::Mouse => "This preference controls mouse capture in the terminal interface.",
            Self::Terminal => "Choose the terminal used when opening the TUI. Scan again after installing a terminal.",
            Self::MarqueeSpeed => "Color movement speed in degrees per second. Reduced or Off motion keeps the banner still.",
            Self::MarqueeWidth => "Width of one color cycle. Wider values make a gentler gradient.",
            Self::MarqueeFps => "Banner animation frames per second. Higher values make motion smoother.",
            Self::MarqueeTrueColor => "Use full color for the banner regardless of the appearance color setting.",
            _ => self.description(),
        }
    }
    /// Choice names; language names are shown in their own language.
    pub fn choices(self) -> &'static [&'static str] {
        match self {
            Self::Installation => &["Open"],
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
            Self::StartupFrontend => &["TUI", "GUI"],
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
            Self::StartPage => &crate::navigation::PAGES,
        }
    }
    pub fn get(self, p: &Preferences) -> usize {
        use crate::motion::MotionMode;
        match self {
            Self::Installation => 0,
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
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupFrontend => p.startup_frontend as usize,
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
    pub fn set(self, p: &mut Preferences, index: usize) {
        use crate::motion::MotionMode;
        let index = match self.range() {
            Some(range) => usize::from(range.clamp(index.min(usize::from(u16::MAX)) as u16)),
            None => index.min(self.choices().len() - 1),
        };
        match self {
            Self::Installation => {}
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
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            Self::StartupFrontend => {
                p.startup_frontend = [StartupFrontend::Tui, StartupFrontend::Gui][index]
            }
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
    pub fn range(self) -> Option<Range> {
        match self {
            Self::MarqueeSpeed => Some(SPEED),
            Self::MarqueeWidth => Some(WIDTH),
            Self::MarqueeFps => Some(FPS),
            _ => None,
        }
    }
    /// The current value as shown in the row and in notices.
    pub fn value_label(self, p: &Preferences) -> String {
        match self.range() {
            Some(range) => format!("{} {}", self.get(p), t(range.unit)),
            None => self.choice_label(self.get(p)).to_owned(),
        }
    }
    pub fn choice_label(self, index: usize) -> &'static str {
        let name = self.choices()[index];
        if self == Self::Language && index > 0 {
            name
        } else {
            t(name)
        }
    }
}

#[derive(Default)]
pub struct TerminalState {
    pub candidates: Vec<crate::terminals::Candidate>,
    pub scan: Option<tokio::task::JoinHandle<Vec<crate::terminals::Candidate>>>,
    pub launch: Option<tokio::task::JoinHandle<Result<(), String>>>,
    pub pending: bool,
    pub inside_settings: bool,
    pub picker: Option<String>,
}

pub struct Choice {
    pub id: String,
    pub label: String,
    pub available: bool,
}

impl Client {
    pub fn refresh_program_info(&mut self) {
        self.program_info.refresh = true;
    }
    /// Load preferences at startup and apply all of them.
    pub fn load_preferences(&mut self, path: PathBuf) {
        let (preferences, error) = load(&path, &self.session);
        self.preferences_path = Some(path);
        self.preferences = preferences;
        self.apply_preferences(None);
        if let Some(error) = error {
            self.message = error;
        }
    }
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub fn load_startup_context(&mut self, context: &crate::windows::Context) {
        self.startup_context = Some(context.clone());
        match context
            .for_frontend(self.preferences.startup_frontend)
            .and_then(|context| startup_registration::enabled(&context))
        {
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
    pub fn change_windows_service(&mut self, enabled: bool) {
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
        let waker = self.waker.clone();
        self.service_change = Some(tokio::task::spawn_blocking(move || {
            let result = astra_platform::windows_service::set_installed(&bin, enabled)
                .and_then(|_| astra_platform::windows_service::installed())
                .map_err(|error| error.to_string());
            waker.wake();
            result
        }));
        self.message = t("Approve the Windows UAC prompt to change the privilege service").into();
    }

    #[cfg(windows)]
    pub async fn poll_service_change(&mut self) -> bool {
        self.poll_service_change_ready()
    }

    #[cfg(windows)]
    pub fn poll_service_change_ready(&mut self) -> bool {
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
            .now_or_never()
            .expect("finished service registration")
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
    pub fn change_preferences(&mut self, change: impl FnOnce(&mut Preferences)) -> bool {
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
    pub fn save_preferences(&self) -> Result<()> {
        let Some(path) = &self.preferences_path else {
            return Ok(());
        };
        let data = serde_json::to_vec_pretty(&self.preferences)?;
        astra_platform::storage::atomic_write_default(path, &data)?;
        Ok(())
    }
    pub fn apply_preferences(&mut self, previous: Option<&Preferences>) {
        let p = self.preferences.clone();
        if previous.is_none_or(|old| old.language != p.language)
            && !(previous.is_none() && crate::i18n::command_line_language())
        {
            crate::i18n::set_language(p.language.language());
        }
        self.session.motion = p.motion;
        self.session.vim_keys = p.vim_keys;
        for viewer in [&mut self.viewer, &mut self.tui_viewer]
            .into_iter()
            .flatten()
        {
            viewer.set_vim_navigation(p.vim_keys);
        }
    }
    pub fn marquee_hold_at(&mut self, now: Instant) -> Option<bool> {
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
                return None;
            }
            self.message = t(if unlock {
                "Marquee unlocked"
            } else {
                "Marquee hidden"
            })
            .into();
            return Some(unlock);
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
        None
    }
    pub fn choose_setting(&mut self, field: Field, index: usize) {
        if field == Field::Installation {
            self.open_installation();
            return;
        }
        if field == Field::Terminal {
            self.choose_terminal(index);
            return;
        }
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        if field == Field::StartupFrontend {
            self.choose_startup_frontend(index);
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
                .and_then(|context| context.for_frontend(self.preferences.startup_frontend))
                .and_then(|context| {
                    if index == 0 {
                        anyhow::ensure!(
                            context.executable.is_file(),
                            "{}: {}",
                            t("Interface executable is unavailable"),
                            context.executable.display()
                        );
                    }
                    startup_registration::set_enabled(
                        &context,
                        index == 0,
                        self.preferences.silent_startup,
                    )
                });
            match result {
                Ok(()) => self.preferences.startup_enabled = index == 0,
                Err(error) => {
                    if let Some(context) = &self.startup_context {
                        if let Ok(enabled) = context
                            .for_frontend(self.preferences.startup_frontend)
                            .and_then(|context| startup_registration::enabled(&context))
                        {
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
                if let Ok(path) = context
                    .for_frontend(self.preferences.startup_frontend)
                    .and_then(|context| startup_registration::managed_path(&context))
                {
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
                .and_then(|context| {
                    context
                        .for_frontend(self.preferences.startup_frontend)
                        .and_then(|context| {
                            startup_registration::set_enabled(&context, true, index == 0)
                        })
                });
            if let Err(error) = result {
                self.message = tr!(
                    "Cannot change startup registration: {}",
                    format!("{error:#}")
                );
                return;
            }
            if !self.change_preferences(|p| p.silent_startup = index == 0) {
                if let Some(context) = &self.startup_context {
                    if let Err(error) = context
                        .for_frontend(self.preferences.startup_frontend)
                        .and_then(|context| {
                            startup_registration::set_enabled(&context, true, previous)
                        })
                    {
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
    /// Extra instructions for settings whose persisted state belongs to the operating system.
    pub fn setting_details(&self, field: Field) -> Vec<String> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if field == Field::StartupRegistration {
            let mut details = Vec::new();
            if let Some(context) = &self.startup_context {
                if let Ok(path) = context
                    .for_frontend(self.preferences.startup_frontend)
                    .and_then(|context| startup_registration::managed_path(&context))
                {
                    details.push(
                        path.display()
                            .to_string()
                            .chars()
                            .filter(|c| !c.is_control())
                            .collect(),
                    );
                }
                if self.preferences.startup_enabled {
                    if let Ok(instructions) = context
                        .for_frontend(self.preferences.startup_frontend)
                        .and_then(|context| startup_registration::instructions(&context))
                    {
                        details.extend(instructions.lines().map(str::to_owned));
                    }
                }
            }
            return details;
        }
        let _ = field;
        Vec::new()
    }
    pub fn setting_save_location(&self, field: Field) -> Option<String> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if field == Field::StartupRegistration {
            return None;
        }
        let _ = field;
        Some(
            self.preferences_path
                .as_ref()
                .map(|path| {
                    let path = path.display().to_string();
                    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
                    tr!("Saved automatically to {}", path)
                })
                .unwrap_or_default(),
        )
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub fn choose_startup_frontend(&mut self, index: usize) {
        let previous = self.preferences.startup_frontend;
        let next = [StartupFrontend::Tui, StartupFrontend::Gui][index.min(1)];
        if previous == next {
            return;
        }
        let result = (|| -> Result<()> {
            if self.preferences.startup_enabled {
                let base = self
                    .startup_context
                    .as_ref()
                    .context("Startup context is unavailable")?;
                let old = base.for_frontend(previous)?;
                let new = base.for_frontend(next)?;
                anyhow::ensure!(
                    new.executable.is_file(),
                    "{}: {}",
                    t("Interface executable is unavailable"),
                    new.executable.display()
                );
                startup_registration::set_enabled(&new, true, self.preferences.silent_startup)?;
                if let Err(error) =
                    startup_registration::set_enabled(&old, false, self.preferences.silent_startup)
                {
                    let _ = startup_registration::set_enabled(
                        &new,
                        false,
                        self.preferences.silent_startup,
                    );
                    return Err(error);
                }
                if !self.change_preferences(|p| p.startup_frontend = next) {
                    startup_registration::set_enabled(&old, true, self.preferences.silent_startup)?;
                    startup_registration::set_enabled(
                        &new,
                        false,
                        self.preferences.silent_startup,
                    )?;
                    return Ok(());
                }
            } else if !self.change_preferences(|p| p.startup_frontend = next) {
                return Ok(());
            }
            self.message = format!(
                "{}: {}",
                t(Field::StartupFrontend.label()),
                Field::StartupFrontend.value_label(&self.preferences)
            );
            Ok(())
        })();
        if let Err(error) = result {
            self.message = tr!(
                "Cannot change startup registration: {}",
                format!("{error:#}")
            );
        }
    }
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub fn start_terminal_interface(&mut self) -> Result<()> {
        let context = self
            .startup_context
            .as_ref()
            .context("Startup context is unavailable")?
            .for_frontend(StartupFrontend::Tui)?;
        anyhow::ensure!(
            context.executable.is_file(),
            "{}: {}",
            t("Interface executable is unavailable"),
            context.executable.display()
        );
        let launch = crate::windows::Launch::detached(
            crate::windows::View::from_page(self.page),
            self.active_profile.clone(),
            self.kind,
        );
        if self.terminals.launch.is_some() {
            return Ok(());
        }
        let terminal = self.preferences.terminal.clone();
        let waker = self.waker.clone();
        self.terminals.launch = Some(tokio::task::spawn_blocking(move || {
            let result = context
                .open(&launch, &terminal)
                .map_err(|error| format!("{error:#}"));
            waker.wake();
            result
        }));
        Ok(())
    }

    pub fn terminal_choices(&self) -> Vec<Choice> {
        let mut choices = vec![Choice {
            id: "auto".into(),
            label: t("Auto").into(),
            available: true,
        }];
        #[cfg(windows)]
        choices.push(Choice {
            id: "native".into(),
            label: t("Native console").into(),
            available: true,
        });
        for candidate in &self.terminals.candidates {
            if !choices.iter().any(|choice| choice.id == candidate.id) {
                choices.push(Choice {
                    id: candidate.id.clone(),
                    label: candidate
                        .label
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect(),
                    available: true,
                });
            }
        }
        if !choices
            .iter()
            .any(|choice| choice.id == self.preferences.terminal)
        {
            let id = &self.preferences.terminal;
            choices.push(Choice {
                id: id.clone(),
                label: format!(
                    "{} · {}",
                    id.chars().filter(|c| !c.is_control()).collect::<String>(),
                    t("Unavailable")
                ),
                available: false,
            });
        }
        choices
    }

    pub fn setting_index(&self, field: Field) -> usize {
        if field == Field::Terminal {
            self.terminal_choices()
                .iter()
                .position(|c| c.id == self.preferences.terminal)
                .unwrap_or(0)
        } else {
            field.get(&self.preferences)
        }
    }

    pub fn setting_choice_count(&self, field: Field) -> usize {
        if field == Field::Terminal {
            self.terminal_choices().len()
        } else {
            field.choices().len()
        }
    }

    pub fn choose_terminal(&mut self, index: usize) {
        let choices = self.terminal_choices();
        let Some(choice) = choices.get(index) else {
            return;
        };
        if !choice.available {
            self.message =
                t("This terminal is unavailable. Scan again or choose another terminal.").into();
            return;
        }
        if self.change_preferences(|p| p.terminal.clone_from(&choice.id)) {
            self.terminals.picker = None;
            self.message = tr!("Terminal for future windows: {}", &choice.label);
        }
    }

    pub fn request_terminal_scan(&mut self) {
        self.terminals.pending = true;
        self.message = t("Scanning installed terminals…").into();
    }

    /// Called by the event loop, never by rendering or keyboard handlers.
    pub async fn poll_terminal_scan(&mut self) -> bool {
        self.poll_terminal_scan_ready()
    }

    pub fn poll_terminal_scan_ready(&mut self) -> bool {
        let inside = self.page == crate::navigation::SETTINGS;
        if inside && !self.terminals.inside_settings {
            self.terminals.pending = true;
        }
        self.terminals.inside_settings = inside;
        if !inside {
            self.terminals.picker = None;
        }
        let mut changed = false;
        if self
            .terminals
            .launch
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let result = self
                .terminals
                .launch
                .take()
                .unwrap()
                .now_or_never()
                .expect("finished terminal launch")
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            if let Err(error) = result {
                self.message = error;
            }
            changed = true;
        }
        if self
            .terminals
            .scan
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            match self
                .terminals
                .scan
                .take()
                .unwrap()
                .now_or_never()
                .expect("finished terminal scan")
            {
                Ok(candidates) => {
                    self.terminals.candidates = candidates;
                    if inside {
                        let count = self
                            .terminals
                            .candidates
                            .iter()
                            .filter(|c| c.executable.is_some())
                            .count();
                        self.message = tr!("Found {} installed terminals", count);
                    }
                }
                Err(error) => self.message = tr!("Cannot scan terminals: {}", error.to_string()),
            }
            changed = true;
        }
        if self.terminals.pending && self.terminals.scan.is_none() {
            self.terminals.pending = false;
            let waker = self.waker.clone();
            self.terminals.scan = Some(tokio::task::spawn_blocking(move || {
                let candidates = crate::terminals::discover();
                waker.wake();
                candidates
            }));
            changed = true;
        }
        changed
    }

    pub fn open_terminal_picker(&mut self) {
        self.terminals.picker = Some(self.preferences.terminal.clone());
    }
}

#[cfg(test)]
#[path = "test/settings_test.rs"]
mod tests;
