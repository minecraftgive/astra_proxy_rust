//! Command and profile selection share the client's translated intent catalog.
use super::ViewEvent;
use crate::model::AppModel;
use astra_client::{i18n::t, palette::PaletteMode};
use gpui::{prelude::*, *};
use gpui_component::{
    input::{Input, InputEvent, InputState},
    ActiveTheme,
};
use std::sync::Arc;

actions!(astra_palette, [Previous, Next, Submit, Dismiss]);
struct Bindings;
impl Global for Bindings {}
pub struct PaletteContext {
    pub page: usize,
    pub window_id: u64,
    pub editor_active: bool,
}
pub struct View {
    model: Entity<AppModel>,
    search: Entity<InputState>,
    mode: PaletteMode,
    page: usize,
    window_id: u64,
    editor_active: bool,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}
impl View {
    pub fn new(
        model: Entity<AppModel>,
        search: Entity<InputState>,
        mode: PaletteMode,
        context: PaletteContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if !cx.has_global::<Bindings>() {
            cx.set_global(Bindings);
            let context = Some("AstraPalette > Input || (AstraPalette && !Input)");
            cx.bind_keys([
                KeyBinding::new("up", Previous, context),
                KeyBinding::new("down", Next, context),
                KeyBinding::new("enter", Submit, context),
                KeyBinding::new("escape", Dismiss, context),
            ]);
        }
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.selected = 0;
                    cx.notify();
                }
            }),
        ];
        search.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        Self {
            model,
            search,
            mode,
            page: context.page,
            window_id: context.window_id,
            editor_active: context.editor_active,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }
    fn entries(&self, cx: &App) -> Vec<astra_client::palette::Entry> {
        let model = self.model.read(cx);
        let editor_active = self.editor_active && model.editor_owned_by(self.window_id);
        model.client.palette_matches_for_view(
            self.mode,
            self.search.read(cx).value().as_str(),
            self.page,
            editor_active,
        )
    }
    fn select(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.entries(cx).len();
        if count > 0 {
            self.selected = (self.selected as isize + step).rem_euclid(count as isize) as usize;
            self.scroll
                .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        }
        cx.notify();
    }
    fn run(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries(cx).into_iter().nth(index) else {
            return;
        };
        if entry.unavailable.is_some() {
            return;
        }
        let input = self.model.update(cx, |model, cx| {
            let editor_active = self.editor_active && model.editor_owned_by(self.window_id);
            let input = model
                .tokio(|client| client.run_palette_entry_for_view(entry, self.page, editor_active));
            cx.notify();
            input
        });
        let page = self.model.read(cx).client.page;
        if page != self.page {
            cx.emit(ViewEvent::Navigate(page));
        }
        cx.emit(ViewEvent::Input(input));
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = Arc::new(self.entries(cx));
        self.selected = self.selected.min(entries.len().saturating_sub(1));
        div()
            .key_context("AstraPalette")
            .size_full()
            .flex()
            .flex_col()
            .p_5()
            .gap_3()
            .on_action(cx.listener(|this, _: &Previous, _, cx| {
                this.select(-1, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &Next, _, cx| {
                this.select(1, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &Submit, _, cx| {
                this.run(this.selected, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|_, _: &Dismiss, _, cx| {
                cx.emit(ViewEvent::Dismiss);
                cx.stop_propagation();
            }))
            .child(div().text_xl().child(t(match self.mode {
                PaletteMode::Commands => "Command palette",
                PaletteMode::Profiles(_) => "Switch current profile",
            })))
            .child(Input::new(&self.search))
            .child(
                uniform_list(
                    "palette-entries",
                    entries.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let entry = &entries[index];
                                div()
                                    .id(("entry", index))
                                    .h(px(58.))
                                    .px_3()
                                    .flex()
                                    .flex_col()
                                    .justify_center()
                                    .rounded_md()
                                    .when(index == this.selected, |row| row.bg(cx.theme().accent))
                                    .text_color(if entry.unavailable.is_some() {
                                        cx.theme().muted_foreground
                                    } else {
                                        cx.theme().foreground
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .justify_between()
                                            .child(format!("{} · {}", t(entry.scope), entry.label))
                                            .child(entry.key),
                                    )
                                    .children(
                                        entry
                                            .unavailable
                                            .map(|reason| div().text_xs().child(t(reason))),
                                    )
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.run(index, cx)),
                                    )
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
    }
}

#[cfg(test)]
#[path = "../test/palette_view_test.rs"]
mod tests;
