//! Stable destination groups for the connection list; runtime connections stay unchanged.
use astra_model::runtime::Connection;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub(super) struct ConnectionTarget {
    pub host: String,
    pub port: u16,
}

impl ConnectionTarget {
    pub fn of(connection: &Connection) -> Self {
        Self {
            host: connection.host.to_ascii_lowercase(),
            port: connection.port,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ConnectionRowKey {
    Target(ConnectionTarget),
    Connection(u64),
}

#[derive(Clone, Debug)]
pub(super) struct ConnectionRow {
    pub key: ConnectionRowKey,
    /// A group carries summed counters here. Its representative ID is never an action target.
    pub connection: Connection,
    pub count: usize,
    pub expanded: bool,
    pub parent: Option<ConnectionTarget>,
    pub child: bool,
    pub fresh: bool,
}

impl ConnectionRow {
    pub fn is_group(&self) -> bool {
        matches!(self.key, ConnectionRowKey::Target(_))
    }

    pub fn label(&self) -> String {
        let target = format!("{}:{}", self.connection.host, self.connection.port);
        if self.is_group() {
            format!(
                "{} {target} ({})",
                if self.expanded { "▾" } else { "▸" },
                self.count,
            )
        } else if self.child {
            format!("  {target}")
        } else {
            target
        }
    }
}

/// Groups are ranked by aggregate traffic; a newly arrived child temporarily pins its group.
/// Filtering happens before this function, so counters describe the displayed connections.
pub(super) fn build_rows(
    connections: Vec<&Connection>,
    expanded: &HashSet<ConnectionTarget>,
    fresh: &HashSet<u64>,
) -> Vec<ConnectionRow> {
    let mut groups: HashMap<ConnectionTarget, Vec<&Connection>> = HashMap::new();
    for connection in connections {
        groups
            .entry(ConnectionTarget::of(connection))
            .or_default()
            .push(connection);
    }
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(target, mut connections)| {
            connections.sort_by_key(|c| {
                (
                    std::cmp::Reverse(fresh.contains(&c.id)),
                    std::cmp::Reverse(c.upload.saturating_add(c.download)),
                    c.id,
                )
            });
            let mut summary = connections[0].clone();
            summary.host = target.host.clone();
            summary.upload = 0;
            summary.download = 0;
            summary.elapsed_ms = 0;
            for connection in &connections {
                summary.upload = summary.upload.saturating_add(connection.upload);
                summary.download = summary.download.saturating_add(connection.download);
                summary.elapsed_ms = summary.elapsed_ms.max(connection.elapsed_ms);
            }
            if connections.iter().any(|c| c.outbound != summary.outbound) {
                summary.outbound = crate::i18n::t("Mixed").to_owned();
            }
            if connections.iter().any(|c| c.network != summary.network) {
                summary.network = crate::i18n::t("Mixed").to_owned();
            }
            if connections.iter().any(|c| c.inbound != summary.inbound) {
                summary.inbound = crate::i18n::t("Mixed").to_owned();
            }
            summary.source = None;
            let fresh = connections.iter().any(|c| fresh.contains(&c.id));
            let first_id = connections.iter().map(|c| c.id).min().unwrap();
            (target, summary, connections, fresh, first_id)
        })
        .collect();
    groups.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| {
                b.1.upload
                    .saturating_add(b.1.download)
                    .cmp(&a.1.upload.saturating_add(a.1.download))
            })
            .then_with(|| a.4.cmp(&b.4))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut rows = Vec::new();
    for (target, summary, connections, group_fresh, _) in groups {
        let grouped = connections.len() > 1;
        if grouped {
            let is_expanded = expanded.contains(&target);
            rows.push(ConnectionRow {
                key: ConnectionRowKey::Target(target.clone()),
                connection: summary,
                count: connections.len(),
                expanded: is_expanded,
                parent: None,
                child: false,
                fresh: group_fresh,
            });
            if !is_expanded {
                continue;
            }
        }
        rows.extend(connections.into_iter().map(|connection| ConnectionRow {
            key: ConnectionRowKey::Connection(connection.id),
            connection: connection.clone(),
            count: 1,
            expanded: false,
            parent: Some(target.clone()),
            child: grouped,
            fresh: fresh.contains(&connection.id),
        }));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(id: u64, host: &str, upload: u64, download: u64) -> Connection {
        Connection {
            id,
            source: Some(format!("127.0.0.1:{}", 4000 + id)),
            host: host.into(),
            port: 443,
            network: "Tcp".into(),
            inbound: "Socks5".into(),
            outbound: "Tokyo".into(),
            upload,
            download,
            elapsed_ms: id * 1000,
        }
    }

    #[test]
    fn groups_only_same_host_and_port_and_sums_both_counters() {
        let mut other_port = connection(4, "example.test", 1, 2);
        other_port.port = 8443;
        let connections = [
            connection(1, "Example.Test", 10, 20),
            connection(2, "example.test", 30, 40),
            connection(3, "other.test", 2, 3),
            other_port,
        ];
        let rows = build_rows(
            connections.iter().collect(),
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(rows.len(), 3);
        let group = &rows[0];
        assert_eq!(group.count, 2);
        assert!(!group.expanded);
        assert_eq!(group.connection.upload, 40);
        assert_eq!(group.connection.download, 60);
        assert_eq!(group.connection.elapsed_ms, 2000);
        assert_eq!(group.connection.source, None);
        assert_eq!(group.label(), "▸ example.test:443 (2)");
        assert_eq!(rows[1].key, ConnectionRowKey::Connection(3));
        assert_eq!(rows[1].label(), "other.test:443");
        assert_eq!(rows[2].key, ConnectionRowKey::Connection(4));
    }

    #[test]
    fn expanded_groups_keep_individual_ids_and_sort_children_deterministically() {
        let connections = [
            connection(3, "example.test", 20, 10),
            connection(2, "example.test", 30, 40),
            connection(1, "example.test", 20, 10),
        ];
        let target = ConnectionTarget::of(&connections[0]);
        let rows = build_rows(
            connections.iter().collect(),
            &HashSet::from([target.clone()]),
            &HashSet::new(),
        );
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].key, ConnectionRowKey::Target(target.clone()));
        assert_eq!(rows[0].label(), "▾ example.test:443 (3)");
        assert_eq!(rows[1].key, ConnectionRowKey::Connection(2));
        assert_eq!(rows[2].key, ConnectionRowKey::Connection(1));
        assert_eq!(rows[3].key, ConnectionRowKey::Connection(3));
        for row in &rows[1..] {
            assert_eq!(row.parent.as_ref(), Some(&target));
            assert!(row.child);
            assert_eq!(row.label(), "  example.test:443");
        }
    }

    #[test]
    fn summaries_use_total_traffic_and_stable_id_ties() {
        let connections = [
            connection(1, "b.test", 0, 6),
            connection(2, "a.test", 0, 6),
            connection(3, "a.test", 0, 6),
            connection(4, "b.test", 0, 6),
            connection(5, "c.test", 0, 10),
        ];
        let rows = build_rows(
            connections.iter().collect(),
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.connection.host.as_str())
                .collect::<Vec<_>>(),
            ["b.test", "a.test", "c.test"]
        );
    }

