use super::*;

impl NativeClient {
    /// Exercise the real Agent -> NativeClient -> Work path without launching
    /// cores, querying SCM, or invoking UAC.
    pub(crate) fn with_test_runtime_actor(
        home: PathBuf,
        mut handle: impl FnMut(Action, bool) -> Result<Reply> + Send + 'static,
    ) -> Result<(Self, tokio::task::JoinHandle<()>)> {
        astra_platform::storage::private_dir(&home.join("state"))?;
        let manager = Manager::load_with_runtime(
            home.join("state"),
            home.join("config"),
            home.join("bin"),
            Some(home.join("runtime")),
        )?;
        let hub = Hub::new(&manager)?;
        let (sender, mut requests) = mpsc::channel::<Work>(8);
        let (stopped, _stop) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            while let Some(work) = requests.recv().await {
                assert!(work.save.is_none());
                let _ = work
                    .reply
                    .send(reply(handle(work.action, work.allow_runtime_uac)));
            }
        });
        Ok((
            Self {
                hub,
                sender,
                stopped,
                diagnostics: Arc::new(Semaphore::new(1)),
            },
            task,
        ))
    }
}
