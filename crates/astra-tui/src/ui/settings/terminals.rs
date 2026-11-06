//! Terminal picker key mapping and rendering.
use super::*;
impl Ui {
    pub(super) fn terminal_picker_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        if !self.vim_keys && matches!(key.code, KeyCode::Char('j' | 'k')) {
            return Some(Input::Idle);
        }
        let choices = self.terminal_choices();
        let current = choices
            .iter()
            .position(|choice| Some(&choice.id) == self.terminals.picker.as_ref())
            .unwrap_or(0);
        let index = match key.code {
            KeyCode::Esc => {
                self.terminals.picker = None;
                return Some(Input::Idle);
            }
            KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (current + 1).min(choices.len() - 1),
            KeyCode::Home => 0,
            KeyCode::End => choices.len() - 1,
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.choose_terminal(current);
                return Some(Input::Idle);
            }
            KeyCode::Char('r') => {
                self.request_terminal_scan();
                return Some(Input::Idle);
            }
            _ => return Some(Input::Idle),
        };
        self.terminals.picker = Some(choices[index].id.clone());
        Some(Input::Idle)
    }

    pub(super) fn render_terminal_choice(
        &self,
        f: &mut Frame<'_>,
        rect: Rect,
        row: usize,
        mut spans: Vec<Span<'static>>,
    ) {
        let choices = self.terminal_choices();
        let choice = &choices[self.setting_index(Field::Terminal)];
        spans.push(Span::styled(
            format!(" {} ▾ ", choice.label),
            Style::default().bg(BLUE).fg(if choice.available {
                Color::White
            } else {
                Color::Yellow
            }),
        ));
        if self.terminals.pending || self.terminals.scan.is_some() {
            spans.push(Span::styled(
                format!("  {}", t("Scanning…")),
                Style::default().fg(MUTED),
            ));
        }
        self.hit(rect, Target::Setting(row, None));
        f.render_widget(Paragraph::new(Line::from(spans)), rect);
    }

    pub(super) fn render_terminal_picker(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(selected) = &self.terminals.picker else {
            return;
        };
        let choices = self.terminal_choices();
        let current = choices
            .iter()
            .position(|choice| &choice.id == selected)
            .unwrap_or(0);
        let width = area.width.saturating_sub(2).min(64);
        let height = area.height.saturating_sub(2).min(choices.len() as u16 + 3);
        if width < 4 || height < 4 {
            return;
        }
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        f.render_widget(Clear, rect);
        self.hits.borrow_mut().clear();
        let block = Block::default().borders(Borders::ALL).title(t("Terminal"));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let visible = inner.height.saturating_sub(1) as usize;
        let offset = current.saturating_sub(visible.saturating_sub(1));
        let row = ALL_FIELDS
            .iter()
            .position(|field| *field == Field::Terminal)
            .unwrap();
        for (index, choice) in choices.iter().enumerate().skip(offset).take(visible) {
            let item = Rect::new(inner.x, inner.y + (index - offset) as u16, inner.width, 1);
            let style = Style::default().fg(if choice.available {
                Color::White
            } else {
                Color::DarkGray
            });
            f.render_widget(
                Paragraph::new(format!(" {}", choice.label)).style(if index == current {
                    style.bg(BLUE)
                } else {
                    style
                }),
                item,
            );
            self.hit(item, Target::Setting(row, Some(index)));
        }
        f.render_widget(
            Paragraph::new(t("Enter select · Esc close · r scan again"))
                .style(Style::default().fg(MUTED)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}
