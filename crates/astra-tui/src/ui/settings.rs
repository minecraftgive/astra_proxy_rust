//! Settings page: personal preferences shared by every window of this user.
//!
//! Preferences live in their own file next to the UI session, not in the
//! per-window session, so a change made in one window is what the next window
//! opens with. Every change applies immediately and is saved at once.
use super::dashboard::{Target, BLUE, MUTED};
use super::*;
use ratatui::{layout::Rect, style::Modifier};

mod program_info;
mod terminals;

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

pub use astra_client::settings::*;
pub(super) trait AccentColor {
    fn color(self) -> Color;
}
impl AccentColor for Accent {
    /// Every accent keeps white text readable on top of it.
    fn color(self) -> Color {
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
impl Ui {
    pub(super) fn load_preferences(&mut self, path: PathBuf) {
        self.client.load_preferences(path);
        self.apply_preferences(None);
    }
    #[cfg(test)]
    pub(super) fn change_preferences(&mut self, change: impl FnOnce(&mut Preferences)) -> bool {
        let previous = self.client.preferences.clone();
        let changed = self.client.change_preferences(change);
        self.apply_preferences(Some(&previous));
        changed
    }
    pub(super) fn apply_preferences(&mut self, previous: Option<&Preferences>) {
        let effects = effects(previous, &self.client.preferences);
        if let Some((accent, colors)) = effects.palette {
            self.animations.set_palette(accent.color(), colors);
        }
        if let Some(motion) = effects.motion {
            self.animations.set_mode(motion);
        }
        if let Some(marquee) = effects.marquee {
            self.animations.set_marquee(marquee);
        }
        if let Some(mouse) = effects.mouse {
            self.mouse_request = Some(mouse);
        }
    }
    fn choose_setting(&mut self, field: Field, index: usize) {
        let previous = self.client.preferences.clone();
        self.client.choose_setting(field, index);
        self.apply_preferences(Some(&previous));
    }
    fn marquee_hold_at(&mut self, now: Instant) {
        let previous = self.client.preferences.clone();
        if self.client.marquee_hold_at(now) == Some(true) {
            self.settings_cursor = MARQUEE_ROW;
            self.settings_view.follow_selection.set(true);
        }
        self.apply_preferences(Some(&previous));
    }
    fn settings_field(&self) -> Field {
        let fields = fields(&self.client.preferences);
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
        let here = self.client.page == shell::SETTINGS
            && self.settings_field() == Field::Accent
            && self.focus.region == focus::Region::Main
            && self.palette.is_none()
            && self.prompt.is_none()
            && self.menu.is_none()
            && self.client.help_scroll.is_none();
        if !(chord && here) {
            self.marquee_hold = None;
            return None;
        }
        self.marquee_hold_at(Instant::now());
        Some(Input::Idle)
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
    pub(super) fn click_setting(&mut self, row: usize, choice: Option<usize>) -> Input {
        self.focus = Default::default();
        self.settings_view.follow_selection.set(true);
        self.settings_cursor = row.min(fields(&self.client.preferences).len() - 1);
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
                    (self.settings_cursor + 1).min(fields(&self.client.preferences).len() - 1)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.settings_cursor = 0;
                self.settings_view.offset.set(0);
                self.settings_view.follow_selection.set(false);
                return Some(Input::Idle);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.settings_cursor = fields(&self.client.preferences).len() - 1
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
        let fields = fields(&self.client.preferences);
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
                if row == selected {
                    let details = self
                        .client
                        .setting_details(fields[row])
                        .into_iter()
                        .map(Line::raw)
                        .collect();
                    lines.extend(
                        program_info::wrap(details, inner.width.saturating_sub(1))
                            .into_iter()
                            .map(|line| (line, None)),
                    );
                }
            }
        }
        let mut description_lines = vec![Line::styled(
            t(fields[selected].description()).to_owned(),
            Style::default().fg(MUTED),
        )];
        if let Some(location) = self.client.setting_save_location(fields[selected]) {
            description_lines.push(Line::styled(location, Style::default().fg(Color::DarkGray)));
        }
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
            let value = format!(" {} ", field.value_label(&self.client.preferences));
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
