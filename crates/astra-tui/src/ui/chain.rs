//! Real proxy hops, distinct from the group selection hierarchy.
use super::*;
use astra_model::runtime::ChainPath;

#[derive(Clone)]
pub(super) enum Source {
    Connection(u64),
    Group(String),
    Destination,
}

impl Ui {
    pub(super) fn render_chain(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let Some(snapshot) = self.live() else {
            self.chain_focus.borrow_mut().take();
            f.render_widget(Paragraph::new(t("Proxy path unavailable")), area);
            return;
        };
        let default = || {
            Source::Group(
                if snapshot.mode == astra_model::runtime::Mode::Direct {
                    "DIRECT"
                } else {
                    "PROXY"
                }
                .into(),
            )
        };
        let source = match self.focus.region {
            focus::Region::Secondary => Some(
                self.selected_proxy_group()
                    .map(|group| Source::Group(group.name.clone()))
                    .unwrap_or_else(default),
            ),
            focus::Region::Main => self.selected_connection_row_key().map(|key| match key {
                connections::ConnectionRowKey::Connection(id) => Source::Connection(id),
                connections::ConnectionRowKey::Target(_) => Source::Destination,
            }),
            _ => None,
        };
        let mut saved = self.chain_focus.borrow_mut();
        if saved.as_ref().is_none_or(|(instance, pid, _)| {
            instance != &self.runtime_instance || *pid != snapshot.pid
        }) {
            *saved = Some((self.runtime_instance.clone(), snapshot.pid, default()));
        }
        if let Some(source) = source {
            saved.as_mut().unwrap().2 = source;
        }
        let source = &saved.as_ref().unwrap().2;
        let telemetry = self.runtime_telemetry.as_ref();
        let chains = telemetry.and_then(|meta| meta.chains.as_deref());
        let (label, path, detail) = match source {
            Source::Connection(id) => {
                let connection = snapshot
                    .connections
                    .iter()
                    .find(|connection| connection.id == *id);
                (
                    t("Actual connection"),
                    connection.and_then(|_| chains?.connections.get(id)),
                    connection
                        .map(|c| format!("{}:{} · {}", c.host, c.port, c.network))
                        .unwrap_or_else(|| t("Connection ended").into()),
                )
            }
            Source::Group(name) => (
                t("Path preview"),
                chains.and_then(|chains| chains.previews.get(name)),
                name.clone(),
            ),
            Source::Destination => (
                t("Actual connection"),
                None,
                t("Select a connection to view its path").into(),
            ),
        };
        let stale = !self.runtime_error.is_empty()
            || self
                .runtime_received
                .is_none_or(|at| at.elapsed() > Duration::from_secs(3))
            || telemetry.is_some_and(|meta| {
                meta.coverage != astra_model::runtime::Coverage::Complete
                    || meta.sample_age_ms > 3000
            });
        let mut lines = vec![format!(
            "{} · {label}{}",
            t("Proxy path"),
            if stale { " ~" } else { "" }
        )];
        if area.height > 2 {
            lines.push(detail);
        }
        if area.height >= 10 {
            if let Some(path) = path.filter(|path| !path.groups.is_empty()) {
                lines.push(format!(
                    "{}: {}",
                    t("Proxy groups"),
                    path.groups.join(" / ")
                ));
            }
        }
        let budget = area.height.saturating_sub(lines.len() as u16) as usize;
        if let Some(path) = path {
            if budget == 1 {
                lines.push(compact_path(path, area.width as usize));
            } else {
                lines.extend(path_lines(path, budget));
            }
        } else if budget > 0 {
            lines.push(t("Proxy path unavailable").into());
        }
        if area.height == 1 {
            lines = vec![path
                .map(|path| {
                    format!(
                        "{}: {}",
                        t("Proxy path"),
                        compact_path(path, area.width.saturating_sub(12) as usize)
                    )
                })
                .unwrap_or_else(|| t("Proxy path unavailable").into())];
        }
        let lines: Vec<Line> = lines
            .into_iter()
            .map(|line| {
                Line::from(astra_tui_kit::text::truncate_cells(
                    &line,
                    area.width as usize,
                    "…",
                ))
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().fg(if stale {
                dashboard::MUTED
            } else {
                dashboard::BLUE
            })),
            area,
        );
    }
}

fn compact_path(path: &ChainPath, width: usize) -> String {
    if path.hops.len() < 2 || path.issue.is_some() || path.status == "blocked" {
        return path_lines(path, 1).join("");
    }
    let middle = if path.hops.len() > 2 {
        format!("{} relay → ", path.hops.len() - 2)
    } else {
        String::new()
    };
    let name_width = width.saturating_sub(17 + middle.len()) / 2;
    let first = astra_tui_kit::text::truncate_cells(&path.hops[0].name, name_width, "…");
    let last =
        astra_tui_kit::text::truncate_cells(&path.hops.last().unwrap().name, name_width, "…");
    format!("entry {first} → {middle}exit {last}")
}

fn path_lines(path: &ChainPath, height: usize) -> Vec<String> {
    if height == 0 {
        return vec![];
    }
    if path.status == "blocked" {
        return vec!["REJECT ⊘".into()];
    }
    if let Some(issue) = &path.issue {
        return vec![issue.clone()];
    }
    if path.hops.is_empty() {
        return vec!["DIRECT".into()];
    }
    let node = |i: usize| {
        let role = if path.hops.len() == 1 {
            "entry/exit"
        } else if i == 0 {
            "entry"
        } else if i + 1 == path.hops.len() {
            "exit"
        } else {
            "relay"
        };
        format!("{role}  {} · {}", path.hops[i].name, path.hops[i].protocol)
    };
    if height < 3 {
        return vec![if path.hops.len() <= 2 {
            (0..path.hops.len())
                .map(node)
                .collect::<Vec<_>>()
                .join(" → ")
        } else {
            format!(
                "{} → …{} relay → {}",
                node(0),
                path.hops.len() - 2,
                node(path.hops.len() - 1)
            )
        }];
    }
    let count = ((height + 1) / 2).min(path.hops.len());
    if count < path.hops.len() {
        let mut lines = vec![node(0)];
        for i in 1..count.saturating_sub(1) {
            lines.push("  ↓".into());
            lines.push(node(i));
        }
        lines.push(format!("  ↓ … {} relay", path.hops.len() - count));
        lines.push(node(path.hops.len() - 1));
        lines
    } else {
        (0..path.hops.len())
            .flat_map(|i| {
                if i == 0 {
                    vec![node(i)]
                } else {
                    vec!["  ↓".into(), node(i)]
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_hops_have_roles_and_long_chains_keep_both_ends() {
        let path = ChainPath {
            status: "proxy".into(),
            hops: (0..4)
                .map(|i| astra_model::runtime::ChainHop {
                    name: format!("node{i}"),
                    protocol: "SOCKS5".into(),
                })
                .collect(),
            ..Default::default()
        };
        let full = path_lines(&path, 7).join("\n");
        assert!(full.contains("entry  node0"));
        assert!(full.contains("relay  node1"));
        assert!(full.contains("relay  node2"));
        assert!(full.contains("exit  node3"));
        let short = path_lines(&path, 3);
        assert_eq!(short.len(), 3);
        assert!(short[1].contains("2 relay"));
        assert!(short[2].contains("exit  node3"));
        assert!(path_lines(&path, 0).is_empty());
    }
}
