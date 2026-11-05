//! Node transfer keeps selection and destination explicit; imports only edit the draft.
use super::*;

pub(super) fn export_path(
    input: &str,
    directory: &std::path::Path,
    share: bool,
) -> Result<PathBuf> {
    let mut path = crate::import::path(input, directory)?;
    if path.is_dir()
        || (!path.is_file() && path.extension().is_none())
        || input.trim_end_matches(['\'', '"']).ends_with(['/', '\\'])
    {
        path.push(if share { "nodes.txt" } else { "nodes.toml" });
    }
    Ok(path)
}

impl Ui {
    pub(super) fn start_node_export(&mut self, share: bool) -> Input {
        let result = (|| {
            anyhow::ensure!(self.kind == Kind::Proxy, "Select a proxy configuration");
            let editor = self.editor.as_ref().context("Missing editor")?;
            anyhow::ensure!(
                editor.browsing(),
                "Confirm or cancel the field before exporting"
            );
            if share {
                editor.export_selected_node_share()
            } else {
                editor.export_selected_node()
            }
        })();
        match result {
            Ok(text) => self.prompt(Prompt::ExportNodeDestination {
                text,
                share,
                clipboard: false,
            }),
            Err(error) => self.fail(format!("{error:#}"), Resume::Editor),
        }
        Input::Idle
    }

    pub(super) fn prepare_node_import(
        &mut self,
        input: &str,
        mode: crate::import::MergeMode,
    ) -> Result<()> {
        anyhow::ensure!(
            self.kind == Kind::Proxy,
            "Configuration does not match this core"
        );
        let name = self.editor.as_ref().context("Missing editor")?.name.clone();
        let candidates = crate::import::node_candidates_at(input, &self.working_dir)?;
        let selected = (0..candidates.tags.len()).collect();
        let path = self.draft_key(&name).1;
        self.prompt(Prompt::ImportNodeSelection {
            name,
            path,
            candidates,
            selected,
            cursor: 0,
            mode,
        });
        Ok(())
    }

    fn copy_node_export_with(
        &mut self,
        text: &str,
        copy: impl FnOnce(&str) -> Result<()>,
    ) -> Result<()> {
        copy(text)?;
        self.message = "Selected nodes copied to clipboard".into();
        Ok(())
    }

    pub(super) fn key_node_transfer(&mut self, key: event::KeyEvent) -> Option<Input> {
        self.key_node_transfer_with(key, crate::clipboard::copy)
    }

    fn key_node_transfer_with(
        &mut self,
        key: event::KeyEvent,
        copy: impl FnOnce(&str) -> Result<()>,
    ) -> Option<Input> {
        match self.prompt.as_mut()? {
            Prompt::ExportNodeDestination { clipboard, .. } => match key.code {
                KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                    *clipboard = !*clipboard
                }
                KeyCode::Esc => self.prompt = None,
                KeyCode::Enter => {
                    let prompt = self.prompt.take().unwrap();
                    if let Prompt::ExportNodeDestination {
                        text,
                        share,
                        clipboard,
                    } = &prompt
                    {
                        if *clipboard {
                            if let Err(error) = self.copy_node_export_with(text, copy) {
                                self.fail(
                                    format!("{error:#}"),
                                    Resume::Prompt(prompt, String::new()),
                                );
                            }
                        } else {
                            self.prompt(Prompt::ExportNode {
                                text: text.clone(),
                                share: *share,
                            });
                        }
                    }
                }
                _ => {}
            },
            Prompt::ImportNodeSelection {
                candidates,
                selected,
                cursor,
                ..
            } => match key.code {
                KeyCode::Esc => self.prompt = None,
                KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = cursor
                        .saturating_add(1)
                        .min(candidates.tags.len().saturating_sub(1))
                }
                KeyCode::Home => *cursor = 0,
                KeyCode::End => *cursor = candidates.tags.len().saturating_sub(1),
                KeyCode::PageUp => *cursor = cursor.saturating_sub(10),
                KeyCode::PageDown => {
                    *cursor = cursor
                        .saturating_add(10)
                        .min(candidates.tags.len().saturating_sub(1))
                }
                KeyCode::Char(' ') | KeyCode::Char('v') => {
                    if selected.contains(cursor) {
                        selected.retain(|i| i != cursor);
                    } else if *cursor < candidates.tags.len() {
                        selected.push(*cursor);
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('V') => {
                    if selected.len() == candidates.tags.len() {
                        selected.clear();
                    } else {
                        *selected = (0..candidates.tags.len()).collect();
                    }
                }
                KeyCode::Enter => {
                    let prompt = self.prompt.take().unwrap();
                    let result = self.apply_node_import(&prompt);
                    if let Err(error) = result {
                        self.fail(format!("{error:#}"), Resume::Prompt(prompt, String::new()));
                    }
                }
                _ => {}
            },
            _ => return None,
        }
        Some(Input::Idle)
    }

