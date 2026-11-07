use super::*;
use gpui::AnyElement;
impl View {
    pub(super) fn transfer(
        &mut self,
        export: bool,
        share: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.commit_pending(window, cx) {
            return;
        }
        self.dispatch(
            |client| {
                let action = if export {
                    EditorAction::ExportItems { share }
                } else {
                    EditorAction::ImportItems {
                        rules: client.kind == astra_client::protocol::Kind::Rule,
                    }
                };
                client.editor_action(action)
            },
            cx,
        );
    }
    pub(super) fn transfer_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_external = self.model.read(cx).client.external_draft.is_some();
        let mut bar = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("editor-import")
                    .label(t("Import into draft"))
                    .on_click(
                        cx.listener(|view, _, window, cx| view.command(Id::Import, window, cx)),
                    ),
            )
            .child(
                Button::new("editor-import-nodes")
                    .label(t("Import nodes or rules"))
                    .on_click(
                        cx.listener(|view, _, window, cx| view.transfer(false, false, window, cx)),
                    ),
            )
            .child(
                Button::new("editor-export-nodes")
                    .label(t("Export selection"))
                    .on_click(
                        cx.listener(|view, _, window, cx| view.transfer(true, false, window, cx)),
                    ),
            )
            .child(
                Button::new("editor-export-links")
                    .label(t("Export share links"))
                    .on_click(
                        cx.listener(|view, _, window, cx| view.transfer(true, true, window, cx)),
                    ),
            )
            .child(
                Button::new("editor-external")
                    .label(t("External editor"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.command(Id::ExternalEditor, window, cx)
                    })),
            )
            .child(
                Button::new("editor-subscriptions")
                    .label(t("Update subscriptions"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        if view.commit_pending(window, cx) {
                            view.dispatch(
                                |client| client.editor_action(EditorAction::UpdateSubscriptions),
                                cx,
                            );
                        }
                    })),
            );
        if has_external {
            bar = bar.child(
                Button::new("editor-read-external")
                    .primary()
                    .label(t("Read external changes"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.command(Id::ReadExternal, window, cx)
                    })),
            );
        }
        bar.into_any_element()
    }
}
