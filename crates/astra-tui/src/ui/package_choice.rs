use super::*;
use astra_client::package_choice::Choice;

/// This screen runs before manager discovery, sessions and startup jobs.
pub(crate) fn choose_package(choice: &Choice) -> Result<Option<bool>> {
    let mut screen = TerminalSession::enter(TerminalOptions {
        mouse: false,
        bracketed_paste: false,
    })?;
    let result = (|| -> Result<Option<bool>> {
        let mut terminal = astra_tui_kit::frame::screen()?;
        loop {
            terminal.draw(|frame| {
                let text = format!(
                    "{}\n\n1 · {}\n{}\n\n2 · {}\n{}\n\nEsc · {}\n\n{}",
                    t("Choose which version to open"),
                    t("Use the opened version"),
                    choice.opened.label(),
                    t("Use the selected version"),
                    choice.selected.label(),
                    t("Cancel"),
                    t("Choosing an interface does not restart running instances.")
                );
                frame.render_widget(
                    Paragraph::new(text).wrap(Wrap { trim: false }),
                    frame.area(),
                );
            })?;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('1') => return Ok(Some(true)),
                    KeyCode::Char('2') => return Ok(Some(false)),
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        return Ok(None)
                    }
                    _ => {}
                }
            }
        }
    })();
    screen.suspend()?;
    result
}
