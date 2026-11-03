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
mod tests {
    use super::*;
    #[tokio::test]
    async fn restart_adopts_verified_boot_and_rejects_changed_boot_without_killing_it() {
        // Control's applied snapshot is process-global. Isolate this fixture
        // from every other manager test instead of racing their control state.
        if std::env::var_os("ASTRA_IDENTITY_FIXTURE_CHILD").is_none() {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "manager::recovery::tests::restart_adopts_verified_boot_and_rejects_changed_boot_without_killing_it", "--nocapture"])
                .env("ASTRA_IDENTITY_FIXTURE_CHILD", "1").kill_on_drop(true).output();
            let output = timeout(Duration::from_secs(30), output)
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("state");
        private_dir(&home).unwrap();
        let config_home = root.path().join("config");
        let runtime = root.path().join("runtime");
        let config = root.path().join("proxy.toml");
        fs::write(&config, "standalone=true\nmixed_listen='127.0.0.1:0'\n").unwrap();
        let config = fs::canonicalize(config).unwrap();
        let mut manager = Manager::load_with_runtime(
            home.clone(),
            config_home.clone(),
            root.path().into(),
            Some(runtime.clone()),
        )
        .unwrap();
        manager.registry.instances.push(Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: config.clone(),
        });
        manager.persist().unwrap();
        let socket = manager.socket("proxy");
        let control = control::Control::bind(Some(socket.clone())).unwrap();
        control::applied(&config, "standalone=true");
        let config2 = config.clone();
        let task = tokio::spawn(async move {
            control
                .run_with_snapshot(
                    |_| async { Ok(()) },
                    move || {
                        let path = config2.clone();
                        async move {
                            Ok(astra_model::control::ConfigSnapshot {
                                api_version: astra_ipc::API_VERSION,
                                pid: std::process::id(),
                                path,
                                text: "standalone=true".into(),
                            })
                        }
                    },
                )
                .await
        });
        let identity = control::identity(&socket).await.unwrap();
        let mut record = Record {
            version: 1,
            name: "proxy".into(),
            config,
            endpoint: socket.clone(),
            identity,
        };
        astra_platform::storage::write_json(&manager.process_record("proxy"), &record).unwrap();
        drop(manager);
        let mut restored =
            Manager::load_with_runtime(home, config_home, root.path().into(), Some(runtime))
                .unwrap();
        restored.recover_processes().await.unwrap();
        assert!(matches!(
            &restored.running["proxy"].child,
            ManagedChild::Reattached(_)
        ));
        // A second, live process record with an unresponsive endpoint must
        // quarantine only that instance and forbid duplicate starts.
        let stalled = restored.socket("stalled");
        let _listener = UnixListener::bind(&stalled).unwrap();
        restored.registry.instances.push(Instance {
            name: "stalled".into(),
            kind: Kind::Proxy,
            config: record.config.clone(),
        });
        let stalled_record = Record {
            version: 1,
            name: "stalled".into(),
            config: record.config.clone(),
            endpoint: stalled,
            identity: record.identity.clone(),
        };
        astra_platform::storage::write_json(&restored.process_record("stalled"), &stalled_record)
            .unwrap();
        restored.recover_processes().await.unwrap();
        assert!(restored.running.contains_key("proxy"));
        assert!(restored.blocked.contains_key("stalled"));
        assert!(restored.instance("stalled").is_err());
        assert!(restored.process_record("stalled").exists());
        record.identity.boot_id = "another-boot".into();
        astra_platform::storage::write_json(&restored.process_record("proxy"), &record).unwrap();
        restored.recover_processes().await.unwrap();
        assert!(restored.blocked["proxy"].contains("boot identity mismatch"));
        assert!(!restored.running.contains_key("proxy"));
        assert!(restored.instance("proxy").is_err());
        assert_eq!(
            control::identity(&socket).await.unwrap().process.pid,
            std::process::id()
        );
        control::request(&socket, Command::Stop).await.unwrap();
        task.await.unwrap().unwrap();
    }
}
