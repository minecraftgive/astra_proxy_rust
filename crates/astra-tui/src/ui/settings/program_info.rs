//! Read-only information about this interface and its selected management node.
use super::*;
use astra_platform::program_info::{BuildInfo, FeatureSupport, ProgramInfo};

fn heading(title: &str) -> Line<'static> {
    Line::styled(
        format!(" {}", t(title)),
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    )
}

fn value(label: &str, text: impl Into<String>) -> Line<'static> {
    // Paths and host-provided strings may contain terminal control characters.
    let text: String = text.into().chars().filter(|c| !c.is_control()).collect();
    Line::from(vec![
        Span::styled(format!(" {}: ", t(label)), Style::default().fg(MUTED)),
        Span::raw(text),
    ])
}

fn indicator(label: &str, state: Option<bool>, support: bool) -> Span<'static> {
    let (mark, status, color) = match (state, support) {
        (Some(true), true) => ("●", "Supported", Color::Green),
        (Some(false), true) => ("○", "Not supported", MUTED),
        (Some(true), false) => ("●", "On", Color::Green),
        (Some(false), false) => ("○", "Off", MUTED),
        (None, _) => ("?", "Unknown", Color::Yellow),
    };
    Span::styled(
        format!(" {mark} {} · {}  ", t(label), t(status)),
        Style::default().fg(color),
    )
}

fn build_lines(lines: &mut Vec<Line<'static>>, label: &str, build: &BuildInfo) {
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

fn system_line(label: &str, info: &ProgramInfo) -> Line<'static> {
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

fn features(lines: &mut Vec<Line<'static>>, title: &str, features: &[FeatureSupport]) {
    lines.push(heading(title));
    if features.is_empty() {
        lines.push(value("Status", t("Unknown")));
    } else {
        lines.push(Line::from(
            features
                .iter()
                .map(|f| indicator(&f.label, Some(f.supported), true))
                .collect::<Vec<_>>(),
        ));
    }
}

/// Wrap at grapheme boundaries using terminal cell widths. Keeping the physical
/// lines lets the same viewport scroll information and clickable preferences.
pub(super) fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut output = Vec::new();
    for line in lines {
        let mut spans = Vec::new();
        let mut used = 0;
        for grapheme in line.styled_graphemes(Style::default()) {
            let cells = astra_tui_kit::text::width(grapheme.symbol);
            if used > 0 && used + cells > width {
                output.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            spans.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            used += cells;
        }
        output.push(Line::from(spans));
    }
    output
}

impl Ui {
    pub(super) fn settings_program_info(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![heading("Program information")];
        let local_build = astra_platform::build_info!();
        build_lines(&mut lines, "Local interface", &local_build);
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
        let colors = match self.animations.detected_colors() {
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
        if let Some(info) = &self.program_info.data {
            build_lines(&mut lines, "Management node", &info.build);
            lines.push(system_line("Node system", info));
            if let Some(path) = &info.system.executable {
                lines.push(value("Node executable", path));
            }
            features(&mut lines, "Software features", &info.features);
            features(&mut lines, "Platform support", &info.platform_features);
            lines.push(Line::styled(
                format!(" {}", t("Support does not imply enabled; permissions and configuration still apply.")),
                Style::default().fg(MUTED),
            ));
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
            let live = self.live().filter(|_| self.runtime_rates_available());
            lines.push(Line::from(vec![
                indicator("System proxy", live.map(|s| s.system_proxy), false),
                indicator("TUN", live.map(|s| s.tun), false),
                indicator("Allow LAN", live.map(|s| s.allow_lan), false),
            ]));
        } else {
            lines.push(value("Status", t("No active proxy instance")));
        }
        wrap(lines, width)
    }
}
