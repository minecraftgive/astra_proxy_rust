//! A separate, minimal process: the secret never enters Client, a channel or a log.
use astra_client::i18n::t;
use gpui::{
    canvas, div, prelude::*, px, size, App, AppContext, Bounds, ClipboardItem, Context,
    ElementInputHandler, EntityInputHandler, FocusHandle, Focusable, KeyDownEvent, Pixels, Point,
    Render, TextInputConfiguration, UTF16Selection, Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    ActiveTheme, Root,
};
use std::{
    io::Write,
    ops::Range,
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
};
use zeroize::{Zeroize, Zeroizing};

const CAPACITY: usize = 4096;
pub fn requested(args: &[String]) -> bool {
    requested_with_marker(args, std::env::var_os("ASTRA_GUI_ASKPASS").as_deref())
}
fn requested_with_marker(args: &[String], marker: Option<&std::ffi::OsStr>) -> bool {
    args.iter().any(|arg| arg == "--askpass") || marker == Some(std::ffi::OsStr::new("1"))
}

#[cfg(unix)]
fn parent_id() -> i32 {
    unsafe extern "C" {
        fn getppid() -> i32;
    }
    // No arguments or borrowed memory cross this libc ABI boundary.
    unsafe { getppid() }
}

/// Cancellation, closing the window, or failure to open it all return 1.
pub fn run() -> i32 {
    #[cfg(unix)]
    let parent = parent_id();
    #[cfg(unix)]
    if parent <= 1 {
        return 1;
    }
    let _ = astra_client::i18n::init();
    let outcome = Arc::new(AtomicI32::new(1));
    let result = outcome.clone();
    gpui_platform::application()
        .with_quit_mode(gpui::QuitMode::Explicit)
        .run(move |cx| {
            #[cfg(unix)]
            if parent_id() != parent {
                crate::lifecycle::finish(cx);
                return;
            }
            let (native_quit, native_quit_requests) = async_channel::unbounded();
            crate::lifecycle::install_quit_hook(native_quit);
            gpui_component::init(cx);
            let bounds = Bounds::centered(None, size(px(440.), px(220.)), cx);
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    is_resizable: false,
                    ..Default::default()
                },
                |window, cx| {
                    window.set_window_title(t("Administrator password"));
                    let secret = cx.new(|cx| Secret::new(outcome, window, cx));
                    secret.update(cx, |_, cx| {
                        cx.spawn(async move |secret, cx| {
                            if native_quit_requests.recv().await.is_ok() {
                                let _ = secret.update(cx, |secret, cx| secret.finish(false, cx));
                            }
                        })
                        .detach();
                    });
                    let closing = secret.downgrade();
                    window.on_window_should_close(cx, move |_, cx| {
                        let _ = closing.update(cx, |secret, _| {
                            secret.clear();
                            secret.outcome.store(1, Ordering::Release);
                        });
                        true
                    });
                    #[cfg(unix)]
                    {
                        secret.update(cx, |_, cx| {
                            cx.spawn(async move |secret, cx| loop {
                                cx.background_executor()
                                    .timer(std::time::Duration::from_millis(150))
                                    .await;
                                if parent_id() != parent {
                                    let _ =
                                        secret.update(cx, |secret, cx| secret.finish(false, cx));
                                    break;
                                }
                            })
                            .detach();
                        });
                    }
                    cx.new(|cx| Root::new(secret, window, cx))
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

// Two allocations of a fixed maximum size prevent reallocation from leaving old
// password bytes behind. Only the masked count is passed to GPUI's text renderer.
struct Secret {
    value: Zeroizing<String>,
    scratch: Zeroizing<String>,
    selection: Range<usize>, // UTF-8 boundaries
    marked: Option<Range<usize>>,
    focus: FocusHandle,
    outcome: Arc<AtomicI32>,
}
impl Secret {
    fn new(outcome: Arc<AtomicI32>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            value: Zeroizing::new(String::with_capacity(CAPACITY)),
            scratch: Zeroizing::new(String::with_capacity(CAPACITY)),
            selection: 0..0,
            marked: None,
            focus,
            outcome,
        }
    }
    fn byte_offset(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (byte, ch) in self.value.char_indices() {
            if units + ch.len_utf16() > utf16 {
                return byte;
            }
            units += ch.len_utf16();
        }
        self.value.len()
    }
    fn utf16_offset(&self, byte: usize) -> usize {
        self.value[..byte].encode_utf16().count()
    }
    fn byte_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.byte_offset(range.start);
        start..self.byte_offset(range.end).max(start)
    }
    fn replace(&mut self, range: Range<usize>, text: &str) -> bool {
        if text.contains(['\0', '\r', '\n'])
            || self.value.len() - range.len() + text.len() > CAPACITY
        {
            return false;
        }
        self.scratch.push_str(&self.value[..range.start]);
        self.scratch.push_str(text);
        self.scratch.push_str(&self.value[range.end..]);
        self.value.zeroize();
        std::mem::swap(&mut self.value, &mut self.scratch);
        self.selection = range.start + text.len()..range.start + text.len();
        true
    }
    fn clear(&mut self) {
        self.value.zeroize();
        self.scratch.zeroize();
        self.selection = 0..0;
        self.marked = None;
    }
    fn write_secret(&mut self, output: &mut impl Write) -> bool {
        let result = output
            .write_all(self.value.as_bytes())
            .and_then(|_| output.write_all(b"\n"))
            .and_then(|_| output.flush())
            .is_ok();
        self.clear();
        result
    }
    fn finish(&mut self, submit: bool, cx: &mut Context<Self>) {
        let success = submit && self.write_secret(&mut std::io::stdout().lock());
        self.clear();
        self.outcome
            .store(if success { 0 } else { 1 }, Ordering::Release);
        crate::lifecycle::finish(cx);
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        match key.key.as_str() {
            "enter" => self.finish(true, cx),
            "escape" => self.finish(false, cx),
            "a" if key.modifiers.platform || key.modifiers.control => {
                self.selection = 0..self.value.len();
            }
            "backspace" => {
                let mut range = self.selection.clone();
                if range.is_empty() {
                    range.start = self.value[..range.start]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i);
                }
                self.replace(range, "");
                self.marked = None;
            }
            "delete" => {
                let mut range = self.selection.clone();
                if range.is_empty() {
                    range.end += self.value[range.end..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8);
                }
                self.replace(range, "");
                self.marked = None;
            }
            "left" => {
                let at = self.value[..self.selection.start]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i);
                self.selection = at..at;
            }
            "right" => {
                let at = self.selection.end
                    + self.value[self.selection.end..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8);
                self.selection = at..at;
            }
            "home" => self.selection = 0..0,
            "end" => self.selection = self.value.len()..self.value.len(),
            _ => return,
        }
        window.notify_text_input_changed(&self.focus);
        cx.stop_propagation();
        cx.notify();
    }
}
impl Focusable for Secret {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EntityInputHandler for Secret {
    // Returning plaintext here would allow native text services to retain it.
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.utf16_offset(self.selection.start)..self.utf16_offset(self.selection.end),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|range| self.utf16_offset(range.start)..self.utf16_offset(range.end))
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }
    fn paste(&mut self, _: ClipboardItem, _: &mut Window, _: &mut Context<Self>) {}
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| self.byte_range(range))
            .or_else(|| self.marked.clone())
            .unwrap_or(self.selection.clone());
        if self.replace(range, text) {
            self.marked = None;
            window.notify_text_input_changed(&self.focus);
            cx.notify();
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| self.byte_range(range))
            .or_else(|| self.marked.clone())
            .unwrap_or(self.selection.clone());
        let start = range.start;
        if self.replace(range, text) {
            self.marked = (!text.is_empty()).then_some(start..start + text.len());
            if let Some(selected) = selected {
                let base = self.utf16_offset(start);
                self.selection = self.byte_range(base + selected.start..base + selected.end);
            }
            window.notify_text_input_changed(&self.focus);
            cx.notify();
        }
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.value.encode_utf16().count())
    }
    fn text_input_configuration(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> TextInputConfiguration {
        TextInputConfiguration::default()
    }
    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.value.encode_utf16().count())
    }
    fn set_selected_text_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = self.byte_range(range);
        window.notify_text_selection_changed(&self.focus);
        cx.notify();
    }
}
impl Render for Secret {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let secret = cx.entity();
        let focus = self.focus.clone();
        div()
            .size_full()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .key_context("AstraSecret")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .child(div().text_lg().child(t("Administrator password")))
            .child(
                div()
                    .id("secret-input")
                    .relative()
                    .h(px(38.))
                    .w_full()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .px_3()
                    .py_2()
                    .child("•".repeat(self.value.chars().count().min(48)))
                    .on_click(cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, cx| {
                                window.handle_input(
                                    &focus,
                                    ElementInputHandler::new(bounds, secret),
                                    cx,
                                )
                            },
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-password")
                            .label(t("Cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.finish(false, cx))),
                    )
                    .child(
                        Button::new("submit-password")
                            .primary()
                            .label(t("Confirm"))
                            .on_click(cx.listener(|this, _, _, cx| this.finish(true, cx))),
                    ),
            )
    }
}

#[cfg(test)]
#[path = "test/askpass_test.rs"]
mod tests;
