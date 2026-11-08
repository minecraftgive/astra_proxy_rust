//! Cumulative traffic presentation shared with the overview.
use crate::state::Client;
use astra_model::runtime::NodeTraffic;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Period {
    #[default]
    Session,
    Hour,
    Day,
    Week,
    Month,
    Persist,
}
impl Period {
    pub fn next(self) -> Self {
        match self {
            Self::Session => Self::Hour,
            Self::Hour => Self::Day,
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Persist,
            Self::Persist => Self::Session,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "this session",
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "1mon",
            Self::Persist => "persist",
        }
    }
    pub fn bytes(self, value: NodeTraffic) -> u64 {
        match self {
            Self::Session => value.session,
            Self::Hour => value.hour,
            Self::Day => value.day,
            Self::Week => value.week,
            Self::Month => value.month,
            Self::Persist => value.persist,
        }
    }
}
impl Client {
    /// Session means the current core/host lifetime, including drained workers.
    pub fn session_traffic(&self) -> Option<(u64, u64)> {
        let snapshot = self.live()?;
        let proxy = snapshot
            .proxy_upload
            .saturating_add(snapshot.proxy_download);
        let direct = snapshot
            .upload
            .saturating_sub(snapshot.proxy_upload)
            .saturating_add(snapshot.download.saturating_sub(snapshot.proxy_download));
        Some((proxy, direct))
    }

    pub fn node_traffic(&self) -> Option<u64> {
        let snapshot = self.live()?;
        let telemetry = self.runtime_telemetry.as_ref()?.traffic.as_ref()?;
        if self.traffic_period != Period::Session && telemetry.history_error.is_some() {
            return None;
        }
        let nodes = telemetry.nodes.as_ref()?;
        let node = telemetry
            .selected_node
            .clone()
            .or_else(|| crate::live::exit_chain(snapshot).0.pop())?;
        let traffic = nodes.get(&node).or_else(|| {
            // A standalone single-outbound configuration has no PROXY group.
            (snapshot.groups.is_empty() && node == "PROXY" && nodes.len() == 1)
                .then(|| nodes.values().next())
                .flatten()
        })?;
        Some(self.traffic_period.bytes(*traffic))
    }
}
