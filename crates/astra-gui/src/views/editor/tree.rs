use super::*;
use crate::widgets::TokenText;
use gpui::{AnyElement, Pixels, Point};

#[derive(Clone)]
struct DraggedRow {
    index: usize,
    label: String,
}
struct DragLabel(String);
impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .bg(crate::theme::palette(cx).surface)
            .border_1()
            .border_color(cx.theme().border)
            .child(self.0.clone())
    }
}
impl View {
    pub(super) fn tree_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("editor-new")
                    .label(t("New"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(Editor::new_entry, cx);
                        window.focus(&view.focus, cx);
                    })),
            )
            .child(
                Button::new("editor-rename")
                    .label(t("Rename"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(Editor::rename_selected, cx);
                        view.sync(window, cx);
                    })),
            )
            .child(
                Button::new("editor-duplicate")
                    .label(t("Duplicate"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(Editor::duplicate, cx);
                    })),
            )
            .child(
                Button::new("editor-delete")
                    .label(t("Delete"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(Editor::delete_selected, cx);
                    })),
            )
            .child(
                Button::new("editor-undo").label(t("Undo")).on_click(
                    cx.listener(|view, _, window, cx| view.command(Id::Undo, window, cx)),
                ),
            )
            .child(
                Button::new("editor-redo").label(t("Redo")).on_click(
                    cx.listener(|view, _, window, cx| view.command(Id::Redo, window, cx)),
                ),
            )
            .child(
                Button::new("editor-mark")
                    .label(t("Mark"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(|editor| editor.mark_selected(false), cx);
                    })),
            )
            .child(
                Button::new("editor-mark-all")
                    .label(t("Mark all"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(|editor| editor.mark_selected(true), cx);
                    })),
            )
            .child(Button::new("editor-up").label("↑").on_click(cx.listener(
                |view, _, window, cx| {
                    if !view.commit_pending(window, cx) {
                        return;
                    }
                    view.edit(|editor| editor.move_selected(false), cx);
                },
            )))
            .child(Button::new("editor-down").label("↓").on_click(cx.listener(
                |view, _, window, cx| {
                    if !view.commit_pending(window, cx) {
                        return;
                    }
                    view.edit(|editor| editor.move_selected(true), cx);
                },
            )))
            .child(
                Button::new("editor-expand")
                    .label(t("Expand all"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(
                            |editor| {
                                editor.expand_all(&editor.document()?);
                                Ok(())
                            },
                            cx,
                        );
                    })),
            )
            .child(
                Button::new("editor-collapse")
                    .label(t("Collapse all"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !view.commit_pending(window, cx) {
                            return;
                        }
                        view.edit(
                            |editor| {
                                editor.collapse_all();
                                Ok(())
                            },
                            cx,
                        );
                    })),
            )
            .into_any_element()
    }
    pub(super) fn tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self
            .model
            .read(cx)
            .client
            .editor
            .as_ref()
            .expect("editor render");
        let rows = editor.presentation().map(|p| p.tree).unwrap_or_default();
        let selected = editor.form.selected;
        let view = cx.entity().downgrade();
        gpui::uniform_list("editor-tree", rows.len(), move |range, _, cx| {
            range
                .map(|index| {
                    let row = &rows[index];
                    let toggle_view = view.clone();
                    let click_view = view.clone();
                    let drop_view = view.clone();
                    let mut item = div()
                        .id(("editor-tree-row", index))
                        .h(px(36.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl(px(8. + row.depth as f32 * 14.))
                        .pr_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .on_click(move |_, window, cx| {
                            let _ = click_view.update(cx, |view, cx| {
                                if view.commit_pending(window, cx) {
                                    view.edit(|editor| editor.select_row(index), cx);
                                    window.focus(&view.focus, cx);
                                }
                            });
                        })
                        .on_drag(
                            DraggedRow {
                                index,
                                label: row.title.clone(),
                            },
                            |drag, _: Point<Pixels>, _, cx| {
                                cx.new(|_| DragLabel(drag.label.clone()))
                            },
                        )
                        .on_drop(move |drag: &DraggedRow, window, cx| {
                            let _ = drop_view.update(cx, |view, cx| {
                                if view.commit_pending(window, cx) {
                                    view.edit(
                                        |editor| {
                                            editor.select_row(drag.index)?;
                                            editor.move_to_row(index)
                                        },
                                        cx,
                                    );
                                }
                            });
                        });
                    if selected == index {
                        item = item.bg(crate::theme::palette(cx).accent_soft);
                    }
                    if let Some(expanded) = row.expanded {
                        item = item.child(
                            Button::new(("editor-fold", index))
                                .label(if expanded { "▾" } else { "▸" })
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    let _ = toggle_view.update(cx, |view, cx| {
                                        if view.commit_pending(window, cx) {
                                            view.edit(
                                                |editor| {
                                                    editor.select_row(index)?;
                                                    if expanded {
                                                        editor.collapse_current()
                                                    } else {
                                                        editor.enter(true)
                                                    }
                                                },
                                                cx,
                                            );
                                            window.focus(&view.focus, cx);
                                        }
                                    });
                                }),
                        );
                    }
                    if row.marked {
                        item = item.child(div().text_color(cx.theme().primary).child("✓"));
                    }
                    item.child(div().min_w_0().flex_1().truncate().child(row.title.clone()))
                        .child(TokenText::new(row.value.clone()))
                })
                .collect::<Vec<_>>()
        })
        .h(px(470.))
        .w(px(370.))
        .into_any_element()
    }
}
