//! DNS history selection and pause state, independent of connection controls.
use crate::state::Client;
use astra_model::runtime::{DnsQuery, DnsQueryLog};

#[derive(Default)]
pub struct State {
    pub filter: String,
    pub filtering: bool,
    pub selected: Option<(String, u64)>,
    pub frozen: Option<Box<DnsQueryLog>>,
    pub detail: bool,
    pub detail_record: Option<DnsQuery>,
    pub detail_scroll: u16,
    pub query_open: bool,
    pub status_open: bool,
    pub status_scroll: u16,
    pub query_field: usize,
    pub target: String,
    pub record: usize,
    pub query: Option<Box<crate::nettools::View>>,
    pub query_error: Option<String>,
    pub query_scroll: u16,
    pending: Option<PendingQuery>,
}
impl Client {
    pub fn dns_live(&self) -> Option<&DnsQueryLog> {
        self.live()?;
        self.runtime_telemetry.as_ref()?.dns.as_deref()
    }
    pub fn dns_log(&self) -> Option<&DnsQueryLog> {
        self.live()?;
        self.dns_state.frozen.as_deref().or_else(|| self.dns_live())
    }
    pub fn dns_rows(&self) -> Vec<&DnsQuery> {
        let query = self.dns_state.filter.to_lowercase();
        self.dns_log()
            .into_iter()
            .flat_map(|log| &log.records)
            .filter(|row| {
                query.is_empty()
                    || format!(
                        "{} {} {} {} {} {}",
                        row.domain,
                        row.query_type,
                        row.source,
                        row.status,
                        row.upstream.as_deref().unwrap_or(""),
                        row.answers.join(" ")
                    )
                    .to_lowercase()
                    .contains(&query)
                    || row
                        .error
                        .as_ref()
                        .is_some_and(|error| error.to_lowercase().contains(&query))
            })
            .collect()
    }
    pub fn dns_selected_index(&self) -> usize {
        self.dns_rows()
            .iter()
            .position(|row| {
                self.dns_state
                    .selected
                    .as_ref()
                    .is_some_and(|(resolver, id)| resolver == &row.resolver && *id == row.id)
            })
            .unwrap_or(0)
    }
    pub fn select_dns(&mut self, index: usize) {
        self.dns_state.selected = self
            .dns_rows()
            .get(index)
            .map(|row| (row.resolver.clone(), row.id));
        self.dns_state.detail_scroll = 0;
    }
    pub fn open_dns_detail(&mut self) {
        self.dns_state.detail_record = self
            .dns_rows()
            .get(self.dns_selected_index())
            .map(|row| (*row).clone());
        self.dns_state.detail = self.dns_state.detail_record.is_some();
        self.dns_state.detail_scroll = 0;
    }
    pub fn toggle_dns_pause(&mut self) {
        if self.dns_state.frozen.is_some() {
            self.dns_state.frozen = None;
        } else {
            self.dns_state.frozen = self.dns_live().cloned().map(Box::new);
        }
    }
}

