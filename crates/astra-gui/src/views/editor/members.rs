use super::*;
use gpui::AnyElement;
use gpui_component::checkbox::Checkbox;
impl View {
    pub(super) fn members(
        &self,
        options: Vec<String>,
        chosen: Vec<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = self.member_search.read(cx).value().to_lowercase();
        let rows: Vec<_> = options
            .into_iter()
            .enumerate()
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .collect();
        let mut order = div().flex().flex_col().gap_1();
        for (index, name) in chosen.iter().enumerate() {
            let up = name.clone();
            let down = name.clone();
            order = order.child(
                div()
                    .flex()
                    .gap_2()
                    .child(format!("{}. {}", index + 1, name))
                    .child(Button::new(("chain-member-up", index)).label("↑").on_click(
                        cx.listener(move |view, _, _, cx| {
                            view.edit(
                                |editor| {
                                    editor.move_member(&up, false);
                                    Ok(())
                                },
                                cx,
                            );
                        }),
                    ))
                    .child(
                        Button::new(("chain-member-down", index))
                            .label("↓")
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.edit(
                                    |editor| {
                                        editor.move_member(&down, true);
                                        Ok(())
                                    },
                                    cx,
                                );
                            })),
                    ),
            );
        }
        let view = cx.entity().downgrade();
        Card::new(t("Group members"))
            .child(
                order
                    .id("chain-member-order")
                    .max_h(px(160.))
                    .overflow_y_scroll(),
            )
            .child(Input::new(&self.member_search))
            .child(
                gpui::uniform_list("editor-member-list", rows.len(), move |range, _, _| {
                    range
                        .map(|i| {
                            let (index, label) = &rows[i];
                            let index = *index;
                            let view = view.clone();
                            div().h(px(32.)).child(
                                Checkbox::new(("editor-member", index))
                                    .label(label.clone())
                                    .checked(chosen.contains(label))
                                    .on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |view, cx| {
                                            view.edit(
                                                |editor| {
                                                    editor.toggle_member(index);
                                                    Ok(())
                                                },
                                                cx,
                                            );
                                        });
                                    }),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .h(px(180.)),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("editor-members-cancel")
                            .label(t("Cancel"))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.edit(
                                    |editor| {
                                        editor.cancel_input();
                                        Ok(())
                                    },
                                    cx,
                                );
                                window.focus(&view.focus, cx);
                            })),
                    )
                    .child(
                        Button::new("editor-members-accept")
                            .primary()
                            .label(t("Confirm"))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.edit(Editor::commit_input, cx);
                                window.focus(&view.focus, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
    pub(super) fn outbound_picker(&self, selected: usize, cx: &mut Context<Self>) -> AnyElement {
        let mut choices = div().flex().flex_wrap().gap_2();
        for (index, kind) in astra_app::schema::OUTBOUND_KINDS.iter().enumerate() {
            let mut button = Button::new(("editor-new-outbound", index))
                .label(*kind)
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.edit(|editor| editor.create_outbound(index), cx);
                    view.sync(window, cx);
                    window.focus(&view.focus, cx);
                }));
            if selected == index {
                button = button.primary();
            }
            choices = choices.child(button);
        }
        Card::new(t("New outbound"))
            .child(choices)
            .child(
                Button::new("editor-outbound-cancel")
                    .label(t("Cancel"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.edit(
                            |editor| {
                                editor.cancel_input();
                                Ok(())
                            },
                            cx,
                        );
                    })),
            )
            .into_any_element()
    }
}
