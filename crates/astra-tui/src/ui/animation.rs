//! Presentation-only motion. Sampling and operations never use this clock.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
    Frame,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    sync::OnceLock,
    time::{Duration, Instant},
};

pub(crate) fn notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}
pub(crate) fn wake() {
    notify().notify_one();
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MotionMode {
    #[default]
    Normal,
    Reduced,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Focus,
    Page,
    Palette,
    Drawer,
    Detail,
    Accordion,
    Sample,
    Feedback,
}
impl Kind {
    fn duration(self) -> Duration {
        Duration::from_millis(match self {
            Self::Focus => 100,
            Self::Page => 140,
            Self::Palette => 160,
            Self::Drawer | Self::Detail => 200,
            Self::Accordion => 180,
            Self::Sample | Self::Feedback => 160,
        })
    }
}
struct Track {
    kind: Kind,
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}
impl Track {
    fn value(&self, now: Instant) -> f32 {
        let t =
            now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32();
        self.from + (self.to - self.from) * ease_out(t.clamp(0.0, 1.0))
    }
    fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.duration
    }
}
// Solve x for cubic-bezier(.23, 1, .32, 1), rather than treating x as the parameter.
fn ease_out(x: f32) -> f32 {
    if x <= 0.0 || x >= 1.0 {
        return x;
    }
    let (mut low, mut high) = (0.0_f32, 1.0_f32);
    for _ in 0..16 {
        let t = (low + high) * 0.5;
        let at = 3.0 * (1.0 - t).powi(2) * t * 0.23 + 3.0 * (1.0 - t) * t * t * 0.32 + t * t * t;
        if at < x {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) * 0.5;
    1.0 - (1.0 - t).powi(3)
}
struct Surface {
    kind: Kind,
    buffer: Buffer,
    closing: bool,
}
struct State {
    mode: MotionMode,
    tracks: Vec<Track>,
    surfaces: Vec<Surface>,
    area: Rect,
    final_frame: bool,
}
pub(super) struct Motion {
    state: RefCell<State>,
    epoch: Instant,
    frame: Duration,
    no_color: bool,
    color_count: u16,
}
impl Motion {
    pub(super) fn new(mode: MotionMode) -> Self {
        let remote =
            std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
        Self {
            state: RefCell::new(State {
                mode,
                tracks: vec![],
                surfaces: vec![],
                area: Rect::default(),
                final_frame: false,
            }),
            epoch: Instant::now(),
            frame: Duration::from_millis(if remote { 67 } else { 34 }),
            no_color: std::env::var_os("NO_COLOR").is_some(),
            color_count: crossterm::style::available_color_count(),
        }
    }
    pub(super) fn set_mode(&self, mode: MotionMode) {
        let mut state = self.state.borrow_mut();
        state.mode = mode;
        state.tracks.clear();
        state.surfaces.clear();
        state.final_frame = false;
    }
    pub(super) fn mode(&self) -> MotionMode {
        self.state.borrow().mode
    }
    pub(super) fn frame_interval(&self) -> Duration {
        self.frame
    }
    pub(super) fn active(&self) -> bool {
        let state = self.state.borrow();
        !state.tracks.is_empty() || state.final_frame
    }
    pub(super) fn finish(&self, kind: Kind) {
        let mut state = self.state.borrow_mut();
        state.tracks.retain(|t| t.kind != kind);
        state.surfaces.retain(|s| s.kind != kind || !s.closing);
    }
    pub(super) fn resize(&self) {
        let mut state = self.state.borrow_mut();
        state.tracks.clear();
        state.surfaces.clear();
        state.final_frame = false;
    }
    pub(super) fn trigger(&self, kind: Kind) {
        self.transition(kind, 0.0, 1.0, kind.duration());
    }
    pub(super) fn open(&self, kind: Kind) {
        self.state
            .borrow_mut()
            .surfaces
            .retain(|s| s.kind != kind || !s.closing);
        self.transition(kind, 0.25, 1.0, kind.duration());
    }
    pub(super) fn close(&self, kind: Kind) {
        {
            let mut state = self.state.borrow_mut();
            if let Some(surface) = state.surfaces.iter_mut().find(|s| s.kind == kind) {
                surface.closing = true;
            } else {
                return;
            }
        }
        self.transition(kind, 1.0, 0.0, Duration::from_millis(120));
    }
    fn transition(&self, kind: Kind, initial: f32, to: f32, duration: Duration) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        if state.mode != MotionMode::Normal {
            state.surfaces.retain(|s| !s.closing);
            return;
        }
        let from = state
            .tracks
            .iter()
            .find(|t| t.kind == kind)
            .map_or(initial, |t| t.value(now));
        state.tracks.retain(|t| t.kind != kind && !t.done(now));
        if state.tracks.len() == 8 {
            state.tracks.remove(0);
        }
        state.tracks.push(Track {
            kind,
            from,
            to,
            start: now,
            duration,
        });
    }
    pub(super) fn spinner(&self) -> &'static str {
        if self.mode() != MotionMode::Normal {
            return "⟳";
        }
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[(self.epoch.elapsed().as_millis() / 80 % 8) as usize]
    }
    pub(super) fn emphasize_sample(&self, f: &mut Frame<'_>, rect: Rect) {
        if !self
            .state
            .borrow()
            .tracks
            .iter()
            .any(|t| t.kind == Kind::Sample && !t.done(Instant::now()))
        {
            return;
        }
        let rect = rect.intersection(f.area());
        if rect.width == 0 {
            return;
        }
        for y in rect.y..rect.bottom() {
            let cell = &mut f.buffer_mut()[(rect.right() - 1, y)];
            if cell.symbol() != " " {
                cell.modifier.insert(Modifier::BOLD);
            }
        }
    }
    /// Render at the final size, then reveal rows. The first three rows (including
    /// the command input) are always visible; hit targets keep their final geometry.
    pub(super) fn surface(&self, f: &mut Frame<'_>, kind: Kind, rect: Rect, before: &Buffer) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        if state.mode != MotionMode::Normal {
            return;
        }
        let rect = rect.intersection(f.area());
        if kind == Kind::Accordion && !state.tracks.iter().any(|t| t.kind == kind && !t.done(now)) {
            return;
        }
        let mut full = Buffer::empty(rect);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                full[(x, y)] = f.buffer_mut()[(x, y)].clone();
            }
        }
        let value = state
            .tracks
            .iter()
            .find(|t| t.kind == kind)
            .map_or(1.0, |t| t.value(now));
        let mut visible = (rect.height as f32 * value).ceil() as u16;
        if kind == Kind::Accordion {
            // The selected field must be usable even on the first reveal frame.
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    let cell = &full[(x, y)];
                    if matches!(cell.symbol(), "›" | "▶" | "▸" | "▏") || cell.bg != Color::Reset
                    {
                        visible = visible.max(y - rect.y + 1);
                    }
                }
            }
        }
        for y in rect.y + visible.max(3).min(rect.height)..rect.bottom() {
            for x in rect.x..rect.right() {
                if before.area.contains((x, y).into()) {
                    f.buffer_mut()[(x, y)] = before[(x, y)].clone();
                }
            }
        }
        state.surfaces.retain(|s| s.kind != kind);
        state.surfaces.push(Surface {
            kind,
            buffer: full,
            closing: false,
        });
    }
    pub(super) fn finish_frame(&self, f: &mut Frame<'_>) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        if state.area != f.area() {
            state.area = f.area();
            state.tracks.clear();
            state.surfaces.clear();
        }
        let focus = state
            .tracks
            .iter()
            .find(|t| t.kind == Kind::Focus && !t.done(now))
            .map(|t| t.value(now));
        let page = state
            .tracks
            .iter()
            .find(|t| t.kind == Kind::Page && !t.done(now))
            .map(|t| t.value(now));
        let feedback = state
            .tracks
            .iter()
            .find(|t| t.kind == Kind::Feedback)
            .is_some_and(|t| !t.done(now));
        let area = f.area();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let cell = &mut f.buffer_mut()[(x, y)];
                if (focus.is_some()
                    && (matches!(cell.symbol(), "›" | "▶" | "▸")
                        || cell.fg == Color::Cyan
                            && matches!(
                                cell.symbol(),
                                "─" | "│" | "┌" | "┐" | "└" | "┘" | "╭" | "╮" | "╰" | "╯"
                            )))
                    || (page.is_some() && y <= area.y + 1)
                {
                    let rgb = match cell.fg {
                        Color::Rgb(r, g, b) => Some((r, g, b)),
                        Color::Cyan => Some((0, 215, 215)),
                        Color::White => Some((230, 230, 230)),
                        _ => None,
                    };
                    if let Some((r, g, b)) = rgb {
                        let p = if y <= area.y + 1 {
                            page.unwrap_or(1.0)
                        } else {
                            focus.unwrap_or(1.0)
                        };
                        let mix = |c: u8| (110.0 + (c as f32 - 110.0) * p) as u8;
                        cell.set_fg(Color::Rgb(mix(r), mix(g), mix(b)));
                    }
                }
                if feedback && y == area.bottom().saturating_sub(1) {
                    cell.modifier.insert(Modifier::BOLD);
                }
            }
        }
        for surface in state.surfaces.iter().filter(|s| s.closing) {
            let value = state
                .tracks
                .iter()
                .find(|t| t.kind == surface.kind)
                .map_or(0.0, |t| t.value(now));
            let rect = surface.buffer.area.intersection(area);
            let visible = (rect.height as f32 * value).ceil() as u16;
            for y in rect.y..rect.y + visible.min(rect.height) {
                for x in rect.x..rect.right() {
                    f.buffer_mut()[(x, y)] = surface.buffer[(x, y)].clone();
                }
            }
        }
        state.final_frame = state.tracks.iter().any(|t| t.done(now));
        state.tracks.retain(|t| !t.done(now));
        let active: Vec<_> = state.tracks.iter().map(|t| t.kind).collect();
        state
            .surfaces
            .retain(|s| !s.closing || active.contains(&s.kind));
        if self.no_color {
            for cell in &mut f.buffer_mut().content {
                cell.set_fg(Color::Reset);
                cell.set_bg(Color::Reset);
            }
        } else if self.color_count < u16::MAX {
            for cell in &mut f.buffer_mut().content {
                cell.set_fg(terminal_color(cell.fg, self.color_count));
                cell.set_bg(terminal_color(cell.bg, self.color_count));
            }
        }
    }
}

