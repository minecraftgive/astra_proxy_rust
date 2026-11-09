use super::*;

#[tokio::test]
async fn network_state_is_sampled_at_target_agent_without_mutation_lock_or_native_core() {
    let root = tempfile::tempdir().unwrap();
    let agent = AgentService::new(ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        ..Default::default()
    })
    .unwrap();
    let hello = agent
        .handle(Request::new("hello", RequestBody::Query(Query::Hello)))
        .await;
    assert!(
        matches!(hello.result, Ok(ResponseBody::Hello(Hello { capabilities, .. })) if capabilities.contains(&"network_state_v1".into()))
    );
    let _mutation = agent.mutation.lock().await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        agent.handle(Request::new(
            "state",
            RequestBody::Query(Query::NetworkState),
        )),
    )
    .await
    .unwrap();
    let Ok(ResponseBody::NetworkState(snapshot)) = result.result else {
        panic!("Expected OS state")
    };
    assert_eq!(snapshot.agent_id, agent.identity.id);
    assert_eq!(snapshot.schema_version, 1);
    assert!(!snapshot.interfaces.is_empty(), "{:?}", snapshot.issues);
    let mut request = Request::new("wrong-agent", RequestBody::Query(Query::NetworkState));
    request.agent_id = Some("another-agent".into());
    assert!(matches!(
        agent.handle(request).await.result,
        Err(ApiError {
            code: ErrorCode::NotFound,
            ..
        })
    ));
}
