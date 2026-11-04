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
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Marquee {
    pub(super) border: bool,
    pub(super) text: bool,
    pub(super) background: bool,
    /// The bottom status line text.
    pub(super) status: bool,
    /// The connection dot and state word, only while connected.
    pub(super) indicator: bool,
    /// Paint marquee cells in 24-bit color whatever the Colors setting says.
    pub(super) true_color: bool,
    /// Hue degrees per second.
    pub(super) speed: f32,
    /// Cells covered by one whole rainbow.
    pub(super) width: f32,
    /// Frames per second while only the marquee moves.
    pub(super) fps: u16,
}
impl Default for Marquee {
    fn default() -> Self {
        Self {
            border: false,
            text: false,
            background: false,
            status: false,
            indicator: false,
            true_color: false,
            speed: 160.0,
            width: 30.0,
            fps: 30,
        }
    }
}
impl Marquee {
    fn any(self) -> bool {
        self.border || self.text || self.background || self.status || self.indicator
    }
}
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
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let color_count = crossterm::style::available_color_count();
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
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, style::Style, widgets::Paragraph, Terminal};

    fn expire(motion: &Motion) {
        for t in &mut motion.state.borrow_mut().tracks {
            t.start = Instant::now() - Duration::from_secs(1);
        }
    }
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
    fn every_animation_is_long_enough_to_read_as_motion() {
        let frame = Motion::new(MotionMode::Normal).frame_interval();
        for kind in [Kind::Palette, Kind::Drawer, Kind::Detail, Kind::Recede] {
            assert!(kind.duration() >= frame * 5, "{kind:?} would only blink");
        }
        assert!(EXIT >= frame * 4);
        assert!(
            Kind::Recede.duration() >= frame * 12,
            "color settles slowly"
        );
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
            motion.trigger(Kind::Recede);
            assert!(!motion.active());
            assert!(motion.progress(Kind::Recede).is_none());
            assert_eq!(motion.spinner(), "⟳");
        }
    }
    #[test]
    fn blend_reaches_both_ends_and_skips_unknown_colors() {
        assert_eq!(
            blend(Color::Rgb(0, 0, 0), Color::Rgb(200, 100, 50), 0.0),
            Color::Rgb(0, 0, 0)
        );
        assert_eq!(
            blend(Color::Green, Color::Rgb(1, 2, 3), 1.0),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(blend(Color::Reset, Color::Red, 0.5), Color::Red);
    }
    #[test]
    fn persistent_content_is_never_restyled() {
        let motion = Motion::new(MotionMode::Normal);
        let mut terminal = Terminal::new(TestBackend::new(12, 5)).unwrap();
        let paint = |f: &mut Frame<'_>| {
            let area = f.area();
            f.render_widget(Paragraph::new("› item\n│ border"), area);
            f.buffer_mut()
                .set_style(area, Style::default().fg(Color::Cyan));
        };
        terminal
            .draw(|f| {
                paint(f);
                motion.finish_frame(f);
            })
            .unwrap();
        let settled = terminal.backend().buffer().clone();
        // Overlay motion elsewhere must not touch cells it does not own.
        motion.trigger(Kind::Recede);
        terminal
            .draw(|f| {
                paint(f);
                motion.finish_frame(f);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer(), &settled);
    }
    #[test]
    fn accent_and_depth_restyle_finished_frames() {
        let motion = Motion::new(MotionMode::Normal);
        let mut terminal = Terminal::new(TestBackend::new(4, 1)).unwrap();
        let paint = |f: &mut Frame<'_>| {
            let area = f.area();
            f.buffer_mut()
                .set_style(area, Style::default().bg(BLUE).fg(Color::Rgb(1, 2, 3)));
        };
        motion.set_palette(
            Color::Rgb(21, 128, 61),
            super::super::settings::ColorDepth::TrueColor,
        );
        terminal
            .draw(|f| {
                paint(f);
                motion.finish_frame(f);
            })
            .unwrap();
        let cell = &terminal.backend().buffer()[(0, 0)];
        assert_eq!(cell.bg, Color::Rgb(21, 128, 61));
        assert_eq!(cell.fg, Color::Rgb(1, 2, 3), "other colors untouched");
        motion.set_palette(BLUE, super::super::settings::ColorDepth::Monochrome);
        terminal
            .draw(|f| {
                paint(f);
                motion.finish_frame(f);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Color::Reset);
        motion.set_palette(BLUE, super::super::settings::ColorDepth::Ansi256);
        terminal
            .draw(|f| {
                paint(f);
                motion.finish_frame(f);
            })
            .unwrap();
        assert!(matches!(
            terminal.backend().buffer()[(0, 0)].bg,
            Color::Indexed(_)
        ));
    }
    #[test]
    fn marquee_runs_around_focused_borders_and_across_selections() {
        use ratatui::widgets::{Block, Borders};
        let paint = |f: &mut Frame<'_>| {
            f.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(FOCUS_MARK)),
                Rect::new(0, 0, 20, 6),
            );
            f.buffer_mut().set_style(
                Rect::new(1, 2, 18, 1),
                Style::default().bg(SELECTED_MARK).fg(Color::White),
            );
        };
        let draw = |motion: &Motion| {
            let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
            terminal
                .draw(|f| {
                    paint(f);
                    motion.finish_frame(f);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        // Off: marks map back to the normal colors.
        let motion = Motion::new(MotionMode::Off);
        let plain = draw(&motion);
        assert_eq!(plain[(0, 0)].fg, Color::Cyan);
        assert_eq!(plain[(5, 2)].bg, BLUE);
        assert!(!motion.marquee_running());
        // Set the field directly: the selection mark flag is process-wide.
        motion.marquee.set(Marquee {
            border: true,
            text: true,
            background: true,
            ..Default::default()
        });
        let lit = draw(&motion);
        let border: Vec<_> = [(0, 0), (10, 0), (19, 3), (10, 5), (0, 3)]
            .map(|p| lit[p].fg)
            .into();
        assert!(border
            .iter()
            .all(|c| matches!(c, Color::Rgb(..)) && *c != FOCUS_MARK));
        assert!(
            border.windows(2).any(|w| w[0] != w[1]),
            "colors vary along the ring"
        );
        assert_ne!(lit[(5, 2)].bg, lit[(12, 2)].bg);
        assert_ne!(lit[(5, 2)].bg, SELECTED_MARK);
        assert_eq!(lit[(5, 3)], plain[(5, 3)], "other content is untouched");
        // Without Normal motion the colors stand still and need no frames.
        assert_eq!(draw(&motion), lit);
        assert!(!motion.marquee_running());
        let moving = Motion::new(MotionMode::Normal);
        moving.marquee.set(motion.marquee.get());
        draw(&moving);
        assert!(moving.marquee_running());
    }
    #[test]
    fn status_marquee_width_and_true_color() {
        let paint = |f: &mut Frame<'_>| {
            f.buffer_mut()
                .set_style(Rect::new(0, 0, 24, 1), Style::default().fg(STATUS_MARK));
            f.buffer_mut().set_style(
                Rect::new(0, 1, 24, 1),
                Style::default().fg(Color::Rgb(64, 128, 230)),
            );
        };
        let draw = |motion: &Motion| {
            let mut terminal = Terminal::new(TestBackend::new(24, 2)).unwrap();
            terminal
                .draw(|f| {
                    paint(f);
                    motion.finish_frame(f);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let motion = Motion::new(MotionMode::Off);
        motion.set_palette(BLUE, super::super::settings::ColorDepth::Ansi16);
        assert!(
            !matches!(draw(&motion)[(0, 0)].fg, Color::Rgb(..)),
            "off: a plain color"
        );
        let status = Marquee {
            status: true,
            width: 12.0,
            ..Default::default()
        };
        motion.marquee.set(status);
        let reduced = draw(&motion);
        assert!(
            !matches!(reduced[(3, 0)].fg, Color::Rgb(..)),
            "16 colors without true color"
        );
        motion.marquee.set(Marquee {
            true_color: true,
            ..status
        });
        let lit = draw(&motion);
        assert!(
            matches!(lit[(3, 0)].fg, Color::Rgb(..)),
            "marquee cells stay 24-bit"
        );
        assert!(
            !matches!(lit[(3, 1)].fg, Color::Rgb(..)),
            "other cells are still reduced"
        );
        // One rainbow per 12 cells: cell 0 and cell 12 share a color, cell 6 does not.
        assert_eq!(lit[(0, 0)].fg, lit[(12, 0)].fg);
        assert_ne!(lit[(0, 0)].fg, lit[(6, 0)].fg);
    }
    #[test]
    fn text_marquee_steps_once_per_character() {
        let motion = Motion::new(MotionMode::Off);
        motion.marquee.set(Marquee {
            text: true,
            width: 12.0,
            ..Default::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(Paragraph::new(" 中 a b"), f.area());
                f.buffer_mut()
                    .set_style(Rect::new(0, 0, 10, 1), Style::default().bg(SELECTED_MARK));
                motion.finish_frame(f);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        // 30° per character: 中 is the first, a the second, b the third,
        // although they sit at cells 1, 4 and 6.
        let hue = |glyph: f32| hsl(glyph * 30.0, 0.9, 0.74);
        assert_eq!(buffer[(1, 0)].fg, hue(0.0));
        assert_eq!(buffer[(4, 0)].fg, hue(1.0));
        assert_eq!(buffer[(6, 0)].fg, hue(2.0));
        assert_eq!(buffer[(6, 0)].bg, BLUE, "text only keeps the background");
    }
    #[test]
    fn hsl_hits_primary_hues() {
        assert_eq!(hsl(0.0, 1.0, 0.5), Color::Rgb(255, 0, 0));
        assert_eq!(hsl(120.0, 1.0, 0.5), Color::Rgb(0, 255, 0));
        assert_eq!(hsl(-120.0, 1.0, 0.5), Color::Rgb(0, 0, 255));
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
            expire(&motion);
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
