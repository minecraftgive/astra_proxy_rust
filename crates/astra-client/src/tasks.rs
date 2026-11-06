//! Frontend-independent task history and program information.
use crate::i18n::t;
use astra_ipc::progress::{self, Progress};
use std::time::Instant;
/// Cached local and selected-manager metadata. Read-only requests never enter
/// the operation queue, and failures retain the last known manager data.
pub struct ProgramInformation {
    pub local: Option<astra_management::v2::ProgramInfo>,
    pub data: Option<astra_management::v2::ProgramInfo>,
    pub loading: bool,
    pub error: Option<String>,
    pub local_error: Option<String>,
    pub refresh: bool,
}
impl Default for ProgramInformation {
    fn default() -> Self {
        Self {
            local: None,
            data: None,
            loading: false,
            error: None,
            local_error: None,
            refresh: true,
        }
    }
}

#[derive(Default)]
pub struct Tasks {
    pub records: std::collections::VecDeque<Record>,
    pub details: Option<u16>,
    pub notice: Option<(String, u64)>,
    /// How the last background task ended, keyed by the message it left behind.
    pub verdict: Option<(String, bool)>,
    pub update_error: Option<String>,
}
pub struct Record {
    pub id: u64,
    pub title: String,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub events: std::collections::VecDeque<String>,
}
impl Tasks {
    pub fn running(&self) -> bool {
        self.records.iter().any(|r| r.finished.is_none())
    }
    pub fn notice_is_old(&self) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|(_, ms)| progress::now_ms().saturating_sub(*ms) > 6000)
    }
    pub fn showing_details(&self) -> bool {
        self.details.is_some()
    }
    pub fn notice_message(&mut self, message: &str) {
        self.notice = Some((message.into(), progress::now_ms()));
    }
    pub fn notice_result(&mut self, message: &str, failed: bool) {
        self.notice_message(message);
        self.verdict = Some((message.into(), failed));
    }
    pub fn event(&mut self, id: u64, event: Progress) {
        if let Some(record) = self.records.iter_mut().find(|r| r.id == id) {
            let count = match (event.current, event.total) {
                (Some(c), Some(t)) => format!(" [{c}/{t}]"),
                _ => String::new(),
            };
            let detail: String = event
                .detail
                .replace(['\n', '\r'], " ")
                .chars()
                .take(4096)
                .collect();
            record.events.push_back(format!(
                "{} {}{count} · {detail}",
                crate::tui_log::stamp(event.timestamp_ms),
                t(&event.phase)
            ));
            if record.events.len() > 128 {
                record.events.pop_front();
            }
        }
    }
}
