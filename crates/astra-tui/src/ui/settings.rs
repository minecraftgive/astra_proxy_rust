//! Settings page: personal preferences shared by every window of this user.
//!
//! Preferences live in their own file next to the UI session, not in the
//! per-window session, so a change made in one window is what the next window
//! opens with. Every change applies immediately and is saved at once.
use super::dashboard::{Target, BLUE, MUTED};
use super::*;
use ratatui::{layout::Rect, style::Modifier};

mod program_info;

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
}
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
}
const FIELDS: [Field; 7] = [
    Field::Language,
    Field::Accent,
    Field::Colors,
    Field::Motion,
    Field::VimKeys,
    Field::Mouse,
    Field::StartPage,
];
const SECTIONS: [(&str, std::ops::Range<usize>); 2] = [("Appearance", 0..4), ("Interaction", 4..7)];

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
        }
    }
    /// Choice names; language names are shown in their own language.
    fn choices(self) -> &'static [&'static str] {
        match self {
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
            Self::VimKeys | Self::Mouse => &["On", "Off"],
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
        }
    }
    fn set(self, p: &mut Preferences, index: usize) {
        use animation::MotionMode;
        let index = index.min(self.choices().len() - 1);
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
    /// Change preferences, apply what differs and save them.
    pub(super) fn change_preferences(&mut self, change: impl FnOnce(&mut Preferences)) {
        let previous = self.preferences.clone();
        change(&mut self.preferences);
        if self.preferences == previous {
            return;
        }
        self.apply_preferences(Some(&previous));
        self.save_preferences();
    }
    fn save_preferences(&mut self) {
        let Some(path) = &self.preferences_path else {
            return;
        };
        let result = serde_json::to_vec_pretty(&self.preferences)
            .map_err(anyhow::Error::from)
            .and_then(|data| Ok(astra_platform::storage::atomic_write_default(path, &data)?));
        if let Err(error) = result {
            self.message = tr!("Cannot save preferences: {}", format!("{error:#}"));
        }
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
        // Mouse capture belongs to the terminal session, which the event loop owns.
        if previous.is_some_and(|old| old.mouse != p.mouse) || (previous.is_none() && !p.mouse) {
            self.mouse_request = Some(p.mouse);
        }
    }

    fn settings_field(&self) -> Field {
        FIELDS[self.settings_cursor.min(FIELDS.len() - 1)]
    }
    fn choose_setting(&mut self, field: Field, index: usize) {
        self.change_preferences(|p| field.set(p, index));
        self.message = format!(
            "{}: {}",
            t(field.label()),
            field.choice_label(field.get(&self.preferences))
        );
    }
    pub(super) fn click_setting(&mut self, row: usize, choice: Option<usize>) -> Input {
        self.focus = Default::default();
        self.settings_view.follow_selection.set(true);
        self.settings_cursor = row.min(FIELDS.len() - 1);
        let field = self.settings_field();
        let count = field.choices().len();
        let index = choice.unwrap_or((field.get(&self.preferences) + 1) % count);
        self.choose_setting(field, index);
        Input::Idle
    }

    /// Settings keys; `None` lets page numbers, the palette and quit through.
    pub(super) fn settings_key(&mut self, key: event::KeyEvent) -> Option<Input> {
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
        let current = field.get(&self.preferences);
        let count = field.choices().len();
        match key.code {
            KeyCode::Char('r') => {
                self.refresh_program_info();
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
                self.settings_cursor = (self.settings_cursor + 1).min(FIELDS.len() - 1)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.settings_cursor = 0;
                self.settings_view.offset.set(0);
                self.settings_view.follow_selection.set(false);
                return Some(Input::Idle);
            }
            KeyCode::End | KeyCode::Char('G') => self.settings_cursor = FIELDS.len() - 1,
            KeyCode::Left | KeyCode::Char('h') if current > 0 => {
                self.choose_setting(field, current - 1)
            }
            KeyCode::Right | KeyCode::Char('l') if current + 1 < count => {
                self.choose_setting(field, current + 1)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {}
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.choose_setting(field, (current + 1) % count)
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
        let selected = self.settings_cursor.min(FIELDS.len() - 1);
        let label_width = FIELDS
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
            }
        }
        let description = Paragraph::new(vec![
            Line::styled(
                t(FIELDS[selected].description()).to_owned(),
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
        ])
        .wrap(Wrap { trim: true });
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
    }

    fn render_setting_row(
        &self,
        f: &mut Frame<'_>,
        rect: Rect,
        row: usize,
        selected: bool,
        label_width: usize,
    ) {
        let field = FIELDS[row];
        let current = field.get(&self.preferences);
        let marker = if selected { " › " } else { "   " };
        let label = astra_tui_kit::text::pad_cells(t(field.label()), label_width);
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled(
                label,
                if selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ];
        let prefix: usize = spans.iter().map(Span::width).sum();
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
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn settings_ui() -> (Ui, tempdir::Dir) {
        let dir = tempdir::Dir::new();
        let mut ui = Ui::new(Session::default());
        ui.load_preferences(dir.path().join(FILE));
        ui.navigate(shell::SETTINGS);
        (ui, dir)
    }
    mod tempdir {
        pub(super) struct Dir(std::path::PathBuf);
        impl Dir {
            pub(super) fn new() -> Self {
                use std::sync::atomic::{AtomicU64, Ordering};
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "astra-settings-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub(super) fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
    fn press(ui: &mut Ui, code: KeyCode) {
        ui.key(event::KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn every_field_round_trips_each_choice() {
        for field in FIELDS {
            for index in 0..field.choices().len() {
                let mut p = Preferences::default();
                field.set(&mut p, index);
                assert_eq!(field.get(&p), index, "{field:?}");
            }
        }
    }
    #[test]
    fn keys_change_apply_and_persist_preferences() {
        let (mut ui, dir) = settings_ui();
        // Motion is the fourth row.
        for _ in 0..3 {
            press(&mut ui, KeyCode::Down);
        }
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.preferences.motion, animation::MotionMode::Reduced);
        assert_eq!(ui.animations.mode(), animation::MotionMode::Reduced);
        assert_eq!(ui.session.motion, animation::MotionMode::Reduced);
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Enter);
        assert!(ui.vim_keys);
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.mouse_request, Some(false));
        let saved = Preferences {
            motion: animation::MotionMode::Reduced,
            vim_keys: true,
            mouse: false,
            ..Default::default()
        };
        let (loaded, error) = load(&dir.path().join(FILE), &Session::default());
        assert!(error.is_none());
        assert_eq!(loaded, saved);
        // Delete restores the default of the selected row.
        press(&mut ui, KeyCode::Delete);
        assert!(ui.preferences.mouse);
        assert_eq!(ui.page, shell::SETTINGS, "keys stay on the page");
    }
    #[test]
    fn missing_file_keeps_session_choices_and_damaged_file_reports() {
        let dir = tempdir::Dir::new();
        let session = Session {
            motion: animation::MotionMode::Off,
            vim_keys: true,
            ..Default::default()
        };
        let (p, error) = load(&dir.path().join(FILE), &session);
        assert!(error.is_none());
        assert_eq!(p.motion, animation::MotionMode::Off);
        assert!(p.vim_keys);
        std::fs::write(dir.path().join(FILE), b"{ not json").unwrap();
        let (p, error) = load(&dir.path().join(FILE), &session);
        assert_eq!(p, Preferences::default());
        assert!(error.is_some());
    }
    #[test]
    fn page_renders_at_any_size_and_choices_are_clickable() {
        let (ui, _dir) = settings_ui();
        for (width, height) in [(30, 8), (80, 24), (140, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| ui.render(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                text.contains("Settings") || text.contains("设置"),
                "{width}x{height}"
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| ui.render(f)).unwrap();
        assert!(ui
            .hits
            .borrow()
            .iter()
            .any(|(_, hit)| matches!(hit, Target::Setting(1, Some(5)))));
    }
    #[test]
    fn accent_and_language_choices_apply() {
        let (mut ui, _dir) = settings_ui();
        ui.click_setting(1, Some(2));
        assert_eq!(ui.preferences.accent, Accent::Green);
        crate::i18n::with_language(crate::i18n::Language::English, || {
            assert_eq!(Field::Language.choice_label(2), "简体中文");
        });
    }
}
