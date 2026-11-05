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