fn terminal_color(color: Color, count: u16) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let distance = |rgb: (u8, u8, u8)| -> i32 {
        (r as i32 - rgb.0 as i32).pow(2)
            + (g as i32 - rgb.1 as i32).pow(2)
            + (b as i32 - rgb.2 as i32).pow(2)
    };
    if count < 256 {
        const COLORS: [(Color, (u8, u8, u8)); 8] = [
            (Color::Black, (0, 0, 0)),
            (Color::Red, (205, 0, 0)),
            (Color::Green, (0, 205, 0)),
            (Color::Yellow, (205, 205, 0)),
            (Color::Blue, (0, 0, 238)),
            (Color::Magenta, (205, 0, 205)),
            (Color::Cyan, (0, 205, 205)),
            (Color::Gray, (229, 229, 229)),
        ];
        return COLORS
            .iter()
            .min_by_key(|(_, rgb)| distance(*rgb))
            .unwrap()
            .0;
    }
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        (0..6)
            .min_by_key(|&i| (v as i32 - LEVELS[i] as i32).abs())
            .unwrap()
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let gray = (((r as i32 + g as i32 + b as i32) / 3 - 8 + 5) / 10).clamp(0, 23) as u8;
    let level = 8 + gray * 10;
    if distance((level, level, level)) < distance((LEVELS[ri], LEVELS[gi], LEVELS[bi])) {
        Color::Indexed(232 + gray)
    } else {
        Color::Indexed((16 + 36 * ri + 6 * gi + bi) as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn low_color_terminals_receive_supported_styles() {
        assert!(matches!(
            terminal_color(Color::Rgb(64, 128, 230), 256),
            Color::Indexed(_)
        ));
        assert!(!matches!(
            terminal_color(Color::Rgb(64, 128, 230), 8),
            Color::Indexed(_) | Color::Rgb(..)
        ));
        assert_eq!(terminal_color(Color::Reset, 8), Color::Reset);
    }
    #[test]
    fn easing_is_monotonic_and_finishes_exactly() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        let values: Vec<_> = (0..=100).map(|i| ease_out(i as f32 / 100.0)).collect();
        assert!(values.windows(2).all(|v| v[0] <= v[1]));
        assert!(ease_out(0.5) > 0.9);
    }
    #[test]
    fn interruptions_retarget_without_queueing() {
        let motion = Motion::new(MotionMode::Normal);
        for _ in 0..100 {
            motion.open(Kind::Palette);
        }
        assert_eq!(motion.state.borrow().tracks.len(), 1);
        motion.finish(Kind::Palette);
        assert!(!motion.active());
    }
    #[test]
    fn reduced_and_off_have_no_timed_geometry_or_spinner() {
        for mode in [MotionMode::Reduced, MotionMode::Off] {
            let motion = Motion::new(mode);
            motion.open(Kind::Drawer);
            motion.trigger(Kind::Focus);
            assert!(!motion.active());
            assert_eq!(motion.spinner(), "⟳");
        }
    }
    #[test]
    fn resize_discards_old_geometry() {
        let motion = Motion::new(MotionMode::Normal);
        motion.open(Kind::Palette);
        motion.resize();
        assert!(!motion.active());
        assert!(motion.state.borrow().surfaces.is_empty());
    }
    #[test]
    fn reveal_keeps_input_visible_and_exits_from_final_geometry() {
        use ratatui::{backend::TestBackend, widgets::Paragraph, Terminal};
        for (width, height) in [(24, 8), (100, 30)] {
            let motion = Motion::new(MotionMode::Normal);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| motion.finish_frame(f)).unwrap();
            motion.open(Kind::Palette);
            terminal
                .draw(|f| {
                    let area = f.area();
                    let before = f.buffer_mut().clone();
                    f.render_widget(
                        Paragraph::new("Commands\nQuery: 中文\nselection\nitem 2\nitem 3\nitem 4"),
                        area,
                    );
                    motion.surface(f, Kind::Palette, area, &before);
                    motion.finish_frame(f);
                })
                .unwrap();
            assert_eq!(terminal.backend().buffer()[(0, 1)].symbol(), "Q");
            motion.close(Kind::Palette);
            {
                let mut state = motion.state.borrow_mut();
                for t in &mut state.tracks {
                    t.start = Instant::now() - Duration::from_secs(1);
                }
            }
            terminal.draw(|f| motion.finish_frame(f)).unwrap();
            assert!(motion.active(), "expiry schedules a final unmodified frame");
            terminal.draw(|f| motion.finish_frame(f)).unwrap();
            assert!(!motion.active());
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|c| c.symbol() == " "));
        }
    }
}