    #[test]
    fn mixed_routes_do_not_show_a_single_representative_route() {
        let first = connection(1, "example.test", 1, 2);
        let mut second = connection(2, "example.test", 3, 4);
        second.outbound = "DIRECT".into();
        second.network = "Udp".into();
        second.inbound = "Tun".into();
        let rows = build_rows(vec![&first, &second], &HashSet::new(), &HashSet::new());
        assert_eq!(rows[0].connection.outbound, crate::i18n::t("Mixed"));
        assert_eq!(rows[0].connection.network, crate::i18n::t("Mixed"));
        assert_eq!(rows[0].connection.inbound, crate::i18n::t("Mixed"));
    }

    #[test]
    fn aggregate_counters_saturate_and_empty_lists_stay_empty() {
        let first = connection(1, "example.test", u64::MAX, u64::MAX - 1);
        let second = connection(2, "example.test", 2, 4);
        let rows = build_rows(vec![&first, &second], &HashSet::new(), &HashSet::new());
        assert_eq!(rows[0].connection.upload, u64::MAX);
        assert_eq!(rows[0].connection.download, u64::MAX);
        assert!(build_rows(Vec::new(), &HashSet::new(), &HashSet::new()).is_empty());
    }

    #[test]
    fn fresh_connections_pin_groups_and_children_until_freshness_expires() {
        let connections = [
            connection(1, "busy.test", 100, 100),
            connection(2, "fresh.test", 10, 20),
            connection(3, "fresh.test", 0, 0),
        ];
        let expanded = HashSet::from([ConnectionTarget::of(&connections[1])]);
        let rows = build_rows(connections.iter().collect(), &expanded, &HashSet::from([3]));
        assert_eq!(rows[0].connection.host, "fresh.test");
        assert!(rows[0].fresh);
        assert_eq!(rows[1].key, ConnectionRowKey::Connection(3));
        assert!(rows[1].fresh);
        assert_eq!(rows[2].key, ConnectionRowKey::Connection(2));
        assert!(!rows[2].fresh);
        assert_eq!(rows[3].key, ConnectionRowKey::Connection(1));
        assert!(!rows[3].fresh);

        let expired = build_rows(connections.iter().collect(), &expanded, &HashSet::new());
        assert_eq!(expired[0].key, ConnectionRowKey::Connection(1));
        assert_eq!(expired[2].key, ConnectionRowKey::Connection(2));
        assert_eq!(expired[3].key, ConnectionRowKey::Connection(3));
        assert!(expired.iter().all(|row| !row.fresh));
    }
}
