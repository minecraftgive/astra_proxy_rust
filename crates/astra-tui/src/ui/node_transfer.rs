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

#[cfg(test)]
mod tests {
    use super::*;

    const NODES: &str = "[[outbounds]]\ntag='first'\nkind='socks5'\nserver='127.0.0.1:1080'\n[[outbounds]]\ntag='second'\nkind='socks5'\nserver='127.0.0.1:1081'\n";

    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ui() -> Ui {
        let mut ui = Ui::new(Session::default());
        ui.page = PROFILES;
        let mut editor =
            Editor::new("proxy".into(), format!("standalone=true\n{NODES}")).with_kind(Kind::Proxy);
        for _ in 0..100 {
            if editor.selected_node_tag().ok().as_deref() == Some("first") {
                break;
            }
            editor.key(key(KeyCode::Down)).unwrap();
        }
        assert_eq!(editor.selected_node_tag().unwrap(), "first");
        editor.key(key(KeyCode::Char('V'))).unwrap();
        ui.editor = Some(editor);
        ui
    }

    #[test]
    fn node_transfer_exports_selected_nodes_to_new_folders_and_never_overwrites() {
        let fixture = crate::import::tests::Fixture::new();
        let mut ui = ui();
        let original = ui.editor.as_ref().unwrap().text();
        ui.key(event::KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
        ));
        assert!(matches!(
            ui.prompt,
            Some(Prompt::ExportNodeDestination {
                clipboard: false,
                ..
            })
        ));
        ui.key(key(KeyCode::Enter));
        let documents = astra_platform::platform::documents_dir().unwrap();
        assert_eq!(ui.working_dir, documents);
        assert_eq!(PathBuf::from(&ui.input), documents.join("nodes.toml"));
        assert!(ui.prefilled_input);
        let prompt = ui.prompt.take().unwrap();
        assert!(matches!(prompt, Prompt::ExportNode { share: false, .. }));
        ui.working_dir = fixture.0.clone();
        ui.submit(prompt.clone(), "\"新目录/with spaces\"".into())
            .unwrap();
        let file = fixture.0.join("新目录/with spaces/nodes.toml");
        let text = std::fs::read_to_string(&file).unwrap();
        let value: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(value["outbounds"].as_array().unwrap().len(), 2);
        assert!(ui.submit(prompt, file.display().to_string()).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), text);
        assert_eq!(ui.editor.as_ref().unwrap().text(), original);
        assert_eq!(
            export_path("new/folder/", &fixture.0, true).unwrap(),
            fixture.0.join("new/folder/nodes.txt")
        );
    }

    #[test]
    fn node_transfer_clipboard_uses_selected_share_links_and_reports_failure() {
        let mut ui = ui();
        ui.key(event::KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(matches!(
            ui.prompt,
            Some(Prompt::ExportNodeDestination { share: true, .. })
        ));
        ui.key(key(KeyCode::Tab));
        let mut copied = String::new();
        ui.key_node_transfer_with(key(KeyCode::Enter), |text| {
            copied = text.into();
            Ok(())
        });
        assert_eq!(copied.lines().count(), 2);
        assert!(copied.lines().all(|line| line.starts_with("socks5://")));
        assert!(ui.prompt.is_none());
        assert_eq!(ui.message, "Selected nodes copied to clipboard");

        ui.start_node_export(false);
        ui.key(key(KeyCode::Tab));
        ui.message.clear();
        ui.key_node_transfer_with(key(KeyCode::Enter), |_| anyhow::bail!("clipboard busy"));
        assert!(ui
            .recovery
            .as_ref()
            .unwrap()
            .message
            .contains("clipboard busy"));
        assert!(ui.message.is_empty());
        ui.recover_key(KeyCode::Char('c'));
        assert!(matches!(
            ui.prompt,
            Some(Prompt::ExportNodeDestination {
                clipboard: true,
                ..
            })
        ));
        ui.key(key(KeyCode::Esc));
        assert!(ui.prompt.is_none());
    }

    #[test]
    fn node_transfer_multiline_import_reviews_selection_and_keeps_cancel_and_undo() {
        let mut ui = ui();
        ui.editor = Some(
            Editor::new(
                "proxy".into(),
                "standalone=true\n[[outbounds]]\ntag='first'\nkind='direct'\n".into(),
            )
            .with_kind(Kind::Proxy),
        );
        let before = ui.editor.as_ref().unwrap().text();
        ui.prompt(Prompt::ImportNode(crate::import::MergeMode::Append));
        ui.paste_prompt(NODES);
        assert_eq!(ui.input, NODES);
        ui.key(key(KeyCode::Enter));
        assert!(
            matches!(&ui.prompt, Some(Prompt::ImportNodeSelection { selected, .. }) if selected == &[0, 1])
        );
        assert_eq!(ui.editor.as_ref().unwrap().text(), before);
        ui.key(key(KeyCode::Esc));
        assert_eq!(ui.editor.as_ref().unwrap().text(), before);

        ui.prepare_node_import(NODES, crate::import::MergeMode::Append)
            .unwrap();
        ui.key(key(KeyCode::Enter));
        assert!(
            ui.recovery.is_some(),
            "conflicting selected tag must be rejected"
        );
        assert_eq!(ui.editor.as_ref().unwrap().text(), before);
        ui.recover_key(KeyCode::Char('c'));
        ui.key(key(KeyCode::Char(' ')));
        ui.key(key(KeyCode::Enter));
        assert!(ui.recovery.is_none());
        let doc: toml::Value = toml::from_str(&ui.editor.as_ref().unwrap().text()).unwrap();
        assert_eq!(doc["outbounds"].as_array().unwrap().len(), 2);
        assert_eq!(doc["outbounds"][0]["kind"].as_str(), Some("direct"));
        assert_eq!(doc["outbounds"][1]["tag"].as_str(), Some("second"));
        ui.editor
            .as_mut()
            .unwrap()
            .key(event::KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::CONTROL,
            ))
            .unwrap();
        assert_eq!(ui.editor.as_ref().unwrap().text(), before);
    }

    #[test]
    fn node_transfer_picklists_render_in_chinese_and_reject_empty_or_changed_targets() {
        crate::i18n::with_language(crate::i18n::Language::Simplified, || {
            let mut ui = ui();
            ui.start_node_export(false);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 24)).unwrap();
            terminal.draw(|frame| ui.render(frame)).unwrap();
            let screen: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(screen.replace(' ', "").contains("剪贴板"), "{screen}");
            ui.prepare_node_import(NODES, crate::import::MergeMode::Append)
                .unwrap();
            for (width, height) in [(20, 6), (90, 24)] {
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.render(frame)).unwrap();
            }
            ui.key(key(KeyCode::Char('a')));
            let prompt = ui.prompt.as_ref().unwrap().clone();
            assert!(ui.apply_node_import(&prompt).is_err());
            ui.key(key(KeyCode::Char('a')));
            let prompt = ui.prompt.as_ref().unwrap().clone();
            ui.editor.as_mut().unwrap().name = "different".into();
            assert!(ui
                .apply_node_import(&prompt)
                .unwrap_err()
                .to_string()
                .contains("target changed"));
        });
    }

    #[test]
    fn node_transfer_selection_is_bound_to_config_path_and_ignores_repeat_toggles() {
        let mut ui = ui();
        ui.rows.push(Row {
            instance: Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: "first.toml".into(),
            },
            pid: None,
            state: "Stopped".into(),
            uptime: 0,
            changed: false,
        });
        ui.prepare_node_import(NODES, crate::import::MergeMode::Append)
            .unwrap();
        let mut held = key(KeyCode::Char(' '));
        held.kind = KeyEventKind::Repeat;
        ui.key(held);
        assert!(
            matches!(&ui.prompt, Some(Prompt::ImportNodeSelection { selected, .. }) if selected == &[0, 1])
        );
        assert!(matches!(
            ui.key(event::KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL
            )),
            Input::Idle
        ));
        assert!(matches!(
            ui.prompt,
            Some(Prompt::ImportNodeSelection { .. })
        ));
        let prompt = ui.prompt.as_ref().unwrap().clone();
        ui.rows[0].instance.config = "different.toml".into();
        assert!(ui
            .apply_node_import(&prompt)
            .unwrap_err()
            .to_string()
            .contains("target changed"));
        ui.key(key(KeyCode::Esc));
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let help = ui.command_help();
            assert!(help.contains("Import nodes"));
            assert!(help.contains("Export selected nodes (TOML)"));
            assert!(help.contains("Export selected nodes (share links)"));
        });
    }
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
