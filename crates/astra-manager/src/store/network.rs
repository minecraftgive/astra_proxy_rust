//! Evidence has a bounded lifetime; operation identities never expire.
use super::*;

impl Store {
    pub(super) fn initialize_network(&self) -> Result<()> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS network_reports (
          id TEXT PRIMARY KEY REFERENCES operations(id), body TEXT NOT NULL,
          bytes INTEGER NOT NULL DEFAULT 0, terminal INTEGER NOT NULL DEFAULT 0,
          expired INTEGER NOT NULL DEFAULT 0, event_count INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS network_items (
          operation_id TEXT NOT NULL REFERENCES network_reports(id), sequence INTEGER NOT NULL,
          body TEXT NOT NULL, PRIMARY KEY(operation_id,sequence));
          CREATE TABLE IF NOT EXISTS event_retention (id INTEGER PRIMARY KEY, floor INTEGER NOT NULL);
          INSERT OR IGNORE INTO event_retention VALUES(1,0);")?;
        let mut query = self
            .db
            .prepare("SELECT id FROM network_reports WHERE terminal=0")?;
        let ids = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(query);
        for id in ids {
            self.network_state(
                &id,
                NetworkState::Interrupted,
                "Agent restarted; diagnostic interrupted and will not be replayed",
            )?;
        }
        self.prune_network()
    }

