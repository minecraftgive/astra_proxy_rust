use super::*;
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "astra-override-checkpoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn rejected_override_checkpoint_restores_exact_previous_file_or_absence() {
    let directory = Directory::new();
    let path = directory.0.join("overrides.json");
    let fault = Arc::new(std::sync::OnceLock::new());
    let next = Overrides {
        allow_lan: Some(true),
        ..Default::default()
    };
    std::fs::write(&path, b"{\n  \"allow_lan\": false\n}\n").unwrap();
    let before = std::fs::read(&path).unwrap();
    drop(OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::remove_file(&path).unwrap();
    drop(OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap());
    assert!(!path.exists());
    assert!(fault.get().is_none());
    let mut checkpoint = OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap();
    checkpoint.commit();
    drop(checkpoint);
    let saved: Overrides = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved.allow_lan, Some(true));
}
#[test]
fn failed_override_rollback_latches_uncertain_runtime() {
    let directory = Directory::new();
    let path = directory.0.join("overrides.json");
    std::fs::write(&path, b"{}").unwrap();
    let fault = Arc::new(std::sync::OnceLock::new());
    let checkpoint =
        OverrideCheckpoint::begin(path.clone(), &Overrides::default(), fault.clone()).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    drop(checkpoint);
    assert!(fault.get().unwrap().contains("outcome unknown"));
    assert!(path.is_dir());
}

#[tokio::test]
async fn runtime_chain_reorders_new_plans_and_rejects_invalid_updates_atomically() {
    let directory = Directory::new();
    let path = directory.0.join("profile.toml");
    let source = r#"standalone=true
standalone_outbound='PROXY'
[tun]
interface='  '
auto_interface=false
routes=['0.0.0.0/1']
[[outbounds]]
kind='socks5'
tag='entry'
server='127.0.0.1:1081'
[[outbounds]]
kind='socks5'
tag='exit'
server='127.0.0.1:1082'
[proxy_groups.PROXY]
members=['entry','exit']
selected='exit'
interface=''
"#;
    std::fs::write(&path, source).unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    let mut app = App::load(&path).unwrap();
    assert!(app.config.tun.as_ref().unwrap().detects_interface());
    assert!(app.config.proxy_groups["PROXY"].interface.is_none());
    app.config.tun = None;
    let mut state = State {
        consistency: Default::default(),
        hosted: None,
        generation: 1,
        path: path.clone(),
        text: source.into(),
        current: Arc::new(ArcSwap::from_pointee(app)),
        listeners: Default::default(),
        system_dns: None,
        tunnel: None,
        tproxy_udp: None,
        dns: None,
        system: Default::default(),
        system_journal: directory.0.join("journal"),
        group_worker: None,
        outbound_interface: None,
        elevated_tun: false,
    };
    let command = |members: &[&str]| Command::SetChain {
        group: "PROXY".into(),
        members: Some(members.iter().map(|s| s.to_string()).collect()),
    };
    let plan = |app: &App| {
        let (_, lease) = app
            .groups
            .plan(app, &astra_model::RouteAction::Proxy("PROXY".into()), true)
            .unwrap();
        lease.3
    };
    let saved_generation = state.current.load_full();
    state
        .runtime_control(command(&["entry", "exit"]))
        .await
        .unwrap();
    let first_generation = state.current.load_full();
    assert_eq!(plan(&first_generation), ["entry", "exit"]);
    state
        .runtime_control(command(&["entry", "exit"]))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&state.current.load_full(), &first_generation));
    state
        .runtime_control(command(&["exit", "entry"]))
        .await
        .unwrap();
    for invalid in [
        vec![],
        vec!["missing"],
        vec!["PROXY"],
        vec!["entry", "entry"],
    ] {
        assert!(state.runtime_control(command(&invalid)).await.is_err());
    }
    state.reload(None).await.unwrap();
    assert_eq!(plan(&state.current.load()), ["exit", "entry"]);
    assert_eq!(plan(&first_generation), ["entry", "exit"]);
    assert_eq!(plan(&saved_generation), ["exit"]);
    state
        .runtime_control(Command::SetChain {
            group: "PROXY".into(),
            members: None,
        })
        .await
        .unwrap();
    assert_eq!(plan(&state.current.load()), ["exit"]);
    assert_eq!(std::fs::read_to_string(path).unwrap(), source);
}
