//! Data and semantic tokens for the local and selected manager information panel.
use crate::{
    highlight::{Token, TokenKind, TokenStyle},
    i18n::t,
    settings::Field,
    state::Client,
};
use astra_platform::program_info::{BuildInfo, FeatureSupport, ProgramInfo};

fn muted(text: impl Into<String>) -> Token {
    Token::styled(text, TokenStyle::default().kind(TokenKind::Comment))
}
fn heading(title: &str) -> Vec<Token> {
    vec![Token::styled(
        format!(" {}", t(title)),
        TokenStyle::default().kind(TokenKind::Comment).strong(),
    )]
}
fn value(label: &str, text: impl Into<String>) -> Vec<Token> {
    let text: String = text.into().chars().filter(|c| !c.is_control()).collect();
    vec![muted(format!(" {}: ", t(label))), Token::raw(text)]
}
fn indicator(label: &str, state: Option<bool>, support: bool) -> Token {
    let (mark, status, kind) = match (state, support) {
        (Some(true), true) => ("●", "Supported", TokenKind::Direct),
        (Some(false), true) => ("○", "Not supported", TokenKind::Comment),
        (Some(true), false) => ("●", "On", TokenKind::Direct),
        (Some(false), false) => ("○", "Off", TokenKind::Comment),
        (None, _) => ("?", "Unknown", TokenKind::Number),
    };
    Token::styled(
        format!(" {mark} {} · {}  ", t(label), t(status)),
        TokenStyle::default().kind(kind),
    )
}
fn build_lines(lines: &mut Vec<Vec<Token>>, label: &str, build: &BuildInfo) {
    lines.push(value(
        label,
        format!("{} {} · {}", build.name, build.version, build.profile),
    ));
    lines.push(value(
        "Compiled at",
        format!("{} · {}", build.compiled_at, build.target),
    ));
    lines.push(value(
        "Toolchain",
        format!(
            "{} · Git {} · {}: {}",
            build.rustc,
            build.git_revision.as_deref().unwrap_or(t("Unknown")),
            t("Debug assertions"),
            t(if build.debug_assertions { "On" } else { "Off" }),
        ),
    ));
}

fn system_line(label: &str, info: &ProgramInfo) -> Vec<Token> {
    let system = &info.system;
    value(
        label,
        format!(
            "{} {} · {} · CPU {} · PID {} · {}: {} · {}",
            system.os,
            system.os_version.as_deref().unwrap_or(t("Unknown")),
            system.architecture,
            system
                .logical_cpus
                .map(|n| n.to_string())
                .unwrap_or_else(|| t("Unknown").into()),
            system.process_id,
            t("Elevated"),
            t(match system.elevated {
                Some(true) => "Yes",
                Some(false) => "No",
                None => "Unknown",
            }),
            system.ipc_transport,
        ),
    )
}

fn features(lines: &mut Vec<Vec<Token>>, title: &str, features: &[FeatureSupport]) {
    lines.push(heading(title));
    if features.is_empty() {
        lines.push(value("Status", t("Unknown")));
    } else {
        lines.push(
            features
                .iter()
                .map(|f| indicator(&f.label, Some(f.supported), true))
                .collect::<Vec<_>>(),
        );
    }
}

impl Client {
    /// `terminal_colors` is omitted for graphical frontends; `Some(None)` means a
    /// terminal which reports no colors. The caller supplies its own build identity.
    pub fn settings_program_info(
        &self,
        local_build: &BuildInfo,
        terminal_colors: Option<Option<u16>>,
    ) -> Vec<Vec<Token>> {
        let mut lines = vec![heading("Program information")];
        build_lines(&mut lines, "Local interface", local_build);
        if let Some(local) = &self.program_info.local {
            lines.push(system_line("Local system", local));
        } else {
            lines.push(value(
                "Local system",
                self.program_info
                    .local_error
                    .as_deref()
                    .unwrap_or(t("Loading…")),
            ));
        }
        if terminal_colors.is_some() {
            let colors = match terminal_colors.flatten() {
                None => t("none").to_owned(),
                Some(u16::MAX) => "24-bit".into(),
                Some(count) => count.to_string(),
            };
            lines.push(value(
                "Terminal",
                format!(
                    "{}: {} · {}: {} · {}: {}",
                    t("Colors"),
                    colors,
                    t("Mouse"),
                    t(if self.preferences.mouse { "On" } else { "Off" }),
                    t("Motion"),
                    Field::Motion.choice_label(Field::Motion.get(&self.preferences)),
                ),
            ));
        }
        if let Some(info) = &self.program_info.data {
            build_lines(&mut lines, "Management node", &info.build);
            lines.push(system_line("Node system", info));
            if let Some(path) = &info.system.executable {
                lines.push(value("Node executable", path));
            }
            features(&mut lines, "Software features", &info.features);
            features(&mut lines, "Platform support", &info.platform_features);
            lines.push(vec![muted(format!(
                " {}",
                t("Support does not imply enabled; permissions and configuration still apply.")
            ))]);
        }
        if self.program_info.loading {
            lines.push(value("Management node", t("Loading…")));
        } else if let Some(error) = &self.program_info.error {
            lines.push(value("Information unavailable", error));
            if self.program_info.data.is_some() {
                lines.push(value("Status", t("Cached information · refresh failed")));
            }
        } else if self.program_info.data.is_none() {
            lines.push(value("Management node", t("Information unavailable")));
        }
        lines.push(heading("Current instance"));
        if let Some(row) = self.active_proxy() {
            lines.push(value(
                "Profile",
                format!("{} · {}", row.instance.name, self.runtime_health()),
            ));
            let live = self.live().filter(|_| self.live_rates_available());
            lines.push(vec![
                indicator("System proxy", live.map(|s| s.system_proxy), false),
                indicator("TUN", live.map(|s| s.tun), false),
                indicator("Allow LAN", live.map(|s| s.allow_lan), false),
            ]);
        } else {
            lines.push(value("Status", t("No active proxy instance")));
        }
        lines
    }
}
