//! Durable identity records contain no configuration text or credentials.
use super::*;
use astra_model::control::Identity;

#[derive(Serialize, Deserialize)]
struct Record {
    version: u16,
    name: String,
    config: PathBuf,
    endpoint: PathBuf,
    identity: Identity,
}
pub(super) struct Reattached {
    pub identity: Identity,
    pub endpoint: PathBuf,
}
// Permission denied means unobservable, never dead. Adoption additionally
// requires a fresh kernel-peer-bound boot identity handshake.
pub(super) fn possibly_live(
    identity: &astra_model::control::ProcessIdentity,
) -> std::io::Result<bool> {
    match astra_platform::lifecycle::matches_live(identity) {
        #[cfg(target_os = "macos")]
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(true),
        result => result,
    }
}
fn exited() -> std::process::ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }
}
impl Reattached {
    pub fn try_wait(&self) -> std::io::Result<Option<std::process::ExitStatus>> {
        Ok((!possibly_live(&self.identity.process)?).then(exited))
    }
    pub async fn wait(&self) -> std::io::Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
    pub async fn kill(&self) -> std::io::Result<()> {
        control::request(
            &self.endpoint,
            Command::StopIdentified {
                boot_id: self.identity.boot_id.clone(),
                force: true,
            },
        )
        .await?;
        timeout(Duration::from_secs(5), self.wait())
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }
}
impl ManagedChild {
    pub(super) fn detach(&mut self) {
        match self {
            Self::Normal(child) => {
                child.committed = true;
            }
            #[cfg(windows)]
            Self::Elevated(child) => child.detach(),
            _ => {}
        }
    }
}
impl Manager {
    fn process_record(&self, name: &str) -> PathBuf {
        self.home.join("processes").join(format!("{name}.json"))
    }
    pub(super) async fn commit_process(&mut self, name: &str) -> Result<()> {
        let endpoint = self.socket(name);
        let identity = control::identity(&endpoint).await?;
        let run = self.running.get(name).context("core is not running")?;
        anyhow::ensure!(
            Some(identity.process.pid) == run.child.id() && possibly_live(&identity.process)?,
            "Core identity changed during readiness"
        );
        let record = Record {
            version: 1,
            name: name.into(),
            config: fs::canonicalize(&run.config_path)?,
            endpoint,
            identity,
        };
        astra_platform::storage::write_json(&self.process_record(name), &record)?;
        control::request(
            &record.endpoint,
            Command::CommitService {
                boot_id: record.identity.boot_id.clone(),
            },
        )
        .await?;
        self.running.get_mut(name).unwrap().child.detach();
        Ok(())
    }
    pub(super) fn forget_process(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.process_record(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    /// Explicitly abandon an unverified session, never a process owned by this
    /// manager. The OS endpoint must be vacant before its identity is forgotten.
    pub(super) fn replace_session(&mut self, name: &str) -> Result<()> {
        self.registered_instance(name)?;
        anyhow::ensure!(
            !self.running.contains_key(name),
            "Stop the managed instance before replacing its session"
        );
        let socket = self.socket(name);
        astra_platform::platform::force_recover_control_endpoint(&socket).with_context(|| {
            format!(
                "Cannot replace session {name}: its control endpoint is active, inaccessible or not a replaceable marker; stop the previous core and retry. Its identity record was retained"
            )
        })?;
        self.forget_process(name)?;
        self.blocked.remove(name);
        self.exits.remove(name);
        astra_ipc::progress::report("Replace previous session", name.to_owned());
        Ok(())
    }
    pub(super) async fn recover_processes(&mut self) -> Result<()> {
        for instance in self.registry.instances.clone() {
            let name = instance.name.clone();
            self.running.remove(&name);
            self.blocked.remove(&name);
            if let Err(error) = self.recover_process(&instance).await {
                let reason = format!("Unresponsive or unverified core: {error:#}");
                self.exits.insert(name.clone(), reason.clone());
                self.blocked.insert(name, reason);
            }
        }
        self.recover_rule_generations().await?;
        Ok(())
    }
    async fn recover_process(&mut self, instance: &Instance) -> Result<()> {
        let path = self.process_record(&instance.name);
        let bytes = match astra_platform::storage::read_bounded(&path, 64 * 1024) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut record: Record = serde_json::from_slice(&bytes)
            .context("Invalid process identity record; retained for inspection")?;
        anyhow::ensure!(
            record.version == 1
                && record.name == instance.name
                && record.endpoint == self.socket(&instance.name),
            "Process record binding mismatch; retained"
        );
        if !possibly_live(&record.identity.process)? {
            self.forget_process(&instance.name)?;
            self.exits.insert(
                instance.name.clone(),
                "Exited while management was unavailable; see core log".into(),
            );
            return Ok(());
        }
        // Identity uses a fresh connection and binds the returned PID to the
        // kernel's IPC peer. On macOS an ordinary manager cannot query the
        // start time of a root core; authenticated boot identity supplies that
        // missing check without treating denied metadata access as death.
        let identity = control::identity(&record.endpoint)
            .await
            .context("Identity endpoint unavailable; retained without adoption")?;
        anyhow::ensure!(
            identity.boot_id == record.identity.boot_id
                && identity.process == record.identity.process,
            "Core boot identity mismatch; retained without adoption"
        );
        anyhow::ensure!(
            identity
                .applied
                .as_ref()
                .is_some_and(
                    |applied| fs::canonicalize(&applied.path).ok().as_ref() == Some(&record.config)
                ),
            "Core configuration binding changed; retained without adoption"
        );
        let snapshot = control::config_snapshot(&record.endpoint)
            .await
            .context("Loaded configuration snapshot unavailable; retained without adoption")?;
        anyhow::ensure!(snapshot.pid == identity.process.pid
            && fs::canonicalize(&snapshot.path)? == record.config
            && identity.applied.as_ref().is_some_and(|applied|
                control::config_digest(&snapshot.text) == applied.digest),
            "Loaded configuration snapshot does not match core identity");
        let config = snapshot.text;
        record.identity = identity;
        astra_platform::storage::write_json(&path, &record)?;
        self.running.insert(
            instance.name.clone(),
            Running {
                child: ManagedChild::Reattached(Reattached {
                    identity: record.identity,
                    endpoint: record.endpoint,
                }),
                config,
                config_path: record.config,
            },
        );
        self.exits.remove(&instance.name);
        Ok(())
    }
}

#[cfg(test)]
#[path = "../test/manager_recovery_test.rs"]
mod tests;
