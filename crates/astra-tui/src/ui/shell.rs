//! Shared frame matching the GUI: status bar, page bar, hint line and command palette.
//! Every page except Profiles follows the active profile, the selected proxy instance.
use super::dashboard::{Target, BLUE, MUTED};
use super::*;

use astra_model::runtime::Mode;
use ratatui::{layout::Rect, style::Modifier};

pub use astra_client::navigation::{
    CONNECTIONS, DNS, LOGS, OVERVIEW, PROFILES, PROXIES, RULES, SETTINGS,
};
// Display order is independent of the shared page IDs used by the native GUI.
pub const PAGES: [(usize, &str); 8] = [
    (OVERVIEW, "Overview"),
    (PROXIES, "Proxies"),
    (CONNECTIONS, "Connections"),
    (DNS, "DNS queries"),
    (RULES, "Rules"),
    (LOGS, "Logs"),
    (PROFILES, "Profiles"),
    (SETTINGS, "Settings"),
];
pub(super) fn page_at(index: usize) -> usize {
    PAGES[index.min(PAGES.len() - 1)].0
}
pub(super) fn page_position(page: usize) -> usize {
    PAGES.iter().position(|&(id, _)| id == page).unwrap_or(0)
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) enum PaletteFocus {
    #[default]
    Search,
    Type,
    List,
}
#[derive(Clone)]
pub(super) struct Palette {
    pub(super) query: String,
    pub(super) cursor: usize,
    pub(super) selected: usize,
    /// Opened with Ctrl-P: choose a proxy profile or a rule core.
    pub(super) profiles: bool,
    pub(super) profile_kind: Kind,
    pub(super) focus: PaletteFocus,
    pub(super) viewport_height: std::cell::Cell<u16>,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            selected: 0,
            profiles: false,
            profile_kind: Kind::Proxy,
            focus: PaletteFocus::Search,
            viewport_height: std::cell::Cell::new(1),
        }
    }
}
impl Palette {
    pub(super) fn profiles(kind: Kind) -> Self {
        Self {
            profiles: true,
            profile_kind: kind,
            ..Default::default()
        }
    }
    fn byte(&self) -> usize {
        self.query
            .char_indices()
            .nth(self.cursor)
            .map_or(self.query.len(), |(index, _)| index)
    }
    pub(super) fn insert(&mut self, text: &str) {
        self.focus = PaletteFocus::Search;
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.query.insert_str(self.byte(), &text);
        self.cursor += text.chars().count();
        self.selected = 0;
    }
}

use astra_client::palette::{Entry, PaletteMode};

pub use astra_client::format::duration;

impl Ui {
    fn palette_mode(&self) -> PaletteMode {
        self.palette
            .as_ref()
            .filter(|p| p.profiles)
            .map_or(PaletteMode::Commands, |p| {
                PaletteMode::Profiles(p.profile_kind)
            })
    }
    fn palette_entries_in(&self, language: crate::i18n::Language, mode: PaletteMode) -> Vec<Entry> {
        use astra_client::{i18n::translate, palette::Intent};
        let mut entries = self.client.palette_entries_in(language, mode);
        if matches!(mode, PaletteMode::Commands) {
            let at = entries
                .iter()
                .position(|entry| matches!(entry.run, Intent::Page(_)))
                .unwrap_or(entries.len());
            entries.retain(|entry| !matches!(entry.run, Intent::Page(_)));
            entries.splice(
                at..at,
                PAGES
                    .iter()
                    .enumerate()
                    .map(|(index, &(page, title))| Entry {
                        scope: "Go to",
                        label: translate(title, language).into(),
                        key: ["1", "2", "3", "4", "5", "6", "7", "8"][index],
                        run: Intent::Page(page),
                        unavailable: None,
                    }),
            );
        }
        entries
    }
    fn palette_matches(&self) -> Vec<Entry> {
        use astra_client::i18n::{language, translate, Language};
        let query = self
            .palette
            .as_ref()
            .map_or("", |p| p.query.as_str())
            .to_lowercase();
        let current = language();
        let matches = |entry: &Entry, language| {
            format!(
                "{} {} {}",
                translate(entry.scope, language),
                entry.label,
                entry.key
            )
            .to_lowercase()
            .contains(&query)
        };
        self.palette_entries_in(current, self.palette_mode())
            .into_iter()
            .zip(self.palette_entries_in(Language::English, self.palette_mode()))
            .filter_map(|(entry, english)| {
                (matches(&entry, current) || matches(&english, Language::English)).then_some(entry)
            })
            .collect()
    }
    pub(super) fn command_help(&self) -> String {
        self.help_override.clone().unwrap_or_else(|| {
            let mut text = commands::help().replace("1–7", "1–8");
            text.push_str("\nPage and profile actions · F2 opens these commands\n");
            for entry in self
                .palette_entries_in(crate::i18n::language(), PaletteMode::Commands)
                .into_iter()
                .filter(|entry| entry.scope != "General")
            {
                text.push_str(&format!(
                    "{}  {}{}\n",
                    entry.key,
                    entry.label,
                    entry
                        .unavailable
                        .map(|reason| format!(" ({})", t(reason)))
                        .unwrap_or_default()
                ));
            }
            text
        })
    }
    pub(super) fn palette_run(&mut self, index: usize) -> Input {
        let Some(entry) = self.palette_matches().into_iter().nth(index) else {
            return Input::Idle;
        };
        if entry.unavailable.is_some() {
            return self.client.run_palette_entry(entry);
        }
        self.palette = None;
        self.focus = Default::default();
        match self.client.run_palette_entry(entry) {
            Input::Frontend(id) => self.run_command(id),
            input => input,
        }
    }