    pub fn network_terminal(&self, id: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT terminal FROM network_reports WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn network_existing(&self, id: &str, fingerprint: &str) -> Result<Option<Task>> {
        operation_id(id)?;
        let prior: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT fingerprint,task FROM operations WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        prior
            .map(|(saved, task)| {
                anyhow::ensure!(
                    saved == fingerprint,
                    crate::error::message(
                        ErrorCode::Conflict,
                        "operation ID already identifies different input"
                    )
                );
                Ok(serde_json::from_str(&task)?)
            })
            .transpose()
    }

    pub fn begin_network(&self, id: &str, fingerprint: &str) -> Result<Task> {
        operation_id(id)?;
        let tx = self.db.unchecked_transaction()?;
        let task = Task {
            id: id.into(),
            state: TaskState::Running,
            message: "Diagnostic queued".into(),
            started_ms: now(),
            finished_ms: None,
        };
        let report = NetworkReport {
            operation_id: id.into(),
            schema_version: 1,
            revision: "0".into(),
            state: NetworkState::Queued,
            message: task.message.clone(),
            started_ms: task.started_ms.clone(),
            finished_ms: None,
            truncated: false,
            expired: false,
            items: vec![],
            cursor: "0".into(),
            has_more: false,
        };
        self.db.execute(
            "INSERT INTO operations(id,fingerprint,task) VALUES(?1,?2,?3)",
            params![id, fingerprint, serde_json::to_string(&task)?],
        )?;
        self.db.execute(
            "INSERT INTO network_reports(id,body,updated) VALUES(?1,?2,?3)",
            params![id, serde_json::to_string(&report)?, now().parse::<i64>()?],
        )?;
        self.network_event(&report, None)?;
        self.prune_network_inner()?;
        tx.commit()?;
        Ok(task)
    }

    fn network_header(&self, id: &str) -> Result<NetworkReport> {
        let body: String = self
            .db
            .query_row("SELECT body FROM network_reports WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .map_err(|error| missing_row(error, "network report not found"))?;
        Ok(serde_json::from_str(&body)?)
    }

    fn save_network_header(&self, report: &NetworkReport) -> Result<()> {
        self.db.execute(
            "UPDATE network_reports SET body=?2,terminal=?3,updated=?4,expired=?5 WHERE id=?1",
            params![
                report.operation_id,
                serde_json::to_string(report)?,
                report.state.terminal(),
                now().parse::<i64>()?,
                report.expired
            ],
        )?;
        Ok(())
    }

    fn network_event(&self, report: &NetworkReport, item: Option<&str>) -> Result<()> {
        let count: usize = self.db.query_row(
            "SELECT event_count FROM network_reports WHERE id=?1",
            [&report.operation_id],
            |r| r.get(0),
        )?;
        // Reserve the final slot for the terminal outcome. No response content in events.
        if count >= NETWORK_EVENT_COUNT - usize::from(!report.state.terminal()) {
            return Ok(());
        }
        let message = serde_json::to_string(&serde_json::json!({
            "stage": report.state, "report_revision": report.revision, "result_id": item
        }))?;
        let event = Event {
            sequence: i64::MAX.to_string(),
            operation_id: Some(report.operation_id.clone()),
            kind: "network_progress".into(),
            message: message.clone(),
            payload: None,
            timestamp_ms: now(),
        };
        anyhow::ensure!(
            serde_json::to_vec(&event)?.len() <= NETWORK_EVENT_BYTES,
            "Diagnostic event exceeds budget"
        );
        self.event(Some(&report.operation_id), "network_progress", &message)?;
        self.db.execute(
            "UPDATE network_reports SET event_count=event_count+1 WHERE id=?1",
            [&report.operation_id],
        )?;
        Ok(())
    }

    pub fn append_network(&self, id: &str, data: serde_json::Value) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        let mut report = self.network_header(id)?;
        anyhow::ensure!(
            !report.state.terminal(),
            crate::error::message(ErrorCode::Conflict, "Diagnostic already finished")
        );
        if report.truncated {
            return Ok(());
        }
        let sequence = report.cursor.parse::<u64>()? + 1;
        let item = NetworkResult {
            id: sequence.to_string(),
            sampled_at_ms: now(),
            data,
        };
        let body = serde_json::to_string(&item)?;
        let used: usize =
            self.db
                .query_row("SELECT bytes FROM network_reports WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
        // Keep room for metadata and a response envelope, and never split a result item.
        if body.len() + 1 > NETWORK_PAGE_BYTES - 4096
            || used + body.len() > NETWORK_REPORT_BYTES - 4096
        {
            report.truncated = true;
        } else {
            self.db.execute(
                "INSERT INTO network_items VALUES(?1,?2,?3)",
                params![id, sequence, body],
            )?;
            self.db.execute(
                "UPDATE network_reports SET bytes=bytes+?2 WHERE id=?1",
                params![id, body.len()],
            )?;
            report.cursor = sequence.to_string();
        }
        report.revision = (report.revision.parse::<u64>()? + 1).to_string();
        self.save_network_header(&report)?;
        self.network_event(
            &report,
            if report.truncated {
                None
            } else {
                Some(&item.id)
            },
        )?;
        self.prune_network_inner()?;
        tx.commit()?;
        Ok(())
    }

    pub fn network_truncated(&self, id: &str) -> Result<()> {
        let mut report = self.network_header(id)?;
        report.truncated = true;
        report.revision = (report.revision.parse::<u64>()? + 1).to_string();
        self.save_network_header(&report)
    }

    pub fn network_state(&self, id: &str, state: NetworkState, message: &str) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        let mut report = self.network_header(id)?;
        anyhow::ensure!(
            !report.state.terminal(),
            crate::error::message(ErrorCode::Conflict, "Diagnostic already finished")
        );
        report.state = if state == NetworkState::Complete && report.truncated {
            NetworkState::Partial
        } else {
            state
        };
        report.message = message.chars().take(512).collect();
        if report.truncated {
            report.message.push_str("; evidence truncated");
        }
        report.revision = (report.revision.parse::<u64>()? + 1).to_string();
        if state.terminal() {
            report.finished_ms = Some(now());
        }
        self.save_network_header(&report)?;
        let mut task = self.task(id)?;
        task.state = match report.state {
            NetworkState::Queued | NetworkState::Running => TaskState::Running,
            NetworkState::Complete => TaskState::Complete,
            NetworkState::Interrupted => TaskState::OutcomeUnknown,
            _ => TaskState::Failed,
        };
        task.message = report.message.clone();
        task.finished_ms = report.finished_ms.clone();
        self.db.execute(
            "UPDATE operations SET task=?2,response=?3 WHERE id=?1",
            params![
                id,
                serde_json::to_string(&task)?,
                if state.terminal() {
                    Some(serde_json::to_string(&ResponseBody::Task(task))?)
                } else {
                    None
                }
            ],
        )?;
        self.network_event(&report, None)?;
        self.prune_network_inner()?;
        tx.commit()?;
        Ok(())
    }

    pub fn network_report(&self, id: &str, cursor: Option<&str>) -> Result<NetworkReport> {
        self.prune_network()?;
        let mut report = self.network_header(id)?;
        let cursor = cursor
            .unwrap_or("0")
            .parse::<u64>()
            .map_err(|error| crate::error::classified(ErrorCode::InvalidRequest, error.into()))
            .context("invalid network report cursor")?;
        anyhow::ensure!(
            cursor <= report.cursor.parse::<u64>()?,
            crate::error::message(
                ErrorCode::InvalidRequest,
                "network report cursor is ahead of the report"
            )
        );
        if report.expired {
            return Ok(report);
        }
        let last = report.cursor.clone();
        report.cursor = cursor.to_string();
        let mut query = self.db.prepare("SELECT body FROM network_items WHERE operation_id=?1 AND sequence>?2 ORDER BY sequence")?;
        let mut rows = query.query(params![id, cursor])?;
        let mut used = 4096;
        while let Some(row) = rows.next()? {
            let body: String = row.get(0)?;
            if used + body.len() + 1 > NETWORK_PAGE_BYTES {
                break;
            }
            used += body.len() + 1;
            let item: NetworkResult = serde_json::from_str(&body)?;
            report.cursor = item.id.clone();
            report.items.push(item);
        }
        report.has_more = report.cursor != last;
        Ok(report)
    }

    pub fn prune_network(&self) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        self.prune_network_inner()?;
        tx.commit()?;
        Ok(())
    }

