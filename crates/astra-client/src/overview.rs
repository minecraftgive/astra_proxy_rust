//! Semantic overview state and expiring notices shared by every frontend.
use crate::{
    i18n::t,
    protocol::{Kind, Row},
    state::Client,
    tr,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Success,
    Warning,
    Error,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceState {
    Running,
    Stopped,
    Starting,
    Error,
}
pub fn indicator_state(row: &Row) -> InstanceState {
    if row.state == "Starting" || row.state == "Stopping" {
        InstanceState::Starting
    } else if row.state.starts_with("Unresponsive")
        || row.state.starts_with("Exited")
        || row.state.starts_with("Failed")
    {
        InstanceState::Error
    } else if row.pid.is_some() && row.state == "Running" {
        InstanceState::Running
    } else {
        InstanceState::Stopped
    }
}
impl Client {
    pub fn notices(&self) -> Vec<(Severity, String)> {
        self.notice_expiry.set(None);
        let mut notices = Vec::new();
        let Some(row) = self.active_proxy() else {
            return notices;
        };
        if let Some(notice) = self
            .details
            .get(&row.instance.name)
            .and_then(|d| d.network_recovery.as_ref())
        {
            use astra_model::recovery::RecoveryState;
            let (label, color) = match notice.state {
                RecoveryState::Recovered => ("Network settings recovered", Severity::Success),
                RecoveryState::Pending => ("Network recovery pending", Severity::Error),
                RecoveryState::Conflict => ("Network recovery conflict", Severity::Error),
            };
            let time = i64::try_from(notice.timestamp_ms)
                .ok()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|date| {
                    date.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S %:z")
                        .to_string()
                })
                .unwrap_or_else(|| "?".into());
            notices.push((color, format!("{} · {time}", t(label))));
            notices.push((color, notice.message.clone()));
            notices.push((
                color,
                format!("{}: {}", t("Recovery record"), notice.journal.display()),
            ));
        }
        if row.state.starts_with("Exited") || row.state == "Unresponsive" {
            notices.push((
                Severity::Error,
                tr!(
                    "Core stopped unexpectedly ({}) · L logs · e edit · c retry",
                    t(&row.state)
                ),
            ));
        }
        if row.changed {
            notices.push((
                Severity::Warning,
                t("Configuration changed but not applied · r apply").into(),
            ));
        }
        if let Some(rule) = self.active_rule() {
            if rule.changed {
                notices.push((
                    Severity::Warning,
                    tr!(
                        "Rule core {} has unapplied changes · reload it in Profiles",
                        rule.instance.name
                    ),
                ));
            }
            if row.pid.is_some() && rule.pid.is_none() {
                notices.push((
                    Severity::Warning,
                    tr!(
                        "Rule core {} is not running; routed connections will fail",
                        rule.instance.name
                    ),
                ));
            }
        }
        if row.pid.is_some() && !self.runtime_error.is_empty() {
            notices.push((Severity::Error, self.runtime_error.clone()));
        }
        if self
            .details
            .get(&row.instance.name)
            .is_some_and(|d| d.config_unavailable)
        {
            notices.push((
                Severity::Error,
                t("Configuration unavailable · e edit").into(),
            ));
        }
        // Successes and warnings recede after 15 seconds; errors stay until fixed.
        let now = Instant::now();
        let mut seen = self.notice_since.borrow_mut();
        seen.retain(|message, _| notices.iter().any(|(_, text)| text == message));
        notices.retain(|(color, message)| {
            if !matches!(color, Severity::Warning | Severity::Success) {
                return true;
            }
            let deadline = *seen.entry(message.clone()).or_insert(now) + Duration::from_secs(15);
            if deadline <= now {
                return false;
            }
            self.notice_expiry.set(Some(
                self.notice_expiry
                    .get()
                    .map_or(deadline, |next| next.min(deadline)),
            ));
            true
        });
        notices
    }

    pub fn next_notice_expiry(&self) -> Option<Instant> {
        if self.page == crate::navigation::OVERVIEW && self.kind != Kind::Server {
            self.notice_expiry.get()
        } else {
            None
        }
    }

    pub fn expire_notices(&self) -> bool {
        if self
            .next_notice_expiry()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            // Keep first-seen times so a later redraw cannot revive expired notices.
            self.notice_expiry.set(None);
            true
        } else {
            false
        }
    }
}