    /// Page switching is allowed whenever no dialog, editor or text input owns the keyboard.
    pub(super) fn can_switch_page(&self) -> bool {
        self.editor.as_ref().is_none_or(Editor::browsing)
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.menu.is_none()
            && self.binding_picker.is_none()
            && self.diagnostics.is_none()
            && self.explanation.is_none()
            && self.help_scroll.is_none()
            && self.palette.is_none()
            && self.monitor.is_none()
            && self.viewer.as_ref().is_none_or(logs::Viewer::browsing)
    }
    /// Keys shared by every page; `None` lets the page handle the key.
    pub(super) fn page_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if (control && key.code == KeyCode::Char('k')) || key.code == KeyCode::Char(':') {
            self.palette = Some(Palette::default());
            return Some(Input::Idle);
        }
        let find = (control && key.code == KeyCode::Char('f'))
            || (key.code == KeyCode::Char('/') && !control);
        if find {
            match self.page {
                PROXIES | PROFILES => {
                    let page = self.page;
                    self.prompt(Prompt::Find(page));
                    return Some(Input::Idle);
                }
                RULES => return Some(self.rule_search()),
                CONNECTIONS => {
                    self.connection_filtering = true;
                    return Some(Input::Idle);
                }
                _ => {}
            }
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='8') => {
                return Some(self.navigate(page_at(c as usize - '1' as usize)))
            }
            KeyCode::Char('p') => {
                self.palette = Some(Palette::profiles(self.kind));
                return Some(Input::Idle);
            }
            _ => {}
        }
        if self.page == PROFILES || self.viewer.is_some() {
            return None;
        }
        if self.kind == Kind::Server {
            return self.server_key(key);
        }
        Some(match (self.page, key.code) {
            (_, KeyCode::Char('q')) => Input::Quit,
            (_, KeyCode::Char('c')) => self.toggle_active(),
            (_, KeyCode::Char('r')) => self.active_operation(Operation::Reload),
            (_, KeyCode::Char('U')) => self.active_operation(Operation::UpdateSubscriptions),
            (OVERVIEW | PROXIES, KeyCode::Char('e')) => self.active_operation(Operation::Edit),
            (OVERVIEW | PROXIES, KeyCode::Char('D')) => {
                self.active_operation(Operation::Diagnostics)
            }
            (OVERVIEW | PROXIES, KeyCode::Char('N')) => self.active_operation(Operation::NetTools),
            (OVERVIEW, KeyCode::Char('L')) => {
                match self.active_proxy().map(|r| r.instance.name.clone()) {
                    Some(name) => self.open_logs(name),
                    None => Input::Idle,
                }
            }
            (RULES, KeyCode::Char('t') | KeyCode::Enter) => self.rule_operation(Operation::Explain),
            (RULES, KeyCode::Char('e')) => self.rule_operation(Operation::Edit),
            (LOGS, KeyCode::Enter | KeyCode::Char('[' | ']')) => {
                match self.log_source(key.code != KeyCode::Char(']')) {
                    Some(name) => self.open_logs(name),
                    None => Input::Idle,
                }
            }
            _ => return None,
        })
    }
    /// `[` / `]` and page numbers while a log is open on the Logs page.
    pub(super) fn viewer_page_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if self.page == LOGS
            && key.code == KeyCode::Char('f')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let viewer = self.viewer.as_mut().filter(|v| v.browsing())?;
            viewer.begin_search();
            return Some(Input::Idle);
        }
        if self.page != LOGS
            || !self.viewer.as_ref().is_some_and(logs::Viewer::browsing)
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('[' | ']') => {
                let name = self.log_source(key.code == KeyCode::Char('['))?;
                Some(self.open_logs(name))
            }
            KeyCode::Char(c @ '1'..='8') => Some(self.navigate(page_at(c as usize - '1' as usize))),
            KeyCode::Char(':') => {
                self.palette = Some(Palette::default());
                Some(Input::Idle)
            }
            _ => None,
        }
    }

    pub(super) fn palette_key(&mut self, key: event::KeyEvent) -> Input {
        let count = self.palette_matches().len();
        let palette = self.palette.as_mut().unwrap();
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let regions: &[PaletteFocus] = if palette.profiles {
                &[PaletteFocus::Search, PaletteFocus::Type, PaletteFocus::List]
            } else {
                &[PaletteFocus::Search, PaletteFocus::List]
            };
            let index = regions
                .iter()
                .position(|focus| *focus == palette.focus)
                .unwrap_or(0);
            let next = if key.code == KeyCode::BackTab {
                (index + regions.len() - 1) % regions.len()
            } else {
                (index + 1) % regions.len()
            };
            palette.focus = regions[next];
            return Input::Idle;
        }
        if palette.profiles && palette.focus == PaletteFocus::Type {
            match key.code {
                KeyCode::Left | KeyCode::Up => {
                    palette.profile_kind = Kind::Proxy;
                    palette.selected = 0;
                }
                KeyCode::Right | KeyCode::Down => {
                    palette.profile_kind = next_kind(palette.profile_kind);
                    palette.selected = 0;
                }
                KeyCode::Enter => palette.focus = PaletteFocus::List,
                KeyCode::Esc => self.palette = None,
                _ => {}
            }
            return Input::Idle;
        }
        let page_size = usize::from(palette.viewport_height.get().max(1));
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.palette = None
            }
            KeyCode::Down => palette.selected = (palette.selected + 1).min(count.saturating_sub(1)),
            KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
            KeyCode::PageDown => {
                palette.selected = (palette.selected + page_size).min(count.saturating_sub(1))
            }
            KeyCode::PageUp => palette.selected = palette.selected.saturating_sub(page_size),
            KeyCode::Home if palette.focus == PaletteFocus::List => palette.selected = 0,
            KeyCode::End if palette.focus == PaletteFocus::List => {
                palette.selected = count.saturating_sub(1)
            }
            KeyCode::Left if palette.focus == PaletteFocus::Search => {
                palette.cursor = palette.cursor.saturating_sub(1)
            }
            KeyCode::Right if palette.focus == PaletteFocus::Search => {
                palette.cursor = (palette.cursor + 1).min(palette.query.chars().count())
            }
            KeyCode::Home => palette.cursor = 0,
            KeyCode::End => palette.cursor = palette.query.chars().count(),
            KeyCode::Backspace if palette.focus == PaletteFocus::Search && palette.cursor > 0 => {
                palette.cursor -= 1;
                palette.query.remove(palette.byte());
                palette.selected = 0;
            }
            KeyCode::Delete
                if palette.focus == PaletteFocus::Search
                    && palette.cursor < palette.query.chars().count() =>
            {
                palette.query.remove(palette.byte());
                palette.selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                palette.query.clear();
                palette.cursor = 0;
                palette.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                palette.insert(&c.to_string());
            }
            KeyCode::Enter => {
                let index = palette.selected;
                return self.palette_run(index);
            }
            _ => {}
        }
        Input::Idle
    }

    /// Status bar, page bar and hint line; returns the page content area.
    pub(super) fn render_frame(&self, f: &mut Frame<'_>) -> Rect {
        let area = f.area();
        let rows = Layout::vertical([
            Constraint::Length(if self.kind == Kind::Server { 1 } else { 2 }),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
        self.render_status_bar(f, rows[0]);
        self.render_page_bar(f, rows[1]);
        let (content, hint) = (rows[2], rows[3]);
        // This page's keys, whole entries only; results live in the status line below.
        let hint_text = format!("[{}] Tab focus · {}", self.focus_label(), self.hint());
        let global = Line::from(crate::theme::key_hints(t("F2 / Ctrl-K commands · F1 help")));
        let global_width = global.width() as u16;
        let hints = if hint.width >= global_width + 22 {
            f.render_widget(
                Paragraph::new(global),
                Rect::new(
                    hint.right() - global_width,
                    hint.y,
                    global_width,
                    hint.height,
                ),
            );
            Rect::new(hint.x, hint.y, hint.width - global_width - 2, hint.height)
        } else {
            hint
        };
        let spans = crate::theme::key_hints_fit(&hint_text, hints.width as usize, t("F1 more"));
        f.render_widget(Paragraph::new(Line::from(spans)), hints);
        content
    }
    /// The same two header rows on every page: profile, mode and exit with upload
    /// rates; listeners, switches and telemetry health with download rates.
    fn render_status_bar(&self, f: &mut Frame<'_>, area: Rect) {
        if self.kind == Kind::Server {
            self.render_server_header(f, area);
            return;
        }
        if area.width == 0 || area.height == 0 {
            return;
        }
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(MUTED);
        let dark = Style::default().fg(Color::DarkGray);
        let sep = || Span::styled(" │ ", dark);
        let brand = vec![Span::styled(" Astra", bold.fg(BLUE)), sep()];
        let Some(row) = self.active_proxy() else {
            let mut spans = brand;
            spans.push(Span::styled(
                t("No profile yet · 7 then n to create one"),
                muted,
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        };
        let failed = row.state.starts_with("Exited")
            || row.state.starts_with("Unresponsive")
            || row.state.starts_with("Failed");
        let (dot, color, state) = if failed {
            ("●", Color::Red, t("Failed"))
        } else if row.pid.is_some() {
            ("●", Color::Green, t("Profile connected"))
        } else {
            ("○", MUTED, t("Not connected"))
        };
        let color = self.indicator_color(color);
        let live = self.live();
        let width = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();

        // Row one, richest first: a narrow terminal drops the exit IP, uptime,
        // intermediate hops, the state word, the exit and finally the mode.
        let mut connect = (0, 0);
        let first = |level: usize, connect: &mut (usize, usize)| {
            let mut spans = brand.clone();
            connect.0 = width(&spans);
            spans.push(Span::styled(format!("{dot} "), Style::default().fg(color)));
            spans.push(Span::styled(row.instance.name.clone(), bold));
            if level < 4 {
                spans.push(Span::styled(
                    format!(" {state}"),
                    Style::default().fg(color),
                ));
            }
            if level < 2 && row.pid.is_some() {
                spans.push(Span::styled(format!(" {}", duration(row.uptime)), muted));
            }
            connect.1 = width(&spans);
            if level >= 6 {
                return spans;
            }
            spans.push(sep());
            let Some(snapshot) = live else {
                spans.push(Span::styled(t("Waiting for runtime data…"), muted));
                return spans;
            };
            let (mode, mode_color) = match snapshot.mode {
                Mode::Rule => ("Rule", Color::Cyan),
                Mode::Global => ("Global", BLUE),
                Mode::Direct => ("Direct", Color::Green),
            };
            spans.push(Span::styled(t(mode), bold.fg(mode_color)));
            let (chain, _, _) = live::exit_chain(snapshot);
            if level >= 5 {
                return spans;
            }
            if !chain.is_empty() {
                spans.push(Span::styled(format!(" {} ", t("Exit")), muted));
                let skip = if level < 3 { 0 } else { chain.len() - 1 };
                for (index, hop) in chain.iter().enumerate().skip(skip) {
                    if index > skip {
                        spans.push(Span::styled(" ▸ ", dark));
                    }
                    spans.push(if index + 1 == chain.len() {
                        Span::styled(hop.clone(), bold.fg(BLUE))
                    } else {
                        Span::raw(hop.clone())
                    });
                }
                // Shared rule: a known result always shows (every connect and
                // switch probes once); an enabled setting keeps its place.
                let exit = chain.last().map(String::as_str).unwrap_or_default();
                let (ip, latency) = self.exit_probe_fields(&row.instance.name, exit);
                let mut fields = vec![];
                if let Some(ip) = ip.filter(|_| level < 1) {
                    fields.push(Span::styled(format!("ip: {ip}"), muted));
                }
                if let Some(latency) = latency {
                    use astra_client::overview::ExitLatency;
                    fields.push(match latency {
                        ExitLatency::Measured(ms) => live::delay_span(Some(ms), Some(true)),
                        ExitLatency::Failed => live::delay_span(None, Some(false)),
                        ExitLatency::Pending => live::delay_span(None, None),
                    });
                }
                if !fields.is_empty() {
                    spans.push(Span::styled(" (", muted));
                    for (index, field) in fields.into_iter().enumerate() {
                        if index > 0 {
                            spans.push(Span::styled(" | ", muted));
                        }
                        spans.push(field);
                    }
                    spans.push(Span::styled(")", muted));
                }
            }
            spans.push(sep());
            spans
        };
        // Row two: listeners, every switch, then telemetry health; narrow terminals
        // drop the health, then the off switches, then the listeners.
        let listeners = self
            .details
            .get(&row.instance.name)
            .map(|details| details.listeners.join(" · "))
            .unwrap_or_default();
        let second = |level: usize| {
            let mut spans = vec![Span::raw(" ")];
            if level < 3 && !listeners.is_empty() {
                spans.push(Span::raw(listeners.clone()));
                spans.push(Span::raw("  "));
            }
            if let Some(snapshot) = live {
                let switches = [
                    (t("System proxy"), snapshot.system_proxy),
                    ("TUN", snapshot.tun),
                    ("LAN", snapshot.allow_lan),
                ];
                for (label, on) in switches.iter().filter(|(_, on)| level < 2 || *on) {
                    spans.push(Span::styled(
                        format!("{} {label}  ", if *on { "●" } else { "○" }),
                        if *on {
                            Style::default().fg(Color::Green)
                        } else {
                            muted
                        },
                    ));
                }
            }
            if level < 1 {
                spans.push(Span::styled(
                    format!("    {}", self.runtime_health()),
                    muted,
                ));
            }
            spans
        };
        const LEVELS: [usize; 2] = [6, 3];
        // Rates share one format on both rows so the columns line up. Out of room,
        // they switch to P / D labels, then short numbers (while the rows drop
        // detail), and are left out only when even that cannot fit.
        let rates = |row: usize, variant: usize| {
            // Shown as — before runtime data arrives so the columns never jump.
            if variant < 3 {
                self.rate_spans(row, variant >= 2, variant >= 1)
            } else {
                vec![]
            }
        };
        let fits =
            |left: &[Span], right: &[Span]| width(left) + width(right) <= area.width as usize;
        // Rates may push out the IP, uptime, middle hops and off switches, but
        // never the state, the exit node or the switches that are on.
        let deepest = |row: usize, variant: usize| match variant {
            0 | 1 => 0,
            2 => [3, 2][row],
            _ => LEVELS[row],
        };
        let fitting_level = |row: usize, variant: usize, connect: &mut (usize, usize)| {
            let right = rates(row, variant);
            (0..=deepest(row, variant)).find(|&level| {
                let left = if row == 0 {
                    first(level, connect)
                } else {
                    second(level)
                };
                fits(&left, &right)
            })
        };
        let variant = (0..3)
            .find(|&variant| {
                fitting_level(0, variant, &mut connect).is_some()
                    && (area.height < 2 || fitting_level(1, variant, &mut connect).is_some())
            })
            .unwrap_or(3);
        let pick = |row: usize, build: &mut dyn FnMut(usize) -> Vec<Span<'static>>| {
            let right = rates(row, variant);
            let mut left = (0..=LEVELS[row])
                .map(&mut *build)
                .find(|left| fits(left, &right))
                .unwrap_or_else(|| build(LEVELS[row]));
            // The trailing separator only leads into the rates.
            if right.is_empty() && left.last().is_some_and(|span| span.content == " │ ") {
                left.pop();
            }
            (left, right)
        };
        let (left, right) = pick(0, &mut |level| first(level, &mut connect));
        // `connect` now spans the profile segment of the row actually drawn.
        let (start, end) = connect;
        Self::render_right_aligned(f, Rect::new(area.x, area.y, area.width, 1), left, right);
        // Clicking the profile segment connects or disconnects, like the GUI switch.
        self.hit(
            Rect::new(area.x + start as u16, area.y, (end - start) as u16, 1).intersection(area),
            Target::Connect,
        );
        if area.height >= 2 {
            let (left, right) = pick(1, &mut |level| second(level));
            Self::render_right_aligned(
                f,
                Rect::new(area.x, area.y + 1, area.width, 1),
                left,
                right,
            );
        }
    }
    fn render_page_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let mut x = area.x + 1;
        let full_width: usize = PAGES
            .iter()
            .enumerate()
            .map(|(i, (_, title))| Line::from(format!(" {} {} ", i + 1, t(title))).width() + 1)
            .sum();
        let compact = full_width + 1 > area.width as usize;
        for (index, &(page, title)) in PAGES.iter().enumerate() {
            let label = if compact && page != self.page {
                format!(" {} ", index + 1)
            } else {
                format!(" {} {} ", index + 1, t(title))
            };
            let width = Line::from(label.as_str()).width() as u16;
            let rect = Rect::new(x, area.y, width, 1).intersection(area);
            f.render_widget(
                Paragraph::new(label).style(
                    if self.focus.region == focus::Region::Tabs && index == self.focus.cursor {
                        Style::default()
                            .bg(Color::Cyan)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else if page == self.page {
                        Style::default()
                            .bg(BLUE)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    },
                ),
                rect,
            );
            self.hit(rect, Target::Page(page));
            x = x.saturating_add(width + 1);
        }
    }
    fn hint(&self) -> String {
        let text = if self.palette.is_some() {
            "↑↓ select · Enter run · Esc close · type to filter"
        } else if self.recovery.is_some() {
            "c retry · b cancel · ↑↓ scroll"
        } else if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            "Tab / ←→ choose · Enter apply · Esc cancel"
        } else if self.prompt.is_some() {
            "Enter confirm · Esc cancel · Ctrl-U clear"
        } else if self.editor.is_some() {
            "Enter edit · Space toggle · Ctrl-S save · Ctrl-Z/Y undo/redo · Esc retain draft"
        } else if self.viewer.is_some() {
            "End bottom · [ ] source · / search · f filter · v level · Space pause · Esc close · ? all keys"
        } else if self.runtime_picker.is_some() {
            "↑↓ member · Enter select · / find · t test · Esc groups"
        } else {
            match self.page {
                OVERVIEW => "e edit · L logs · ? help",
                PROXIES => "↑↓ group · Enter members · / find · t test · D node tests · U update subscriptions",
                CONNECTIONS => "↑↓ select · / filter · x close · X close all · Esc clear filter",
                DNS => "↑↓ select · Enter details · / filter · Space pause · Esc back",
                RULES => "t test a route · / search rules · e edit rules",
                LOGS => "Enter open · [ proxy log · ] rule core log · F7 TUI log",
                SETTINGS => "↑↓ select · ←→ change · PgUp/PgDn scroll · Home top · r refresh",
                _ => "↑↓ browse · Ctrl-P activate profile · Enter actions · e edit · Insert new · Delete remove",
            }
        };
        format!(" {}", t(text))
    }
    pub(super) fn render_palette(&self, f: &mut Frame<'_>) {
        let Some(palette) = &self.palette else {
            return;
        };
        let before = f.buffer_mut().clone();
        let entries = self.palette_matches();
        let area = f.area();
        let width = area.width.saturating_sub(4).min(76);
        let height = (entries.len() as u16 + 4).clamp(6, 22).min(area.height);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + area.height.saturating_sub(height) / 3,
            width,
            height,
        );
        f.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(BLUE))
            .title(format!(
                " {} ",
                t(if palette.profiles {
                    "Switch profile"
                } else {
                    "Commands"
                })
            ))
            .title_bottom(format!(
                " {} ",
                t(if palette.profiles {
                    "Tab: search / type / list · arrows choose · Enter select"
                } else {
                    "Right column: the same key on this page"
                })
            ));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
        palette.viewport_height.set(rows[2].height);
        let cursor = palette.byte();
        f.render_widget(
            Paragraph::new(format!(
                "› {}{}{}",
                &palette.query[..cursor],
                if palette.focus == PaletteFocus::Search {
                    "▏"
                } else {
                    ""
                },
                &palette.query[cursor..]
            ))
            .style(if palette.focus == PaletteFocus::Search {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            }),
            rows[0],
        );
        let content_width = rows[2].width.saturating_sub(2) as usize;
        let scope_width = if content_width >= 40 { 9 } else { 0 };
        let key_width = if content_width >= 40 {
            entries
                .iter()
                .map(|e| Line::from(e.key).width())
                .max()
                .unwrap_or(0)
                .min(18)
        } else {
            0
        };
        let label_width =
            content_width.saturating_sub(scope_width + key_width + usize::from(key_width > 0));
        let items: Vec<_> = entries
            .iter()
            .map(|e| {
                let style = if e.unavailable.is_some() {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                let mut label = e.label.clone();
                if let Some(reason) = e.unavailable {
                    label = format!("{label} · {}", t(reason));
                }
                ListItem::new(Line::from(vec![
                    Span::styled(
                        palette_column(t(e.scope), scope_width),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(palette_column(&label, label_width), style),
                    Span::raw(if key_width > 0 { " " } else { "" }),
                    Span::styled(
                        palette_column(e.key, key_width),
                        Style::default().fg(Color::Cyan),
                    ),
                ]))
            })
            .collect();
        let empty = items.is_empty();
        if palette.profiles {
            let type_style = |kind| {
                if palette.profile_kind == kind {
                    if palette.focus == PaletteFocus::Type {
                        crate::theme::selected()
                    } else {
                        Style::default().fg(Color::Cyan)
                    }
                } else {
                    Style::default().fg(MUTED)
                }
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(format!("{}: ", t("Type"))),
                    Span::styled(
                        format!(
                            "{}{}{}",
                            if palette.profile_kind == Kind::Proxy {
                                "["
                            } else {
                                " "
                            },
                            t("Proxy"),
                            if palette.profile_kind == Kind::Proxy {
                                "]"
                            } else {
                                " "
                            }
                        ),
                        type_style(Kind::Proxy),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!(
                            "{}{}{}",
                            if palette.profile_kind == Kind::Rule {
                                "["
                            } else {
                                " "
                            },
                            t("Rules"),
                            if palette.profile_kind == Kind::Rule {
                                "]"
                            } else {
                                " "
                            }
                        ),
                        type_style(Kind::Rule),
                    ),
                ])),
                rows[1],
            );
        } else if let Some(reason) = entries
            .get(palette.selected)
            .and_then(|entry| entry.unavailable)
        {
            f.render_widget(
                Paragraph::new(palette_column(t(reason), rows[1].width as usize))
                    .style(Style::default().fg(Color::Yellow)),
                rows[1],
            );
        }
        let mut state = ListState::default().with_selected(
            (!empty).then_some(palette.selected.min(entries.len().saturating_sub(1))),
        );
        let geometry = astra_tui_kit::SelectableList::render(
            f,
            List::new(items)
                .highlight_style(crate::theme::selected())
                .highlight_symbol(if palette.focus == PaletteFocus::List {
                    "› "
                } else {
                    "  "
                }),
            rows[2],
            rows[2],
            &mut state,
            entries.len(),
            1,
        );
        if empty {
            f.render_widget(Paragraph::new(t("No matching command")), rows[2]);
        }
        self.hits.borrow_mut().clear();
        self.hits.borrow_mut().bind_rows(&geometry, Target::Palette);
        self.animations
            .surface(f, super::animation::Kind::Palette, popup, &before);
    }
    /// Tab-like title: the shown list is highlighted, the other one muted.
    pub(super) fn profiles_title(&self) -> Line<'static> {
        let proxies = self.proxies_count();
        let rules = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Rule)
            .count();
        let servers = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Server)
            .count();
        let tab = |label: String, on: bool| {
            Span::styled(
                format!(" {label} "),
                if on {
                    crate::theme::selected()
                } else {
                    Style::default().fg(MUTED)
                },
            )
        };
        Line::from(vec![
            Span::raw(" "),
            tab(tr!("Profiles {}", proxies), self.kind == Kind::Proxy),
            Span::raw(" "),
            tab(tr!("Rule cores {}", rules), self.kind == Kind::Rule),
            Span::raw(" "),
            tab(format!("Servers {servers}"), self.kind == Kind::Server),
            Span::styled(
                format!("  {} ", t("Tab controls · Enter switch")),
                Style::default().fg(MUTED),
            ),
        ])
    }
}

/// Pad terminal columns rather than Unicode scalar count (Chinese scopes are double width).
fn palette_column(text: &str, width: usize) -> String {
    astra_tui_kit::text::fit_column(text, width)
}

#[cfg(test)]
#[path = "../test/ui_shell_palette_layout_test.rs"]
mod palette_layout_tests;
