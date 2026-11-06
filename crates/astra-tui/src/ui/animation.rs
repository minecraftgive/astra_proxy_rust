//! Presentation-only motion. Sampling and operations never use this clock.
//!
//! Perceptual rules this module follows — breaking them reads as flicker:
//! - Content that stays on screen never changes brightness. Dimming something
//!   and bringing it back (focus pulses, page fades, emphasis blinks) is seen as
//!   the screen blinking, not as motion.
//! - Frequent keyboard actions (moving focus, switching pages, folding fields)
//!   are instant; the content change itself is the feedback.
//! - Only things that enter or leave space move: overlays reveal from their
//!   anchor edge and exit faster than they enter.
//! - Color changes are one-way and slow enough to read as settling (≥ 12
//!   frames), never a quick round trip.
//! - At ~30 fps a transition shorter than about 5 frames cannot be seen as
//!   motion; such transitions are not animated at all.
use astra_tui_kit::theme::BLUE;
use ratatui::{buffer::Buffer, layout::Rect, style::Color, Frame};
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

pub use astra_client::motion::{Marquee, MotionMode};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Palette,
    Drawer,
    Detail,
    /// A settled notice fading to the muted tone instead of switching at once.
    Recede,
}
impl Kind {
    fn duration(self) -> Duration {
        Duration::from_millis(match self {
            Self::Palette => 180,
            Self::Drawer | Self::Detail => 220,
            Self::Recede => 600,
        })
    }
}
/// Exits are quicker than entrances: the user has already decided.
const EXIT: Duration = Duration::from_millis(140);

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
/// How finished frames are colored: the accent replacing the default blue and
/// the color depth the terminal is limited to. Changes apply on the next frame,
/// all at once — restyling the whole screen is never animated.
#[derive(Clone, Copy)]
struct Palette {
    accent: Option<Color>,
    no_color: bool,
    color_count: u16,
}
pub(super) struct Motion {
    state: RefCell<State>,
    epoch: Instant,
    frame: Duration,
    detected_no_color: bool,
    detected_colors: u16,
    palette: std::cell::Cell<Palette>,
    marquee: std::cell::Cell<Marquee>,
    /// Whether the last frame had anything for the marquee to paint.
    marquee_seen: std::cell::Cell<bool>,
}

