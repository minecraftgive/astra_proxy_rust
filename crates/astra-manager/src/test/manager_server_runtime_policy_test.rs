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
                let _ = work.reply.send(handle(work.action, work.allow_runtime_uac));
            }
        });
        Ok((
            Self {
                hub,
                sender,
                stopped,
                diagnostics: Arc::new(Semaphore::new(1)),
                probes: Default::default(),
            },
            task,
        ))
    }
}

#[tokio::test]
async fn in_process_actor_preserves_typed_errors_and_context() {
    let home = tempfile::tempdir().unwrap();
    let (client, actor) =
        NativeClient::with_test_runtime_actor(home.path().to_path_buf(), |_, _| {
            Err(
                crate::error::message(astra_management::v2::ErrorCode::Conflict, "名称已被使用")
                    .context("native action"),
            )
        })
        .unwrap();
    let error = client
        .call(Action::Start("proxy".into()))
        .await
        .unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        astra_management::v2::ErrorCode::Conflict
    ));
    assert!(format!("{error:#}").contains("名称已被使用"));
    actor.abort();
}