    fn apply_node_import(&mut self, prompt: &Prompt) -> Result<()> {
        let Prompt::ImportNodeSelection {
            name,
            path,
            candidates,
            selected,
            mode,
            ..
        } = prompt
        else {
            return Ok(());
        };
        anyhow::ensure!(
            self.kind == Kind::Proxy && self.draft_key(name).1 == *path,
            "Import target changed; reopen node import"
        );
        let editor = self.editor.as_mut().context("Missing editor")?;
        anyhow::ensure!(
            editor.name == *name,
            "Import target changed; reopen node import"
        );
        let imported =
            crate::import::merge_selected_nodes(&editor.text(), candidates, selected, *mode)?;
        editor.import(imported.text)?;
        self.message = "Imported nodes to draft · Ctrl-S save".into();
        Ok(())
    }

    pub(super) fn render_node_transfer(&self, frame: &mut Frame<'_>) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let (items, cursor, title, footer) = match prompt {
            Prompt::ExportNodeDestination {
                clipboard, share, ..
            } => (
                vec![
                    format!(
                        "{} · {}",
                        t("File or folder"),
                        if *share { "nodes.txt" } else { "nodes.toml" }
                    ),
                    t("Clipboard").to_owned(),
                ],
                usize::from(*clipboard),
                t("Export selected nodes").to_owned(),
                t("Tab / arrows choose · Enter export · Esc cancel"),
            ),
            Prompt::ImportNodeSelection {
                candidates,
                selected,
                cursor,
                ..
            } => {
                let items = candidates
                    .tags
                    .iter()
                    .enumerate()
                    .map(|(index, tag)| {
                        format!(
                            "[{}] {}",
                            if selected.contains(&index) { "x" } else { " " },
                            tag.replace(['\r', '\n', '\t'], " ")
                        )
                    })
                    .collect();
                (
                    items,
                    *cursor,
                    format!(
                        "{} · {}/{}",
                        t("Select nodes to import"),
                        selected.len(),
                        candidates.tags.len()
                    ),
                    t("Space select · a all/clear · Enter import · Esc cancel"),
                )
            }
            _ => return,
        };
        let mut measure = format!("{title}\n{footer}\n{}", items.join("\n"));
        let warnings = match prompt {
            Prompt::ImportNodeSelection { candidates, .. } => candidates.warnings.join("\n"),
            _ => String::new(),
        };
        if !warnings.is_empty() {
            measure.push_str(&format!("\n{warnings}"));
        }
        let popup = crate::help::popup(frame.area(), &measure);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_bottom(footer);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(if warnings.is_empty() { 0 } else { 3 }),
        ])
        .split(inner);
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(Color::Cyan));
        frame.render_stateful_widget(
            list,
            chunks[0],
            &mut ListState::default().with_selected(Some(cursor)),
        );
        if !warnings.is_empty() {
            frame.render_widget(
                Paragraph::new(warnings)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Yellow)),
                chunks[1],
            );
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_node_transfer_test.rs"]
mod tests;