/// Hidden running-light effect, unlocked from Settings. Each part is optional.
/// It deliberately breaks the "never restyle persistent content" rule above, so
/// it is off unless the user turns it on, and it stands still without Normal motion.
/// Focused border color while the border marquee is on; one step off Cyan so an
/// unprocessed frame looks the same.
pub(super) const FOCUS_MARK: Color = Color::Rgb(0, 215, 214);
/// Status line text, and the connected dot and word, while their marquee is on.
pub(super) const STATUS_MARK: Color = Color::Rgb(140, 147, 160);
use astra_tui_kit::theme::{SELECTED_KEEP_MARK, SELECTED_MARK};
impl Motion {
    pub(super) fn new(mode: MotionMode) -> Self {
        let remote =
            std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
        // Tests must not depend on the runner's TERM/COLORTERM/NO_COLOR: CI terminals
        // report fewer colors, which would remap every expected RGB value.
        let (no_color, color_count) = if cfg!(test) {
            (false, u16::MAX)
        } else {
            (
                std::env::var_os("NO_COLOR").is_some(),
                crossterm::style::available_color_count(),
            )
        };
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
            detected_no_color: no_color,
            detected_colors: color_count,
            palette: std::cell::Cell::new(Palette {
                accent: None,
                no_color,
                color_count,
            }),
            marquee: Default::default(),
            marquee_seen: Default::default(),
        }
    }
    pub(super) fn set_marquee(&self, marquee: Marquee) {
        self.marquee.set(marquee);
        astra_tui_kit::theme::set_selection_mark(marquee.text || marquee.background);
    }
    pub(super) fn marquee(&self) -> Marquee {
        self.marquee.get()
    }
    /// Time between marquee frames.
    pub(super) fn marquee_interval(&self) -> Duration {
        Duration::from_secs_f32(1.0 / f32::from(self.marquee.get().fps.max(1)))
    }
    /// Shortest gap between two draws: the marquee may ask for more frames
    /// than overlay motion uses.
    pub(super) fn draw_interval(&self) -> Duration {
        if self.marquee_running() {
            self.frame.min(self.marquee_interval())
        } else {
            self.frame
        }
    }
    /// The marquee needs frames of its own while it has something to move.
    pub(super) fn marquee_running(&self) -> bool {
        self.mode() == MotionMode::Normal && self.marquee.get().any() && self.marquee_seen.get()
    }
    /// Paint marked cells; returns which cells now carry marquee colors.
    fn paint_marquee(&self, buffer: &mut Buffer, mode: MotionMode) -> Vec<bool> {
        let marquee = self.marquee.get();
        let area = buffer.area;
        let width = area.width as usize;
        let mut painted = vec![false; buffer.content.len()];
        if width == 0 {
            return painted;
        }
        // Reduced and Off keep the colors but stand still.
        let phase = if mode == MotionMode::Normal {
            self.epoch.elapsed().as_secs_f32() * marquee.speed
        } else {
            0.0
        };
        let rainbow = marquee.width.max(2.0);
        let step = 360.0 / rainbow;
        let at = |index: usize| ((index % width) as u16, (index / width) as u16);
        let mut seen = false;
        let cells = &mut buffer.content;
        let mut visited = vec![false; cells.len()];
        for start in 0..cells.len() {
            if visited[start] || cells[start].fg != FOCUS_MARK {
                continue;
            }
            // One connected ring of border cells per focused card.
            let mut ring = vec![];
            let mut stack = vec![start];
            visited[start] = true;
            while let Some(index) = stack.pop() {
                ring.push(index);
                let (x, y) = at(index);
                let mut push = |next: usize| {
                    if !visited[next] && cells[next].fg == FOCUS_MARK {
                        visited[next] = true;
                        stack.push(next);
                    }
                };
                if x > 0 {
                    push(index - 1);
                }
                if (x as usize) + 1 < width {
                    push(index + 1);
                }
                if y > 0 {
                    push(index - width);
                }
                if index + width < cells.len() {
                    push(index + width);
                }
            }
            if !marquee.border {
                for index in ring {
                    cells[index].set_fg(Color::Cyan);
                }
                continue;
            }
            seen = true;
            let (mut x0, mut y0, mut x1, mut y1) = (u16::MAX, u16::MAX, 0, 0);
            for &index in &ring {
                let (x, y) = at(index);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
            let (w, h) = ((x1 - x0) as f32, (y1 - y0) as f32);
            let length = (2.0 * (w + h)).max(1.0);
            // A whole number of rainbows, so the loop has no seam.
            let cycles = (length / rainbow).round().max(1.0);
            for index in ring {
                let (x, y) = at(index);
                let (dx, dy) = ((x - x0) as f32, (y - y0) as f32);
                let position = if y == y0 {
                    dx
                } else if x == x1 {
                    w + dy
                } else if y == y1 {
                    w + h + (w - dx)
                } else if x == x0 {
                    2.0 * w + h + (h - dy)
                } else {
                    dx + dy
                };
                let hue = 360.0 * cycles * position / length - phase;
                cells[index].set_fg(hsl(hue, 0.85, 0.6));
                painted[index] = true;
            }
        }
        let items = marquee.text || marquee.background;
        // Text colors step once per visible character of a selected run, so a
        // wide (CJK) character and the spaces between words do not skip hues.
        let mut glyph = 0usize;
        let mut in_run = false;
        for (index, cell) in cells.iter_mut().enumerate() {
            let marked = matches!(cell.bg, SELECTED_MARK | SELECTED_KEEP_MARK);
            if !marked || index % width == 0 {
                in_run = false;
            }
            if marked && !in_run {
                in_run = true;
                glyph = 0;
            }
            let visible = !cell.symbol().trim().is_empty();
            if cell.fg == STATUS_MARK {
                if marquee.status || marquee.indicator {
                    seen = true;
                    painted[index] = true;
                    let hue = (index % width) as f32 * step - phase;
                    cell.set_fg(hsl(hue, 0.85, 0.68));
                } else {
                    cell.set_fg(Color::Gray);
                }
            }
            let original = match cell.bg {
                SELECTED_MARK => BLUE,
                SELECTED_KEEP_MARK => Color::Rgb(24, 28, 34),
                _ => continue,
            };
            if !items {
                cell.set_bg(original);
                continue;
            }
            seen = true;
            painted[index] = true;
            let (x, y) = at(index);
            let hue = (x as f32 + y as f32 * 2.0) * step - phase;
            let text_hue = (glyph as f32 + y as f32 * 2.0) * step - phase;
            if visible {
                glyph += 1;
            }
            match (marquee.text, marquee.background) {
                (true, true) => {
                    cell.set_bg(hsl(hue, 0.7, 0.36));
                    cell.set_fg(hsl(text_hue + 180.0, 0.9, 0.8));
                }
                (false, true) => {
                    cell.set_bg(hsl(hue, 0.7, 0.4));
                    cell.set_fg(Color::White);
                }
                _ => {
                    cell.set_bg(original);
                    cell.set_fg(hsl(text_hue, 0.9, 0.74));
                }
            }
        }
        self.marquee_seen.set(seen);
        painted
    }
    pub(super) fn set_palette(&self, accent: Color, depth: super::settings::ColorDepth) {
        use super::settings::ColorDepth;
        let (no_color, color_count) = match depth {
            ColorDepth::Auto => (self.detected_no_color, self.detected_colors),
            ColorDepth::TrueColor => (false, u16::MAX),
            ColorDepth::Ansi256 => (false, 256),
            ColorDepth::Ansi16 => (false, 16),
            ColorDepth::Monochrome => (true, self.detected_colors),
        };
        self.palette.set(Palette {
            accent: (accent != BLUE).then_some(accent),
            no_color,
            color_count,
        });
    }
    /// What the terminal reported, shown next to the Auto color choice.
    pub(super) fn detected_colors(&self) -> Option<u16> {
        (!self.detected_no_color).then_some(self.detected_colors)
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
        self.transition(kind, 1.0, 0.0, EXIT);
    }
    fn transition(&self, kind: Kind, initial: f32, to: f32, duration: Duration) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        if state.mode != MotionMode::Normal {
            state.surfaces.retain(|s| !s.closing);
            return;
        }
        // Retarget from where the motion is now; never restart from the edge.
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
    /// Eased progress of a running track, `None` once settled (or with motion off).
    pub(super) fn progress(&self, kind: Kind) -> Option<f32> {
        let now = Instant::now();
        self.state
            .borrow()
            .tracks
            .iter()
            .find(|t| t.kind == kind && !t.done(now))
            .map(|t| t.value(now))
    }
    pub(super) fn spinner(&self) -> &'static str {
        if self.mode() != MotionMode::Normal {
            return "⟳";
        }
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[(self.epoch.elapsed().as_millis() / 80 % 8) as usize]
    }
    /// Render at the final size, then reveal rows from the top edge. The first
    /// three rows (including the command input) are always visible; hit targets
    /// keep their final geometry.
    pub(super) fn surface(&self, f: &mut Frame<'_>, kind: Kind, rect: Rect, before: &Buffer) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        if state.mode != MotionMode::Normal {
            return;
        }
        let rect = rect.intersection(f.area());
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
        let visible = (rect.height as f32 * value).ceil() as u16;
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
        let area = f.area();
        // A closing overlay retracts toward its top edge over what is now below it.
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
        let mode = state.mode;
        drop(state);
        let painted = self.paint_marquee(f.buffer_mut(), mode);
        // With "always true color", marquee cells skip the depth reduction below.
        let keep = |index: usize| self.marquee.get().true_color && painted[index];
        let palette = self.palette.get();
        if let Some(accent) = palette.accent {
            for cell in &mut f.buffer_mut().content {
                if cell.fg == BLUE {
                    cell.set_fg(accent);
                }
                if cell.bg == BLUE {
                    cell.set_bg(accent);
                }
            }
        }
        if palette.no_color {
            for (index, cell) in f.buffer_mut().content.iter_mut().enumerate() {
                if keep(index) {
                    continue;
                }
                cell.set_fg(Color::Reset);
                cell.set_bg(Color::Reset);
            }
        } else if palette.color_count < u16::MAX {
            for (index, cell) in f.buffer_mut().content.iter_mut().enumerate() {
                if keep(index) {
                    continue;
                }
                cell.set_fg(terminal_color(cell.fg, palette.color_count));
                cell.set_bg(terminal_color(cell.bg, palette.color_count));
            }
        }
    }
}

fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    Some(match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Red => (205, 49, 49),
        Color::LightRed => (241, 76, 76),
        Color::Green => (13, 188, 121),
        Color::LightGreen => (35, 209, 139),
        Color::Yellow => (229, 229, 16),
        Color::LightYellow => (245, 245, 67),
        Color::Blue => (36, 114, 200),
        Color::LightBlue => (59, 142, 234),
        Color::Magenta => (188, 63, 188),
        Color::LightMagenta => (214, 112, 214),
        Color::Cyan => (0, 215, 215),
        Color::LightCyan => (41, 184, 219),
        Color::Gray => (204, 204, 204),
        Color::White => (230, 230, 230),
        _ => return None,
    })
}
/// Hue in degrees (any value), saturation and lightness in 0..=1.
fn hsl(hue: f32, saturation: f32, lightness: f32) -> Color {
    let hue = hue.rem_euclid(360.0) / 60.0;
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let second = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u8 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let base = lightness - chroma / 2.0;
    let channel = |v: f32| ((v + base) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(channel(r), channel(g), channel(b))
}
fn mix(from: (u8, u8, u8), to: (u8, u8, u8), p: f32) -> Color {
    let p = p.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * p).round() as u8;
    Color::Rgb(lerp(from.0, to.0), lerp(from.1, to.1), lerp(from.2, to.2))
}
/// Interpolate between two foreground colors; colors without a known RGB value
/// (Reset, indexed) switch at the end rather than guessing.
pub(super) fn blend(from: Color, to: Color, p: f32) -> Color {
    match (rgb(from), rgb(to)) {
        (Some(a), Some(b)) if p < 1.0 => mix(a, b, p),
        _ => to,
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
#[path = "../test/ui_animation_test.rs"]
mod tests;