/// A request captures the observed instance before the next UI refresh.
struct PendingQuery {
    instance: String,
    pid: u32,
    boot: String,
    generation: Option<u64>,
    target: String,
    record: String,
    server: String,
}
impl Client {
    pub fn dns_resolver(&self) -> Option<&astra_model::runtime::DnsResolverStatus> {
        self.live()?;
        self.runtime_telemetry.as_ref()?.dns_resolver.as_deref()
    }
    pub fn dns_query_running(&self) -> bool {
        self.dns_state.pending.is_some()
            || self
                .dns_state
                .query
                .as_ref()
                .is_some_and(|view| view.running())
    }
    pub fn dns_query_output(&self) -> Vec<crate::nettools::OutputLine> {
        use crate::nettools::{OutputLine, OutputTone};
        let Some(view) = &self.dns_state.query else {
            return Vec::new();
        };
        // Legacy Agents return answers in successful attempts, without the newer
        // structured DNS events. Keep these visible in a simple query as well.
        let mut rows: Vec<_> = view
            .attempts
            .iter()
            .filter(|attempt| attempt.error.is_none() && !attempt.detail.is_empty())
            .map(|attempt| OutputLine {
                text: attempt.detail.chars().filter(|c| !c.is_control()).collect(),
                tone: OutputTone::Success,
            })
            .collect();
        rows.extend(view.output_rows());
        rows
    }
    pub fn open_dns_query(&mut self) {
        self.dns_state.query_open = true;
        self.dns_state.status_open = false;
        self.dns_state.filtering = false;
        self.dns_state.detail = false;
        self.dns_state.query_field = 0;
        if self.dns_state.target.is_empty() {
            if let Some(row) = self.dns_rows().get(self.dns_selected_index()) {
                self.dns_state.target = row.domain.clone();
            }
        }
    }
    pub fn request_dns_query(&mut self) {
        if self.dns_query_running() {
            return;
        }
        self.dns_state.query_error = None;
        let request = self.prepare_dns_query();
        match request {
            Ok(request) => {
                self.dns_state.pending = Some(request);
                self.dns_state.query_scroll = 0;
            }
            Err(error) => self.dns_state.query_error = Some(error),
        }
    }
    fn prepare_dns_query(&self) -> Result<PendingQuery, String> {
        use crate::i18n::t;
        let target = self.dns_state.target.trim();
        let name = target.strip_suffix('.').unwrap_or(target);
        if name.is_empty()
            || name.len() > 253
            || !name.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            })
        {
            return Err(t("Enter a valid DNS name (for example, example.com)").into());
        }
        let snapshot = self
            .live()
            .ok_or_else(|| t("Connect a profile before querying DNS").to_owned())?;
        let meta = self
            .runtime_telemetry
            .as_ref()
            .ok_or_else(|| t("Current DNS settings unavailable").to_owned())?;
        let stale = self.runtime_received.is_none_or(|at| {
            (at.elapsed().as_millis() as u64).saturating_add(meta.sample_age_ms) > 3000
        }) || !self.runtime_error.is_empty()
            || meta.coverage != astra_model::runtime::Coverage::Complete
            || meta.ingress_paused;
        if stale {
            return Err(t("Wait for fresh runtime data before querying DNS").into());
        }
        let resolver = self
            .dns_resolver()
            .ok_or_else(|| t("Current DNS settings unavailable").to_owned())?;
        if !resolver.enabled {
            return Err(t("DNS is disabled for this profile").into());
        }
        let mut listener = *resolver
            .listen
            .first()
            .ok_or_else(|| t("No DNS listener configured for this profile").to_owned())?;
        if listener.ip().is_unspecified() {
            listener.set_ip(if listener.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        Ok(PendingQuery {
            instance: self.runtime_instance.clone(),
            pid: snapshot.pid,
            boot: meta.host_boot_id.clone(),
            generation: meta.active_generation,
            target: target.into(),
            record: crate::nettools::RECORDS
                [self.dns_state.record % crate::nettools::RECORDS.len()]
            .into(),
            // Executed on the selected Agent, against this core's listener. TCP
            // preserves full answers and the core's cache/Fake-IP/policy behavior.
            server: format!("tcp://{listener}"),
        })
    }
    pub fn poll_dns_query(&mut self, socket: &std::path::Path) -> bool {
        let mut changed = false;
        if let Some(request) = self.dns_state.pending.take() {
            changed = true;
            let current = self.live().is_some_and(|s| s.pid == request.pid)
                && self.runtime_instance == request.instance
                && self.runtime_telemetry.as_ref().is_some_and(|m| {
                    m.host_boot_id == request.boot && m.active_generation == request.generation
                });
            if current {
                let mut view = crate::nettools::View::new(socket, request.instance, &[])
                    .with_waker(self.waker.clone());
                view.kind = crate::nettools::Kind::Dns;
                view.target = request.target;
                view.record = request.record;
                view.server = request.server;
                view.count = 1;
                view.start();
                self.dns_state.query = Some(Box::new(view));
            } else {
                self.dns_state.query_error =
                    Some(crate::i18n::t("DNS settings changed; submit the query again").into());
            }
        }
        if let Some(view) = &mut self.dns_state.query {
            if view.running() {
                changed |= view.poll();
            }
        }
        changed
    }
    pub fn cancel_dns_query(&mut self) {
        self.dns_state.pending = None;
        if let Some(view) = &mut self.dns_state.query {
            view.stop();
        }
    }
}

#[cfg(test)]
#[path = "test/dns_test.rs"]
mod tests;
