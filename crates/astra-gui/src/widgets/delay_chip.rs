use crate::theme::palette;
use gpui::{div, App, IntoElement, ParentElement, RenderOnce, Styled, Window};

/// None is a timeout or missing measurement. The caller owns measurement state.
#[derive(IntoElement)]
pub struct DelayChip {
    milliseconds: Option<u64>,
    failed: bool,
}
impl DelayChip {
    pub fn new(milliseconds: Option<u64>) -> Self {
        Self {
            milliseconds,
            failed: false,
        }
    }
    /// The measurement ran and failed: shown as a red Timeout, not a placeholder.
    pub fn failed() -> Self {
        Self {
            milliseconds: None,
            failed: true,
        }
    }
}
impl RenderOnce for DelayChip {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        // The web UI's `.lat.g/.y/.r/.n` latency colors, in a soft pill.
        let (fg, bg) = match self.milliseconds {
            _ if self.failed => (p.err, p.err_soft),
            Some(0..=99) => (p.ok, p.ok_soft),
            Some(100..=299) => (p.warn, p.warn_soft),
            Some(_) => (p.err, p.err_soft),
            None => (p.faint, p.surface_2),
        };
        let text = if self.failed {
            astra_client::i18n::t("Timeout").to_owned()
        } else {
            self.milliseconds
                .map(|value| astra_client::tr!("{} ms", value))
                .unwrap_or_else(|| "—".into())
        };
        div()
            .flex_none()
            .px_2()
            .rounded_full()
            .text_xs()
            .font_family(crate::theme::mono(cx))
            .whitespace_nowrap()
            .bg(bg)
            .text_color(fg)
            .child(text)
    }
}

/// The header's exit probe fields; each is present only while its setting is on.
#[derive(IntoElement)]
pub struct ExitProbe {
    ip: Option<String>,
    latency: Option<astra_client::overview::ExitLatency>,
}
impl ExitProbe {
    pub fn new(header: &astra_client::overview::Header) -> Self {
        Self {
            ip: header.exit_ip.clone(),
            latency: header.exit_latency,
        }
    }
}
impl RenderOnce for ExitProbe {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        use astra_client::overview::ExitLatency;
        let p = palette(cx);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .children(self.ip.map(|ip| {
                div()
                    .text_xs()
                    .font_family(crate::theme::mono(cx))
                    .whitespace_nowrap()
                    .text_color(p.muted)
                    .child(format!("ip: {ip}"))
            }))
            .children(self.latency.map(|latency| match latency {
                ExitLatency::Measured(ms) => DelayChip::new(Some(ms)),
                ExitLatency::Failed => DelayChip::failed(),
                ExitLatency::Pending => DelayChip::new(None),
            }))
    }
}
