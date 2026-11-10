//! A small isolated chooser process; it never starts a manager or a log session.
use astra_client::{i18n::t, package_choice::Choice};
use gpui::{
    div, prelude::*, px, size, App, AppContext, Bounds, Context, Render, Window, WindowBounds,
    WindowOptions,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    ActiveTheme, Root,
};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc,
};

pub const OPENED: i32 = 10;
pub const SELECTED: i32 = 11;
pub const CANCELLED: i32 = 12;

/// Bind the isolated window to the exact choices held by the launching process.
pub struct Request {
    opened: String,
    selected: String,
}
impl Request {
    pub fn take(arguments: &mut Vec<String>) -> anyhow::Result<Option<Self>> {
        if arguments.first().is_none_or(|a| a != "--choose-version") {
            return Ok(None);
        }
        anyhow::ensure!(arguments.len() >= 3, "Missing version selection identities");
        let request = Self {
            opened: arguments[1].clone(),
            selected: arguments[2].clone(),
        };
        arguments.drain(..3);
        Ok(Some(request))
    }
    pub fn verify(self, choice: Option<Choice>) -> anyhow::Result<Choice> {
        let choice = choice
            .ok_or_else(|| anyhow::anyhow!("Version selection changed; reopen the interface"))?;
        anyhow::ensure!(
            choice.opened.id() == self.opened && choice.selected.id() == self.selected,
            "Version selection changed; reopen the interface"
        );
        Ok(choice)
    }
}

pub fn run(choice: Choice) -> i32 {
    let result = Arc::new(AtomicI32::new(CANCELLED));
    let output = result.clone();
    gpui_platform::application()
        .with_quit_mode(gpui::QuitMode::Explicit)
        .run(move |cx| {
            let (quit, requests) = async_channel::unbounded();
            crate::lifecycle::install_quit_hook(quit);
            gpui_component::init(cx);
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(680.), px(420.)),
                        cx,
                    ))),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(t("Choose which version to open").into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|_| Picker {
                        choice,
                        result: output,
                    });
                    view.update(cx, |_, cx| {
                        cx.spawn(async move |view, cx| {
                            if requests.recv().await.is_ok() {
                                let _ = view.update(cx, |view, cx| view.finish(CANCELLED, cx));
                            }
                        })
                        .detach();
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                },
            );
            if opened.is_err() {
                crate::lifecycle::finish(cx);
            }
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    crate::lifecycle::finish(cx);
                }
            })
            .detach();
            cx.activate(true);
        });
    result.load(Ordering::Acquire)
}
struct Picker {
    choice: Choice,
    result: Arc<AtomicI32>,
}
impl Picker {
    fn finish(&mut self, result: i32, cx: &mut App) {
        self.result.store(result, Ordering::Release);
        crate::lifecycle::finish(cx);
    }
}
impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("package-choice")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_xl().child(t("Choose which version to open")))
            .child(div().child(self.choice.opened.label()))
            .child(
                Button::new("opened")
                    .primary()
                    .label(t("Use the opened version"))
                    .on_click(cx.listener(|this, _, _, cx| this.finish(OPENED, cx))),
            )
            .child(div().child(self.choice.selected.label()))
            .child(
                Button::new("selected")
                    .label(t("Use the selected version"))
                    .on_click(cx.listener(|this, _, _, cx| this.finish(SELECTED, cx))),
            )
            .child(div().text_sm().child(t(
                "Choosing an interface does not restart running instances.",
            )))
            .child(
                Button::new("cancel")
                    .label(t("Cancel"))
                    .on_click(cx.listener(|this, _, _, cx| this.finish(CANCELLED, cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn choice() -> Choice {
        let package = |name: &str| astra_client::package_choice::Package {
            directory: format!("/packages/{name}").into(),
            release_id: "0.1.0".into(),
            manifest_sha256: name.repeat(64),
        };
        Choice {
            opened: package("b"),
            selected: package("a"),
        }
    }

    #[test]
    fn chooser_handoff_keeps_parent_package_identities() {
        let choice = choice();
        let mut args = vec![
            "--choose-version".into(),
            choice.opened.id(),
            choice.selected.id(),
            "--state-dir".into(),
            "/state".into(),
        ];
        let request = Request::take(&mut args).unwrap().unwrap();
        assert_eq!(args, ["--state-dir", "/state"]);
        assert!(request.verify(Some(choice.clone())).is_ok());
        let request = Request {
            opened: choice.opened.id(),
            selected: choice.selected.id(),
        };
        let mut changed = choice;
        changed.selected.directory = "/other-selected-package".into();
        assert!(request.verify(Some(changed)).is_err());
        assert!(Request::take(&mut vec!["--choose-version".into()]).is_err());
    }

    #[gpui::test]
    fn picker_rendering_never_chooses_a_version(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let result = Arc::new(AtomicI32::new(CANCELLED));
        let output = result.clone();
        let _window = cx.add_window(|window, cx| {
            let view = cx.new(|_| Picker {
                choice: choice(),
                result: output,
            });
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(result.load(Ordering::Acquire), CANCELLED);
    }
}