    fn prune_network_inner(&self) -> Result<()> {
        let cutoff = astra_ipc::progress::now_ms().saturating_sub(NETWORK_RETENTION_MS);
        let (mut count, mut bytes): (usize, usize) = self.db.query_row(
            "SELECT COUNT(*),COALESCE(SUM(bytes + length(CAST(body AS BLOB))),0) FROM network_reports WHERE expired=0", [], |r| Ok((r.get(0)?,r.get(1)?)))?;
        let mut query = self.db.prepare("SELECT id,bytes+length(CAST(body AS BLOB)),updated FROM network_reports WHERE terminal=1 AND expired=0 ORDER BY updated,id")?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, usize>(1)?,
                    r.get::<_, u64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(query);
        // Include associated events in the aggregate evidence budget.
        let event_bytes: usize = self.db.query_row("SELECT COALESCE(SUM(length(CAST(message AS BLOB))+length(CAST(kind AS BLOB))+length(CAST(operation_id AS BLOB))+128),0) FROM events WHERE operation_id IN (SELECT id FROM network_reports)", [], |r| r.get(0))?;
        bytes += event_bytes;
        for (id, size, updated) in rows {
            if updated >= cutoff
                && count <= NETWORK_RETENTION_COUNT
                && bytes <= NETWORK_RETENTION_BYTES
            {
                continue;
            }
            let mut report = self.network_header(&id)?;
            report.expired = true;
            report.revision = (report.revision.parse::<u64>()? + 1).to_string();
            self.save_network_header(&report)?;
            self.db
                .execute("DELETE FROM network_items WHERE operation_id=?1", [&id])?;
            self.db
                .execute("UPDATE network_reports SET bytes=0 WHERE id=?1", [&id])?;
            self.db.execute("UPDATE event_retention SET floor=MAX(floor,COALESCE((SELECT MAX(sequence) FROM events WHERE operation_id=?1),0)) WHERE id=1", [&id])?;
            let removed: usize = self.db.query_row("SELECT COALESCE(SUM(length(CAST(message AS BLOB))+length(CAST(kind AS BLOB))+length(CAST(operation_id AS BLOB))+128),0) FROM events WHERE operation_id=?1", [&id], |r|r.get(0))?;
            self.db
                .execute("DELETE FROM events WHERE operation_id=?1", [&id])?;
            count -= 1;
            bytes = bytes.saturating_sub(size + removed);
        }
        Ok(())
    }
}
