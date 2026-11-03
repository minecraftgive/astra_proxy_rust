//! Durable operation identities and revision-checked drafts, owned by the executing agent.
use anyhow::{Context, Result};
use astra_management::v2::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Store {
    db: Connection,
    event_limit: usize,
}
impl Store {
    pub fn set_event_limit(&mut self, limit: usize) {
        self.event_limit = limit.clamp(64, 100000);
    }
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            matches!(version, 0 | 2),
            "Unsupported management database version {version}; database is retained unchanged"
        );
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, task TEXT NOT NULL, response TEXT);
          CREATE TABLE IF NOT EXISTS drafts(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT, operation_id TEXT, kind TEXT NOT NULL, message TEXT NOT NULL, timestamp_ms TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS servers(name TEXT PRIMARY KEY, config TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS identities(name TEXT PRIMARY KEY,id TEXT NOT NULL UNIQUE);
          CREATE TABLE IF NOT EXISTS controller_agents(id TEXT PRIMARY KEY,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS controller_snapshots(id TEXT PRIMARY KEY,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS controller_routes(operation_id TEXT PRIMARY KEY,agent_id TEXT NOT NULL,request_id TEXT NOT NULL,state TEXT NOT NULL,updated_ms TEXT NOT NULL);
          PRAGMA user_version=2;")?;
        let store = Self {
            db,
            event_limit: 10000,
        };
        let mut query = store
            .db
            .prepare("SELECT id,task FROM operations WHERE response IS NULL")?;
        let pending: Vec<(String, String)> = query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(query);
        for (id, text) in pending {
            let mut task: Task = serde_json::from_str(&text)?;
            task.state = TaskState::OutcomeUnknown;
            task.message = "Agent restarted before recording an outcome; inspect runtime before any new operation".into();
            store.db.execute(
                "UPDATE operations SET task=?2 WHERE id=?1",
                params![id, serde_json::to_string(&task)?],
            )?;
        }
        Ok(store)
    }
    pub fn begin(&self, id: &str, fingerprint: &str) -> Result<Option<ResponseBody>> {
        anyhow::ensure!(!id.is_empty() && id.len() <= 160, "invalid operation ID");
        let previous: Option<(String, String, Option<String>)> = self
            .db
            .query_row(
                "SELECT fingerprint,task,response FROM operations WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((previous, task, response)) = previous {
            anyhow::ensure!(
                previous == fingerprint,
                "Conflict: operation ID already identifies different input"
            );
            return Ok(Some(match response {
                Some(text) => serde_json::from_str(&text)?,
                None => ResponseBody::Task(serde_json::from_str(&task)?),
            }));
        }
        let task = Task {
            id: id.into(),
            state: TaskState::Running,
            message: "Executing".into(),
            started_ms: now(),
            finished_ms: None,
        };
        self.db.execute(
            "INSERT INTO operations(id,fingerprint,task) VALUES(?1,?2,?3)",
            params![id, fingerprint, serde_json::to_string(&task)?],
        )?;
        self.event(Some(id), "operation_started", "Operation started")?;
        Ok(None)
    }
    pub fn finish(
        &self,
        id: &str,
        result: Option<&ResponseBody>,
        error: Option<&str>,
    ) -> Result<()> {
        let mut task = self.task(id)?;
        task.state =
            if error.is_some_and(|e| e.contains("outcome unknown") || e.contains("not replayed")) {
                TaskState::OutcomeUnknown
            } else if error.is_some() {
                TaskState::Failed
            } else {
                TaskState::Complete
            };
        task.message = error.map(str::to_owned).unwrap_or_else(|| match result {
            Some(ResponseBody::Draft(draft)) => draft
                .last_save
                .as_ref()
                .map(|save| save.message.clone())
                .unwrap_or_else(|| "Completed".into()),
            _ => "Completed".into(),
        });
        if let Some(ResponseBody::UpdateStatus(status)) = result {
            if let Some(operation) = &status.operation {
                use astra_management::updates::UpdatePhase;
                task.state = match operation.phase {
                    UpdatePhase::Failed => TaskState::Failed,
                    UpdatePhase::NeedsRecovery => TaskState::NeedsRecovery,
                    UpdatePhase::Uncertain => TaskState::OutcomeUnknown,
                    _ => task.state,
                };
                task.message = operation
                    .error
                    .clone()
                    .unwrap_or_else(|| operation.phase.label().into());
            }
        }
        task.finished_ms = Some(now());
        // Failed operations also record a terminal result, so identical IDs never execute again.
        let response = result
            .cloned()
            .unwrap_or_else(|| ResponseBody::Task(task.clone()));
        self.db.execute(
            "UPDATE operations SET task=?2,response=?3 WHERE id=?1",
            params![
                id,
                serde_json::to_string(&task)?,
                serde_json::to_string(&response)?
            ],
        )?;
        self.event(
            Some(id),
            if error.is_some()
                || matches!(
                    task.state,
                    TaskState::Failed | TaskState::NeedsRecovery | TaskState::OutcomeUnknown
                )
            {
                "operation_failed"
            } else {
                "operation_complete"
            },
            &task.message,
        )?;
        Ok(())
    }
    pub fn task(&self, id: &str) -> Result<Task> {
        let value: String = self
            .db
            .query_row("SELECT task FROM operations WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .context("operation not found")?;
        Ok(serde_json::from_str(&value)?)
    }
    pub fn identity(&self, name: &str) -> Result<String> {
        self.db.execute(
            "INSERT OR IGNORE INTO identities(name,id) VALUES(?1,?2)",
            params![name, uuid::Uuid::new_v4().to_string()],
        )?;
        Ok(self
            .db
            .query_row("SELECT id FROM identities WHERE name=?1", [name], |r| {
                r.get(0)
            })?)
    }
    pub fn resolve(&self, reference: &str) -> Result<String> {
        Ok(self
            .db
            .query_row(
                "SELECT name FROM identities WHERE id=?1 OR name=?1",
                [reference],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| reference.to_owned()))
    }
    pub fn forget_identity(&self, name: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM identities WHERE name=?1", [name])?;
        Ok(())
    }
    pub fn tasks(&self, limit: u16) -> Result<Vec<Task>> {
        let mut query = self
            .db
            .prepare("SELECT task FROM operations ORDER BY rowid DESC LIMIT ?1")?;
        let rows = query
            .query_map([limit.clamp(1, 256)], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|text| Ok(serde_json::from_str(&text)?))
            .collect()
    }
    pub fn event(&self, id: Option<&str>, kind: &str, message: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO events(operation_id,kind,message,timestamp_ms) VALUES(?1,?2,?3,?4)",
            params![id, kind, message, now()],
        )?;
        self.db.execute(
            "DELETE FROM events WHERE sequence < (SELECT COALESCE(MAX(sequence),0)-?1 FROM events)",
            [self.event_limit as i64],
        )?;
        Ok(())
    }
    pub fn events(&self, after: Option<&str>, limit: u16) -> Result<EventBatch> {
        let after = after
            .unwrap_or("0")
            .parse::<i64>()
            .context("invalid event cursor")?;
        let first: i64 =
            self.db
                .query_row("SELECT COALESCE(MIN(sequence),0) FROM events", [], |r| {
                    r.get(0)
                })?;
        let mut query=self.db.prepare("SELECT sequence,operation_id,kind,message,timestamp_ms FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
        let mut events = query
            .query_map(params![after, limit.clamp(1, 256)], |r| {
                Ok(Event {
                    sequence: r.get::<_, i64>(0)?.to_string(),
                    operation_id: r.get(1)?,
                    kind: r.get(2)?,
                    message: r.get(3)?,
                    payload: None,
                    timestamp_ms: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // SQLite retains the original record format; the public contract carries
        // structured event data rather than JSON embedded inside a text field.
        for event in &mut events {
            event.payload = match event.kind.as_str() {
                "diagnostics_event" => Some(EventPayload::Diagnostics(serde_json::from_str(
                    &event.message,
                )?)),
                "nettool_event" => {
                    Some(EventPayload::NetTool(serde_json::from_str(&event.message)?))
                }
                _ => None,
            };
            if event.payload.is_some() {
                event.message = if event.kind == "diagnostics_event" {
                    "Diagnostic progress"
                } else {
                    "Network probe progress"
                }
                .into();
            }
        }
        let cursor = events
            .last()
            .map_or_else(|| after.to_string(), |e| e.sequence.clone());
        Ok(EventBatch {
            events,
            cursor,
            resync_required: after > 0 && first > after + 1,
        })
    }
    pub fn draft(&self, id: &str) -> Result<Draft> {
        let text: String = self
            .db
            .query_row("SELECT body FROM drafts WHERE id=?1", [id], |r| r.get(0))
            .context("draft not found")?;
        Ok(serde_json::from_str(&text)?)
    }
    pub fn open_draft(&self, id: String, doc: Document) -> Result<Draft> {
        anyhow::ensure!(!id.is_empty() && id.len() <= 160, "invalid draft ID");
        let previous: Option<String> = self
            .db
            .query_row("SELECT body FROM drafts WHERE id=?1", [&id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(text) = previous {
            let existing: Draft = serde_json::from_str(&text)
                .context("Saved draft cannot be read; original record is retained")?;
            anyhow::ensure!(
                existing.instance == doc.instance && existing.path == doc.path,
                "Conflict: draft belongs to another document"
            );
            return Ok(existing);
        }
        let draft = Draft {
            last_save: None,
            id,
            instance: doc.instance,
            path: doc.path,
            original: doc.text.clone(),
            text: doc.text,
            revision: "0".into(),
            source_revision: doc.revision,
            kind: doc.kind,
        };
        self.db.execute(
            "INSERT INTO drafts(id,revision,body) VALUES(?1,0,?2)",
            params![draft.id, serde_json::to_string(&draft)?],
        )?;
        Ok(draft)
    }
    pub fn replace_draft(&self, mut draft: Draft, expected: &str) -> Result<Draft> {
        let previous = expected.parse::<i64>().context("invalid draft revision")?;
        draft.revision = previous
            .checked_add(1)
            .context("draft revision overflow")?
            .to_string();
        let changed = self.db.execute(
            "UPDATE drafts SET revision=?2,body=?3 WHERE id=?1 AND revision=?4",
            params![
                draft.id,
                draft.revision.parse::<i64>()?,
                serde_json::to_string(&draft)?,
                previous
            ],
        )?;
        anyhow::ensure!(changed == 1, "Conflict: draft changed in another frontend");
        Ok(draft)
    }
    pub fn discard(&self, id: &str, revision: &str) -> Result<()> {
        let count = self.db.execute(
            "DELETE FROM drafts WHERE id=?1 AND revision=?2",
            params![id, revision.parse::<i64>()?],
        )?;
        anyhow::ensure!(count == 1, "Conflict: draft changed or was already removed");
        Ok(())
    }
    pub fn servers(&self) -> Result<Vec<(String, std::path::PathBuf)>> {
        let mut query = self
            .db
            .prepare("SELECT name,config FROM servers ORDER BY name")?;
        let rows = query
            .query_map([], |r| {
                Ok((r.get(0)?, std::path::PathBuf::from(r.get::<_, String>(1)?)))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
    pub fn add_server(&self, name: &str, path: &Path) -> Result<()> {
        self.db.execute(
            "INSERT INTO servers(name,config) VALUES(?1,?2)",
            params![name, path.to_string_lossy()],
        )?;
        Ok(())
    }
    pub fn remove_server(&self, name: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM servers WHERE name=?1", [name])?;
        Ok(())
    }
    pub fn set_server_config(&self, name: &str, path: &Path) -> Result<()> {
        anyhow::ensure!(
            self.db.execute(
                "UPDATE servers SET config=?2 WHERE name=?1",
                params![name, path.to_string_lossy()]
            )? == 1,
            "server not found"
        );
        Ok(())
    }
    pub fn remember_agent(&self, agent: Agent) -> Result<()> {
        self.db.execute("INSERT INTO controller_agents(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![agent.id,serde_json::to_string(&agent)?])?;
        Ok(())
    }
    pub fn snapshot(&self, id: &str, body: &str) -> Result<()> {
        self.db.execute("INSERT INTO controller_snapshots(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,body])?;
        Ok(())
    }
    pub fn remembered_agents(&self) -> Result<Vec<Agent>> {
        let mut statement = self
            .db
            .prepare("SELECT body FROM controller_agents ORDER BY id")?;
        let texts = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        texts
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub fn route(&self, operation: &str, agent: &str, request: &str, state: &str) -> Result<()> {
        let prior: Option<String> = self
            .db
            .query_row(
                "SELECT agent_id FROM controller_routes WHERE operation_id=?1",
                [operation],
                |row| row.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            prior.as_deref().is_none_or(|owner| owner == agent),
            "Conflict: operation ID already belongs to another agent"
        );
        self.db.execute("INSERT INTO controller_routes(operation_id,agent_id,request_id,state,updated_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(operation_id) DO UPDATE SET request_id=excluded.request_id,state=excluded.state,updated_ms=excluded.updated_ms",params![operation,agent,request,state,now()])?;
        Ok(())
    }
}
pub fn now() -> String {
    astra_ipc::progress::now_ms().to_string()
}
pub fn revision(text: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn memory() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }
    #[test]
    fn failed_or_uncertain_updates_never_become_successful_tasks() {
        use astra_management::updates::{
            UpdateIntent, UpdateOperation, UpdatePhase, UpdateRequest, UpdateStatus,
        };
        for (phase, expected) in [
            (UpdatePhase::Failed, "failed"),
            (UpdatePhase::NeedsRecovery, "needs_recovery"),
            (UpdatePhase::Uncertain, "outcome_unknown"),
        ] {
            let store = memory();
            store.begin("update", "fingerprint").unwrap();
            let status = UpdateStatus {
                operation: Some(UpdateOperation {
                    request: UpdateRequest {
                        operation_id: "update".into(),
                        intent: UpdateIntent::Stage {
                            package: "package.tar.gz".into(),
                        },
                    },
                    phase,
                    before_directory: "before".into(),
                    target_directory: None,
                    release_id: None,
                    runtime_committed: false,
                    next_launch_saved: false,
                    pending_record_saved: false,
                    participants: vec![],
                    uncertain_preparations: vec![],
                    error: Some("verification failed".into()),
                    updated_at_ms: 1,
                }),
                ..Default::default()
            };
            store
                .finish(
                    "update",
                    Some(&ResponseBody::UpdateStatus(Box::new(status))),
                    None,
                )
                .unwrap();
            let task = serde_json::to_value(store.task("update").unwrap()).unwrap();
            assert_eq!(task["state"], expected);
            assert_eq!(task["message"], "verification failed");
            assert_eq!(
                store.events(None, 10).unwrap().events.last().unwrap().kind,
                "operation_failed"
            );
        }
    }
    #[test]
    fn progress_events_expose_objects_instead_of_nested_json_strings() {
        let store = memory();
        store
            .event(
                Some("operation"),
                "nettool_event",
                r#"{"Ping":{"sequence":"9007199254740993"}}"#,
            )
            .unwrap();
        let events = store.events(None, 10).unwrap();
        let event = serde_json::to_value(&events.events[0]).unwrap();
        assert_eq!(event["payload"]["type"], "net_tool");
        assert_eq!(
            event["payload"]["data"]["Ping"]["sequence"],
            "9007199254740993"
        );
        assert_eq!(event["message"], "Network probe progress");
    }
    #[test]
    fn operation_route_cannot_change_agents() {
        let store = memory();
        store.route("op", "a", "request1", "forwarding").unwrap();
        assert!(store
            .route("op", "b", "request2", "forwarding")
            .unwrap_err()
            .to_string()
            .starts_with("Conflict:"));
        let agent: String = store
            .db
            .query_row(
                "SELECT agent_id FROM controller_routes WHERE operation_id='op'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent, "a");
        store.route("op", "a", "request3", "responded").unwrap();
    }
    #[test]
    fn operations_never_reexecute_and_reject_changed_identity() {
        let s = memory();
        assert!(s.begin("op", "a").unwrap().is_none());
        assert!(matches!(
            s.begin("op", "a").unwrap(),
            Some(ResponseBody::Task(_))
        ));
        assert!(s.begin("op", "b").is_err());
        s.finish(
            "op",
            Some(&ResponseBody::Done {
                message: "done".into(),
            }),
            None,
        )
        .unwrap();
        assert!(matches!(
            s.begin("op", "a").unwrap(),
            Some(ResponseBody::Done { .. })
        ));
    }
    #[test]
    fn stale_frontend_cannot_replace_or_discard_newer_draft() {
        let s = memory();
        let d = s
            .open_draft(
                "d".into(),
                Document {
                    instance: "i".into(),
                    path: "a.toml".into(),
                    text: "a=1".into(),
                    revision: "base".into(),
                    kind: InstanceKind::Proxy,
                },
            )
            .unwrap();
        let mut newer = d.clone();
        newer.text = "a=2".into();
        s.replace_draft(newer, "0").unwrap();
        assert!(s.replace_draft(d, "0").is_err());
        assert!(s.discard("d", "0").is_err());
        assert_eq!(s.draft("d").unwrap().text, "a=2");
    }
    #[test]
    fn malformed_or_future_drafts_are_rejected_without_replacement() {
        let store = memory();
        let document = Document {
            instance: "i".into(),
            path: "a.toml".into(),
            text: "a=1".into(),
            revision: "base".into(),
            kind: InstanceKind::Proxy,
        };
        let draft = store.open_draft("d".into(), document.clone()).unwrap();
        let mut future = serde_json::to_value(draft).unwrap();
        future["future_field"] = serde_json::json!(true);
        for original in [
            "{broken".to_owned(),
            serde_json::to_string(&future).unwrap(),
        ] {
            store
                .db
                .execute("UPDATE drafts SET body=?1 WHERE id='d'", [&original])
                .unwrap();
            assert!(store.open_draft("d".into(), document.clone()).is_err());
            let retained: String = store
                .db
                .query_row("SELECT body FROM drafts WHERE id='d'", [], |row| row.get(0))
                .unwrap();
            assert_eq!(retained, original);
        }
    }
    #[test]
    fn future_database_version_is_rejected_before_schema_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("management.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA user_version=99; CREATE TABLE future_state(value TEXT); INSERT INTO future_state VALUES('retain');").unwrap();
        drop(db);
        let original = std::fs::read(&path).unwrap();
        assert!(Store::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
}
